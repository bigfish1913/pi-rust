//! Observe the child's canonical native journal. This never executes tools,
//! opens a session, changes settlement or invents a transcript. The child runner
//! owns the commit receiver and drains it after native cleanup.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use rpi_harness::{
    session::session::{NativeJournalCommit, SessionEntryCommit},
    tool_program_body::BodyDescriptor,
    tool_program_queries::{read_child_body_page, ChildBodyQuery},
};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

const MAX_BODY_BYTES: usize = 256 * 1024;
const FRAGMENT_BYTES: usize = 8192;
const PHASE_TYPE: &str = "rpi.tool_program.child_phase.v1";

pub(super) struct Observer {
    session: Session,
    stop: CancellationToken,
    worker: Option<tokio::task::JoinHandle<()>>,
    job: Arc<Job>,
}
impl Observer {
    pub(super) fn start(session: Session, job: Arc<Job>) -> Self {
        let (sender, mut receiver) = broadcast::channel::<SessionEntryCommit>(256);
        session.set_entry_commit_publisher(Some(sender));
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let source = session.clone();
        let sink = job.clone();
        let worker = tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    biased;
                    received = receiver.recv() => received,
                    _ = worker_stop.cancelled() => break,
                };
                match received {
                    Ok(commit) => {
                        let Some(native) = commit.native_journal.clone() else {
                            continue;
                        };
                        let event = crate::rpc::tool_program_queries::commit_event(commit);
                        push(&sink, event);
                        if native.custom_type == PHASE_TYPE
                            && native.data.get("descriptor").is_some_and(|d| !d.is_null())
                        {
                            if body(&source, &sink, native).await.is_err() {
                                gap(&sink, "native_tool_program_body_unavailable");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        gap(&sink, "native_tool_program_commit_gap")
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self {
            session,
            stop,
            worker: Some(worker),
            job,
        }
    }
    pub(super) async fn finish(mut self) {
        self.session.set_entry_commit_publisher(None);
        self.stop.cancel();
        if let Some(mut worker) = self.worker.take() {
            if tokio::time::timeout(Duration::from_secs(2), &mut worker)
                .await
                .is_err()
            {
                gap(&self.job, "native_tool_program_drain_timeout");
                worker.abort();
                let _ = worker.await;
            }
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.session.set_entry_commit_publisher(None);
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}
fn push(job: &Job, value: Value) {
    job.state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .journal
        .push(value);
}
fn gap(job: &Job, reason: &str) {
    push(job, json!({"type":"observation_gap","reason":reason}));
}
async fn body(session: &Session, job: &Job, phase: NativeJournalCommit) -> Result<(), String> {
    let descriptor: BodyDescriptor = serde_json::from_value(phase.data["descriptor"].clone())
        .map_err(|_| "invalid descriptor")?;
    let mut query = ChildBodyQuery {
        child_id: descriptor.child_id.clone(),
        phase_entry_id: phase.entry_id.clone(),
        descriptor_id: descriptor.descriptor_id.clone(),
        leaf_id: Some(phase.entry_id.clone()),
        cursor: None,
        limit: 2,
        max_bytes: 65536,
    };
    let mut observed = 0;
    loop {
        let page = read_child_body_page(session, &phase.lane, &query)
            .await
            .map_err(|_| "native body query failed")?;
        for chunk in page.body.chunks {
            let bytes = STANDARD
                .decode(&chunk.data)
                .map_err(|_| "invalid body chunk")?;
            for (part, fragment) in bytes.chunks(FRAGMENT_BYTES).enumerate() {
                push(
                    job,
                    json!({"type":"tool_program_child_body","sourceSessionId":phase.source_session_id,
                    "lane":phase.lane,"childId":descriptor.child_id,"phaseEntryId":phase.entry_id,
                    "descriptorId":descriptor.descriptor_id,"byteOffset":chunk.byte_offset+part*FRAGMENT_BYTES,
                    "data":STANDARD.encode(fragment),"sha256":format!("{:x}",Sha256::digest(fragment))}),
                );
            }
            observed += bytes.len();
        }
        match page.next_cursor {
            None => return Ok(()),
            Some(cursor) if observed < MAX_BODY_BYTES => query.cursor = Some(cursor),
            Some(_) => {
                gap(job, "native_tool_program_body_limit");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> Session {
        Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "child-source".into(),
                    created_at: 0,
                    parent_session_id: Some("parent".into()),
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        )
    }
    #[tokio::test]
    async fn observer_ignores_ordinary_commits_and_drains_owned_receiver() {
        let source = session();
        let job = Arc::new(Job {
            owner: 7,
            cancel: CancellationToken::new(),
            invocation_signal: None,
            state: Mutex::new(State::default()),
        });
        let observer = Observer::start(source.clone(), job.clone());
        source
            .append_custom_entry(
                "ordinary",
                Some(json!({"private":"not a native tool program"})),
            )
            .await
            .unwrap();
        observer.finish().await;
        source.append_custom_entry("later", None).await.unwrap();
        let manager = ChildAgents::default();
        job.state.lock().unwrap().parent_session_id = "parent".into();
        manager
            .observations
            .lock()
            .unwrap()
            .remember("child-source".into(), job.clone());
        let roster = manager
            .query(
                "parent",
                json!({"type":"get_child_agents","version":1,"parentSessionId":"parent"}),
            )
            .unwrap();
        assert_eq!(roster["children"][0]["lastSeq"], 0);
        assert!(
            !job.cancel.is_cancelled(),
            "observation cleanup must not cancel execution"
        );
    }
}
