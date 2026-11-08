//! Explicit-run native execution reads. No event observers, executors or cache.
mod page;
use super::*;
use crate::durable::{
    DurableError, DurableExecutionIndexSnapshot, DurableExecutionSnapshot, DurableRunSummary,
    DurableScope, MAX_EXECUTION_INDEX_STARTS,
};
use crate::session::types::{EntryCursor, SessionStorage};
use rpi_durable::execution::{
    ReplayPolicy, RunCheckpoint, RunStatus, ToolCheckpoint, ToolStatus, MAX_TOOLS,
};

fn read_error(error: impl std::fmt::Display) -> DurableError {
    DurableError::ReadFailure(error.to_string())
}
fn invalid(message: &str) -> DurableError {
    DurableError::InvalidJournal(format!("native execution: {message}"))
}
fn unavailable(message: &str) -> DurableError {
    DurableError::ExecutionUnavailable(message.into())
}

/// The native proof needs only this read surface. Keeping it narrower than
/// SessionStorage lets an explicit historical reader filter records before the
/// proof sees them, without inventing a second storage implementation.
#[async_trait::async_trait]
pub(crate) trait NativeExecutionStorage: Send + Sync {
    async fn find_records(
        &self,
        query: &RecordQuery,
    ) -> crate::error::SessionResult<Vec<LaneRecord>>;
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> crate::error::SessionResult<Vec<Entry>>;
    async fn history_identity(
        &self,
    ) -> crate::error::SessionResult<crate::session::types::HistoryIdentity>;
}

struct CurrentStorage<'a>(&'a dyn SessionStorage);

