//! Native-owned volatile observation tails, independent of plugin polling.
use super::*;
use serde::Deserialize;

const RETAINED_CHILDREN: usize = 32;
const JOURNAL_EVENTS: usize = 256;
const JOURNAL_BYTES: usize = 1024 * 1024;
const EVENT_BYTES: usize = 32768;
const PAGE_BYTES: usize = 131072;

pub(crate) fn capabilities() -> Value {
    json!({"version":1,"listCommand":"get_child_agents",
        "eventsCommand":"get_child_agent_events","cancelCommand":"cancel_child_agent",
        "retention":"volatile","maxChildren":RETAINED_CHILDREN,"maxEvents":JOURNAL_EVENTS,
        "maxEventBytes":EVENT_BYTES,"maxPageCount":32,"maxResponseBytes":PAGE_BYTES,
        "sequence":"childEpochSeq-v1"})
}

pub(super) struct Observations {
    epoch: String,
    children: VecDeque<(String, Arc<Job>)>,
}
impl Default for Observations {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            children: VecDeque::new(),
        }
    }
}
impl Observations {
    pub(super) fn remember(&mut self, id: String, job: Arc<Job>) {
        if self.children.len() >= RETAINED_CHILDREN {
            // Active runs retain observation ownership. MAX_JOBS < RETAINED_CHILDREN.
            if let Some(index) = self.children.iter().position(|(_, job)| {
                let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                state.finished_ms.is_some()
            }) {
                self.children.remove(index);
            } else {
                return;
            }
        }
        self.children.push_back((id, job));
    }
}

#[derive(Default)]
pub(super) struct Journal {
    seq: u64,
    bytes: usize,
    entries: VecDeque<(u64, Value, usize)>,
}
impl Journal {
    pub(super) fn observe(&mut self, event: &AgentEvent) {
        let encoded =
            crate::rpc::events::encode(event, crate::args::RpcMessageUpdates::CompactDeltas);
        let value = match encoded {
            Ok(bytes) if bytes.len() <= EVENT_BYTES => serde_json::from_slice(&bytes).ok(),
            _ => None,
        }
        .unwrap_or_else(|| json!({"type":"observation_gap","reason":"event_too_large"}));
        self.push(value);
    }
    pub(super) fn push(&mut self, event: Value) {
        let bytes = serde_json::to_vec(&event).map_or(EVENT_BYTES, |v| v.len());
        self.seq += 1;
        self.bytes += bytes;
        self.entries.push_back((self.seq, event, bytes));
        while self.entries.len() > JOURNAL_EVENTS || self.bytes > JOURNAL_BYTES {
            if let Some((_, _, bytes)) = self.entries.pop_front() {
                self.bytes -= bytes;
            }
        }
    }
    fn earliest(&self) -> u64 {
        self.entries.front().map_or(self.seq + 1, |e| e.0)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Query {
    #[serde(rename = "type")]
    command: String,
    id: Option<Value>,
    version: u32,
    parent_session_id: String,
    epoch: Option<String>,
    child_id: Option<String>,
    after_seq: Option<u64>,
    limit: Option<usize>,
}
fn status(state: &State) -> &'static str {
    if state.error.is_some() {
        "failed"
    } else if state.result.is_some() {
        "done"
    } else {
        "running"
    }
}
impl ChildAgents {
    pub(crate) fn query(&self, parent: &str, args: Value) -> Result<Value, String> {
        let query: Query =
            serde_json::from_value(args).map_err(|_| "invalid child observation query")?;
        let _ = query.id; // Only the dispatcher echoes correlation IDs.
        if query.version != 1 || query.parent_session_id != parent {
            return Err("child observation parent/version mismatch".into());
        }
        let observations = self.observations.lock().unwrap_or_else(|e| e.into_inner());
        if query
            .epoch
            .as_ref()
            .is_some_and(|epoch| epoch != &observations.epoch)
        {
            return Err("child observation epoch changed; list again".into());
        }
        let epoch = observations.epoch.clone();
        let children: Vec<_> = observations.children.iter().cloned().collect();
        drop(observations);
        if query.command == "get_child_agents" {
            if query.child_id.is_some() || query.after_seq.is_some() || query.limit.is_some() {
                return Err("unexpected child list query fields".into());
            }
            let rows: Vec<_> = children
                .iter()
                .rev()
                .filter_map(|(id, job)| {
                    let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                    (state.parent_session_id == parent).then(|| {
                        json!({"childId":id,
                    "status":status(&state),"turns":state.turns,"role":state.role,
                    "taskPreview":state.task_preview,"startedMs":state.started_ms,
                    "finishedMs":state.finished_ms,"earliestSeq":state.journal.earliest(),
                    "lastSeq":state.journal.seq})
                    })
                })
                .collect();
            return Ok(json!({"version":1,"parentSessionId":parent,"epoch":epoch,
                "retention":"volatile","children":rows}));
        }
        if query.epoch.is_none() {
            return Err("child query requires the roster epoch".into());
        }
        let id = query.child_id.ok_or("child query requires childId")?;
        let job = children
            .into_iter()
            .find(|(child, job)| {
                child == &id
                    && job
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .parent_session_id
                        == parent
            })
            .map(|(_, job)| job)
            .ok_or("child unavailable or evicted in this parent session")?;
        if query.command == "cancel_child_agent" {
            if query.after_seq.is_some() || query.limit.is_some() {
                return Err("unexpected child cancel fields".into());
            }
            let finished = {
                let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                state.finished_ms.is_some()
            };
            job.fail("child cancelled by user");
            return Ok(json!({"version":1,"parentSessionId":parent,"epoch":epoch,
                "childId":id,"status":if finished {"already-finished"} else {"cancelled"}}));
        }
        if query.command != "get_child_agent_events" {
            return Err("unknown child observation command".into());
        }
        let after = query.after_seq.ok_or("child events require afterSeq")?;
        let limit = query.limit.ok_or("child events require limit")?;
        if !(1..=32).contains(&limit) {
            return Err("child event limit must be 1-32".into());
        }
        let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
        if after > state.journal.seq {
            return Err("child cursor is ahead of native observations".into());
        }
        let mut events = Vec::new();
        let mut bytes = 1024; // Bounded envelope and per-event sequence overhead.
        let mut next = after;
        for (seq, event, size) in &state.journal.entries {
            if *seq <= after {
                continue;
            }
            if events.len() >= limit || bytes + size + 64 > PAGE_BYTES {
                break;
            }
            events.push(json!({"seq":seq,"event":event}));
            bytes += size + 64;
            next = *seq;
        }
        Ok(
            json!({"version":1,"parentSessionId":parent,"epoch":epoch,"childId":id,
            "retention":"volatile","earliestSeq":state.journal.earliest(),"lastSeq":state.journal.seq,
            "afterSeq":after,"nextSeq":next,"hasMore":next < state.journal.seq,
            "resyncRequired":after.saturating_add(1) < state.journal.earliest(),"events":events}),
        )
    }
}
