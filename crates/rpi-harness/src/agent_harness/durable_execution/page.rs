//! Explicit, scope-bound paging. The old all-or-error index is unchanged.
use super::*;
use crate::durable::{DurableExecutionCursor, DurableExecutionPage, MAX_EXECUTION_PAGE_RECORDS};
use crate::session::types::RecordPageQuery;

impl AgentHarness {
    pub(crate) async fn durable_execution_page(
        &self,
        cursor: Option<&DurableExecutionCursor>,
    ) -> Result<DurableExecutionPage, DurableError> {
        let _gate = self.message_writes.lock().await;
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let snapshot = self.durable_projection().await?;
        let scope = &snapshot.scope;
        if target.binding_token() != scope.binding_token {
            return Err(DurableError::Conflict);
        }
        let storage = target.storage();
        let result = async {
            if let Some(cursor) = cursor {
                if cursor.scope != *scope {
                    return Err(DurableError::Conflict);
                }
                if cursor.before_seq == 0 || cursor.before_seq > scope.history_revision {
                    return Err(DurableError::InvalidRequest(
                        "execution cursor position is outside its scope".into(),
                    ));
                }
            }
            // Existing storage paging returns an ascending newest window, with
            // a count for exactly these exclusive bounds. Never scan/aggregate
            // unbounded windows here or infer a current/latest run from it.
            let page = storage
                .record_page(&RecordPageQuery {
                    lane: scope.lane.clone(),
                    after_seq: None,
                    before_seq: cursor.map(|cursor| cursor.before_seq),
                    limit: MAX_EXECUTION_PAGE_RECORDS + 1,
                    from_end: true,
                })
                .await
                .map_err(read_error)?;
            if page.records.len() != page.total.min(MAX_EXECUTION_PAGE_RECORDS + 1)
                || page.total as u128 > scope.history_revision as u128
            {
                return Err(invalid("execution page count or bound is invalid"));
            }
            let mut previous_seq = None;
            for record in &page.records {
                if record.lane() != scope.lane
                    || record.seq() == 0
                    || record.seq() > scope.history_revision
                    || cursor.is_some_and(|cursor| record.seq() >= cursor.before_seq)
                    || previous_seq.is_some_and(|seq| record.seq() <= seq)
                {
                    return Err(invalid("execution page ordering or scope is invalid"));
                }
                previous_seq = Some(record.seq());
            }
            let has_more = page.records.len() > MAX_EXECUTION_PAGE_RECORDS;
            let mut runs = Vec::new();
            let mut unavailable_runs = 0;
            let mut scanned_records = 0;
            let mut oldest_seq = None;
            // The older lookahead is shape-checked above, but neither consumed
            // nor proved here. The exclusive continuation includes it next time.
            for record in page
                .records
                .into_iter()
                .rev()
                .take(MAX_EXECUTION_PAGE_RECORDS)
            {
                scanned_records += 1;
                oldest_seq = Some(record.seq());
                let LaneRecord::OperationStarted(started) = record else {
                    continue;
                };
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
            let next = if has_more {
                Some(DurableExecutionCursor {
                    scope: scope.clone(),
                    before_seq: oldest_seq
                        .ok_or_else(|| invalid("execution page made no progress"))?,
                })
            } else {
                None
            };
            Ok((runs, unavailable_runs, scanned_records, next))
        }
        .await;
        // Always fence even an empty/filtered/error page. This is observed
        // consistency, not atomic storage fencing or protection against ABA.
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
        let (runs, unavailable_runs, scanned_records, next) = result?;
        Ok(DurableExecutionPage {
            scope: snapshot.scope,
            runs,
            unavailable_runs,
            control: snapshot.execution_control,
            scanned_records,
            next,
        })
    }
}