#[async_trait::async_trait]
impl NativeExecutionStorage for CurrentStorage<'_> {
    async fn find_records(
        &self,
        query: &RecordQuery,
    ) -> crate::error::SessionResult<Vec<LaneRecord>> {
        self.0.find_records(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> crate::error::SessionResult<Vec<Entry>> {
        self.0.find_entries_on_branch(query, bounds, start).await
    }
    async fn history_identity(
        &self,
    ) -> crate::error::SessionResult<crate::session::types::HistoryIdentity> {
        self.0.history_identity().await
    }
}

/// Both discovery APIs adopt only a proof of the exact scanned ID/sequence pair.
fn proven_summary(run: RunCheckpoint, scanned_seq: u64) -> Result<DurableRunSummary, DurableError> {
    if run.started_seq != scanned_seq {
        return Err(invalid("run index start disagrees with native proof"));
    }
    Ok(DurableRunSummary {
        id: run.id,
        started_seq: run.started_seq,
        finished_record_id: run.finished_record_id,
        finished_seq: run.finished_seq,
        status: run.status,
    })
}

/// Membership is checked against the captured immutable parent chain, not a
/// moving lane pointer. Only a referenced entry is hydrated, never result text
/// copied into the public checkpoint.
async fn member(
    storage: &dyn NativeExecutionStorage,
    scope: &DurableScope,
    id: &str,
) -> Result<Option<Entry>, DurableError> {
    let Some(leaf) = scope.leaf_id.as_deref() else {
        return Ok(None);
    };
    let entries = storage
        .find_entries_on_branch(
            &EntryQuery {
                entry_id: Some(id.into()),
                limit: Some(1),
                ..Default::default()
            },
            &BranchBounds::default(),
            leaf,
        )
        .await
        .map_err(read_error)?;
    match entries.into_iter().next() {
        Some(entry) if entry.id() == id && entry.seq() <= scope.history_revision => Ok(Some(entry)),
        Some(_) => Err(invalid("branch membership returned an unexpected identity")),
        None => Ok(None),
    }
}

impl AgentHarness {
    pub(crate) async fn durable_execution_index(
        &self,
    ) -> Result<DurableExecutionIndexSnapshot, DurableError> {
        let _gate = self.message_writes.lock().await;
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let snapshot = self.durable_projection().await?;
        let scope = &snapshot.scope;
        if target.binding_token() != scope.binding_token {
            return Err(DurableError::Conflict);
        }
        let storage = target.storage();
        let result = async {
            let starts = storage
                .find_records(&RecordQuery {
                    lane: Some(scope.lane.clone()),
                    record_type: Some("operation_started"),
                    order: Some(EntryOrder::NewestFirst),
                    limit: Some(MAX_EXECUTION_INDEX_STARTS + 1),
                    ..Default::default()
                })
                .await
                .map_err(read_error)?;
            if starts.len() > MAX_EXECUTION_INDEX_STARTS {
                return Err(DurableError::LimitExceeded);
            }
            let mut runs = Vec::new();
            let mut unavailable_runs = 0;
            let mut previous_seq = None;
            for record in starts {
                let LaneRecord::OperationStarted(started) = record else {
                    return Err(invalid("run index received a non-start record"));
                };
                if started.base.lane != scope.lane
                    || started.base.seq == 0
                    || started.base.seq > scope.history_revision
                    || previous_seq.is_some_and(|seq| started.base.seq >= seq)
                {
                    return Err(invalid("run index start ordering or scope is invalid"));
                }
                previous_seq = Some(started.base.seq);
                if !matches!(started.intent, OperationIntent::Run { .. })
                    || snapshot
                        .execution_control
                        .forgotten_run_ids
                        .contains(&started.base.id)
                {
                    continue;
                }
                match self
                    .durable_read_execution(&target, scope, &started.base.id)
                    .await
                {
                    Ok(run) => runs.push(proven_summary(run, started.base.seq)?),
                    Err(DurableError::ExecutionUnavailable(_)) => unavailable_runs += 1,
                    Err(error) => return Err(error),
                }
            }
            Ok((runs, unavailable_runs))
        }
        .await;
        // Even an all-unavailable/forgotten/empty index needs a final fence:
        // individual proof reads may have returned before their own recheck.
        // This detects observed movement, not cross-process generation fencing.
        let identity = storage.history_identity().await.map_err(read_error)?;
        if target
            .view(&scope.lane)
            .get_leaf_id()
            .await
            .map_err(read_error)?
            != scope.leaf_id
            || identity.revision != scope.history_revision
            || identity.generation != scope.history_generation
            || self.session.binding_token() != scope.binding_token
        {
            return Err(DurableError::Conflict);
        }
        let (runs, unavailable_runs) = result?;
        Ok(DurableExecutionIndexSnapshot {
            scope: snapshot.scope,
            runs,
            unavailable_runs,
            control: snapshot.execution_control,
        })
    }

    pub(crate) async fn durable_execution(
        &self,
        run_id: &str,
    ) -> Result<DurableExecutionSnapshot, DurableError> {
        let _gate = self.message_writes.lock().await;
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let snapshot = self.durable_projection().await?;
        if target.binding_token() != snapshot.scope.binding_token {
            return Err(DurableError::Conflict);
        }
        let run = if snapshot
            .execution_control
            .forgotten_run_ids
            .iter()
            .any(|id| id == run_id)
        {
            None
        } else {
            Some(
                self.durable_read_execution(&target, &snapshot.scope, run_id)
                    .await?,
            )
        };
        Ok(DurableExecutionSnapshot {
            scope: snapshot.scope,
            run,
            control: snapshot.execution_control,
        })
    }

    pub(super) async fn durable_read_execution(
        &self,
        target: &Session,
        scope: &DurableScope,
        run_id: &str,
    ) -> Result<RunCheckpoint, DurableError> {
        let storage = target.storage();
        Self::durable_read_execution_from_storage(
            &self.session,
            scope,
            run_id,
            &CurrentStorage(storage.as_ref()),
        )
        .await
    }

    pub(crate) async fn durable_read_execution_from_storage(
        source: &Session,
        scope: &DurableScope,
        run_id: &str,
        storage: &dyn NativeExecutionStorage,
    ) -> Result<RunCheckpoint, DurableError> {
        let starts = storage
            .find_records(&RecordQuery {
                lane: Some(scope.lane.clone()),
                record_type: Some("operation_started"),
                run_id: Some(run_id.into()),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .map_err(read_error)?;
        let started = match starts.as_slice() {
            [LaneRecord::OperationStarted(started)]
                if started.base.id == run_id
                    && started.base.lane == scope.lane
                    && matches!(started.intent, OperationIntent::Run { .. }) =>
            {
                started
            }
            [] => return Err(unavailable("no native run with that identity on this lane")),
            _ => return Err(invalid("invalid or ambiguous native run start")),
        };
        if started.base.seq == 0 || started.base.seq > scope.history_revision {
            return Err(invalid("run start is outside the observed history"));
        }
        if let Some(source) = started.source_leaf_id.as_deref() {
            let source = member(storage, scope, source)
                .await?
                .ok_or_else(|| unavailable("run source is not on the selected branch"))?;
            if source.seq() >= started.base.seq {
                return Err(invalid("run source did not precede its start"));
            }
        }
        let finishes = storage
            .find_records(&RecordQuery {
                lane: Some(scope.lane.clone()),
                record_type: Some("operation_finished"),
                run_id: Some(run_id.into()),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .map_err(read_error)?;
        let finished = match finishes.as_slice() {
            [] => None,
            [LaneRecord::OperationFinished(finished)]
                if finished.run_id == run_id && finished.base.lane == scope.lane =>
            {
                Some(finished)
            }
            _ => return Err(invalid("invalid or ambiguous native terminal record")),
        };
        let end_seq = finished.map(|r| r.base.seq).unwrap_or(u64::MAX);
        if finished
            .is_some_and(|r| r.base.seq <= started.base.seq || r.base.seq > scope.history_revision)
        {
            return Err(invalid("terminal record is outside the run interval"));
        }
        let mut anchored = false;
        // Normal prompt admission commits operation_started and its first
        // message atomically at adjacent sequences. This proves the run's branch
        // even when startup recovery has no terminal assistant/result identity.
        if let Some(leaf) = scope.leaf_id.as_deref() {
            let first = storage
                .find_entries_on_branch(
                    &EntryQuery {
                        entry_type: Some("message"),
                        cursor: Some(EntryCursor {
                            after_seq: started.base.seq,
                        }),
                        order: Some(EntryOrder::OldestFirst),
                        limit: Some(1),
                        ..Default::default()
                    },
                    &BranchBounds::default(),
                    leaf,
                )
                .await
                .map_err(read_error)?;
            anchored = first.first().is_some_and(|entry| {
                started.base.seq.checked_add(1) == Some(entry.seq())
                    && entry.parent_id() == started.source_leaf_id.as_deref()
            });
        }
        let mut final_entry_id = None;
        let mut assistant_entry_id = None;
        let mut terminal_leaf_seq = None;
        let mut final_seq = None;
        let mut assistant_seq = None;
        if let Some(identity) = finished.and_then(|r| r.result.as_ref()) {
            if !identity.leaf_id.is_empty() {
                let leaf = member(storage, scope, &identity.leaf_id)
                    .await?
                    .ok_or_else(|| {
                        unavailable("native terminal leaf is not on the selected branch")
                    })?;
                if leaf.seq() <= started.base.seq || leaf.seq() >= end_seq {
                    return Err(invalid("terminal leaf is outside the run interval"));
                }
                terminal_leaf_seq = Some(leaf.seq());
                anchored = true;
            }
            for (id, assistant) in [
                (identity.final_entry_id.as_deref(), false),
                (identity.assistant_entry_id.as_deref(), true),
            ] {
                if let Some(id) = id {
                    let entry = member(storage, scope, id).await?.ok_or_else(|| {
                        unavailable("terminal reference is not on the selected branch")
                    })?;
                    if entry.seq() <= started.base.seq
                        || entry.seq() >= end_seq
                        || terminal_leaf_seq.is_some_and(|leaf| entry.seq() > leaf)
                    {
                        return Err(invalid(
                            "terminal reference is outside the terminal branch snapshot",
                        ));
                    }
                    if assistant
                        && !matches!(&entry, Entry::Message(m) if matches!(&m.message, AgentMessage::Assistant(_)))
                    {
                        return Err(invalid("terminal assistant reference is not an assistant"));
                    }
                    if assistant {
                        assistant_seq = Some(entry.seq());
                        assistant_entry_id = Some(id.to_owned());
                    } else {
                        final_seq = Some(entry.seq());
                        final_entry_id = Some(id.to_owned());
                    }
                    anchored = true;
                }
            }
        }
        if matches!((assistant_seq, final_seq), (Some(assistant), Some(final_entry)) if assistant > final_entry)
        {
            return Err(invalid("terminal assistant follows the final entry"));
        }
        let tool_records = storage
            .find_records(&RecordQuery {
                lane: Some(scope.lane.clone()),
                record_type: Some("tool_started"),
                run_id: Some(run_id.into()),
                order: Some(EntryOrder::OldestFirst),
                limit: Some(MAX_TOOLS + 1),
                ..Default::default()
            })
            .await
            .map_err(read_error)?;
        if tool_records.len() > MAX_TOOLS {
            return Err(DurableError::LimitExceeded);
        }
        let mut tools = Vec::new();
        for record in tool_records {
            let LaneRecord::ToolStarted(tool) = record else {
                return Err(invalid("unexpected native tool record"));
            };
            if tool.run_id != run_id
                || tool.base.lane != scope.lane
                || tool.base.seq <= started.base.seq
                || tool.base.seq >= end_seq
                || tool.base.seq > scope.history_revision
            {
                return Err(invalid("tool invocation is outside the run interval"));
            }
            let entry = member(storage, scope, &tool.assistant_entry_id)
                .await?
                .ok_or_else(|| {
                    unavailable("tool invocation assistant is not on the selected branch")
                })?;
            let Entry::Message(message) = entry else {
                return Err(invalid("tool invocation has no assistant entry"));
            };
            if message.base.seq <= started.base.seq
                || message.base.seq >= tool.base.seq
                || terminal_leaf_seq.is_some_and(|leaf| message.base.seq > leaf)
            {
                return Err(invalid("assistant is outside the invocation interval"));
            }
            let AgentMessage::Assistant(assistant) = message.message else {
                return Err(invalid("tool source is not an assistant"));
            };
            let call = assistant
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::ToolCall(call) => Some(call),
                    _ => None,
                })
                .nth(tool.tool_index as usize);
            if !call.is_some_and(|call| call.id == tool.tool_call_id && call.name == tool.tool_name)
            {
                return Err(invalid(
                    "tool invocation does not match the exact assistant ordinal",
                ));
            }
            anchored = true;
            let mut result_entry_id = None;
            let mut status = ToolStatus::Unknown;
            if let Some(entry) = member(storage, scope, &tool.result_entry_id).await? {
                if entry.seq() <= tool.base.seq
                    || entry.seq() >= end_seq
                    || terminal_leaf_seq.is_some_and(|leaf| entry.seq() > leaf)
                {
                    return Err(invalid("tool result is outside the invocation interval"));
                }
                let Entry::Message(message) = entry else {
                    return Err(invalid("reserved tool result is not a message"));
                };
                let AgentMessage::ToolResult(result) = message.message else {
                    return Err(invalid("reserved tool result is not a tool result"));
                };
                if result.tool_call_id != tool.tool_call_id || result.tool_name != tool.tool_name {
                    return Err(invalid("reserved tool result identity mismatch"));
                }
                // Existing recovery commits a synthetic error for unresolved
                // side effects. Its presence is NOT proof that the tool failed.
                let unknown = result.content.iter().any(|c| matches!(c, Content::Text(t) if t.text == crate::frame_progress::INTERRUPTED_TOOL_RESULT));
                status = if unknown {
                    ToolStatus::Unknown
                } else if result.is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                result_entry_id = Some(tool.result_entry_id.clone());
            }
            tools.push(ToolCheckpoint {
                invocation_id: tool.base.id,
                assistant_entry_id: tool.assistant_entry_id,
                tool_index: tool.tool_index,
                call_id: tool.tool_call_id,
                name: tool.tool_name,
                reserved_result_entry_id: tool.result_entry_id,
                result_entry_id,
                status,
                replay: match tool.replay {
                    ToolReplay::Never => ReplayPolicy::Never,
                    ToolReplay::Safe => ReplayPolicy::Safe,
                },
            });
        }
        if !anchored {
            return Err(unavailable(
                "run has no provable entry anchor on this branch",
            ));
        }
        let status = match finished {
            None => RunStatus::Open,
            Some(finished)
                if finished.outcome == OperationOutcome::Aborted
                    && finished
                        .error
                        .as_ref()
                        .is_some_and(|error| error.code == "interrupted") =>
            {
                RunStatus::Interrupted
            }
            Some(finished) => match finished.outcome {
                OperationOutcome::Completed => RunStatus::Completed,
                OperationOutcome::Aborted => RunStatus::Aborted,
                OperationOutcome::Failed => RunStatus::Failed,
                OperationOutcome::Declined => RunStatus::Declined,
            },
        };
        let run = RunCheckpoint {
            id: run_id.into(),
            started_seq: started.base.seq,
            finished_record_id: finished.map(|r| r.base.id.clone()),
            finished_seq: finished.map(|r| r.base.seq),
            status,
            final_entry_id,
            assistant_entry_id,
            tools,
        };
        run.validate()
            .map_err(|error| DurableError::InvalidJournal(error.to_string()))?;
        let identity = storage.history_identity().await.map_err(read_error)?;
        if identity.revision != scope.history_revision
            || identity.generation != scope.history_generation
            || source.binding_token() != scope.binding_token
        {
            return Err(DurableError::Conflict);
        }
        Ok(run)
    }
}
