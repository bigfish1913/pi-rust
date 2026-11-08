//! Read-only TUI lifecycle observations; never recovery admission or execution.

use std::{future::Future, time::Duration};

use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{DurableActor, DurableError, DurableService, DurableSnapshot},
};

// Cooperative async timeout, not a hard wall-clock bound on a synchronous
// storage poll or DTO validation. Dropping the caller still cancels this read.
const READ_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_NOTICE_BYTES: usize = 4 * 1024;
const HELP: &str = "View-entry observation only; inspect /durable for current state and /durable help. Recovery is intent only; this notice starts no recovery or replay and provides no exactly-once guarantee. Open != Interrupted; Unknown/Safe do not authorize replay.";

/// Observes only the current harness binding. Disabled means no read is polled.
/// Dropping this future cancels its read: no spawned task survives the caller.
pub(crate) async fn lifecycle_notice(h: &AgentHarness, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    notice_from(
        true,
        async {
            let actor = DurableActor::child("native-notice")?;
            DurableService::bind(h, actor).snapshot().await
        },
        READ_TIMEOUT,
    )
    .await
}

// A future seam keeps timeout/cancellation tests independent of storage and tools.
async fn notice_from(
    enabled: bool,
    read: impl Future<Output = Result<DurableSnapshot, DurableError>>,
    timeout: Duration,
) -> Option<String> {
    if !enabled {
        return None;
    }
    Some(match tokio::time::timeout(timeout, read).await {
        Ok(Ok(snapshot)) => return observation(&snapshot),
        Ok(Err(error)) => unavailable(error_code(&error)),
        Err(_) => unavailable("timeout"),
    })
}

fn error_code(error: &DurableError) -> &'static str {
    // Match typed variants only; never render diagnostics, paths, or user input.
    match error {
        DurableError::Closed => "closed",
        DurableError::Sealed => "sealed",
        DurableError::Recovering => "recovering",
        DurableError::Conflict => "conflict",
        DurableError::PermissionDenied => "permission_denied",
        DurableError::InvalidRequest(_) => "invalid_request",
        DurableError::InvalidJournal(_) => "invalid_journal",
        DurableError::LegacyJournalPresent => "legacy_journal_present",
        DurableError::LimitExceeded => "limit_exceeded",
        DurableError::ReadFailure(_) => "read_failure",
        DurableError::CommitUncertain(_) => "commit_uncertain",
        DurableError::ExecutionUnavailable(_) => "execution_unavailable",
        DurableError::Execution(_) => "execution_error",
        DurableError::Control(_) => "control_error",
    }
}

fn unavailable(code: &str) -> String {
    // Defense in depth: this function also cannot reflect an arbitrary code.
    let code = match code {
        "closed"
        | "sealed"
        | "recovering"
        | "conflict"
        | "permission_denied"
        | "invalid_request"
        | "invalid_journal"
        | "legacy_journal_present"
        | "limit_exceeded"
        | "commit_uncertain"
        | "execution_unavailable"
        | "execution_error"
        | "control_error"
        | "timeout" => code,
        _ => "read_failure",
    };
    format!(
        "Observed native durable state: unavailable ({code}); not an empty-state observation. No recovery was started by this notice. {HELP}"
    )
}

fn observation(snapshot: &DurableSnapshot) -> Option<String> {
    // Validate before deciding idle: invalid schema is unavailable, not empty.
    if snapshot.state.validate().is_err() || snapshot.execution_control.validate().is_err() {
        return Some(unavailable("invalid_journal"));
    }
    let workflow = snapshot.state.workflow.as_ref().map(|w| w.status.as_str());
    let active_workflow = matches!(workflow, Some("running" | "paused" | "failed"));
    let queued = snapshot
        .state
        .queue
        .iter()
        .filter(|item| item.status.as_str() == "queued")
        .count();
    let claimed = snapshot
        .state
        .queue
        .iter()
        .filter(|item| item.status.as_str() == "claimed")
        .count();
    let recovery = snapshot.execution_control.recovery_request.as_ref();
    if !active_workflow && queued == 0 && claimed == 0 && recovery.is_none() {
        return None;
    }
    // Project only status/counts and the explicit saved recovery target. Never
    // serialize scope, workflow names, queue content, owners, or hidden run IDs.
    let prefix = format!(
        "Observed native durable state: workflow={}; queue queued={queued}, claimed={claimed}.",
        workflow.unwrap_or("none")
    );
    let mut text = format!("{prefix} {HELP}");
    if let Some(request) = recovery {
        let lead = format!(
            " Saved recovery intent: unknown invocation count={}; recovery.runId=",
            request.unknown_invocation_ids.len()
        );
        let budget = MAX_NOTICE_BYTES.saturating_sub(text.len() + lead.len() + 1);
        text.push_str(&lead);
        if let Some(id) = escaped_id(&request.run_id, budget) {
            text.push_str(&id);
        } else {
            // No partial business IDs: omit the whole ID, explicitly.
            text.push_str("[entire ID omitted for display budget; inspect /durable]");
        }
        text.push('.');
    }
    debug_assert!(text.len() <= MAX_NOTICE_BYTES);
    Some(text)
}

/// Escape all C0/DEL/C1 controls and directional/line-formatting characters.
/// Quotes make the saved run's boundaries visible without interpreting markup.
/// Returning None omits the WHOLE ID rather than truncating its business value.
fn escaped_id(id: &str, budget: usize) -> Option<String> {
    let mut out = String::from("\"");
    for ch in id.chars() {
        let escaped = if ch.is_control()
            || matches!(ch, '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            format!("\\u{{{:x}}}", ch as u32)
        } else if matches!(ch, '"' | '\\') {
            format!("\\{ch}")
        } else {
            ch.to_string()
        };
        if out.len() + escaped.len() + 1 > budget {
            return None;
        }
        out.push_str(&escaped);
    }
    out.push('"');
    (out.len() <= budget).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        cell::Cell,
        pin::Pin,
        rc::Rc,
        sync::Arc,
        task::{Context, Poll},
    };

    fn snapshot(workflow: Option<&str>, queue: &[&str]) -> DurableSnapshot {
        let workflow = workflow.map(|status| {
            json!({
                "name":"PRIVATE-workflow", "status":status,
                "steps":[{"id":"s", "title":"PRIVATE-step", "status":"completed"}]
            })
        });
        let queue: Vec<Value> = queue
            .iter()
            .enumerate()
            .map(|(i, status)| {
                json!({
                    "id":format!("item-{}", i + 1), "text":"PRIVATE-queue",
                    "status":status, "createdRevision":1, "updatedRevision":1,
                    "claimedBy": if *status == "claimed" { Some("PRIVATE-owner") } else { None }
                })
            })
            .collect();
        serde_json::from_value(json!({
            "scope":{"sessionId":"PRIVATE-session", "lane":"PRIVATE-lane",
                "bindingToken":"PRIVATE-token", "leafId":null,
                "historyRevision":1, "entryId":null, "controlRevision":1},
            "state":{"revision":1, "nextQueueId":queue.len(), "workflow":workflow, "queue":queue},
            "executionControl":{}
        }))
        .unwrap()
    }

    fn recovery(s: &mut DurableSnapshot, id: &str) {
        s.execution_control = serde_json::from_value(json!({
            "forgottenRunIds":["PRIVATE-forgotten"],
            "recoveryRequest":{"runId":id, "startedSeq":1,
                "observedHistoryRevision":1, "unknownInvocationIds":["PRIVATE-invocation"]}
        }))
        .unwrap();
    }

    #[test]
    fn idle_and_terminal_only_are_silent() {
        for workflow in [None, Some("completed")] {
            for queue in [vec![], vec!["completed", "failed", "cancelled"]] {
                assert!(observation(&snapshot(workflow, &queue)).is_none());
            }
        }
        let mut s = snapshot(None, &[]);
        s.execution_control
            .forgotten_run_ids
            .push("PRIVATE-forgotten".into());
        assert!(observation(&s).is_none());
    }

    #[test]
    fn workflow_statuses_and_open_queue_counts() {
        for status in ["running", "paused", "failed", "completed"] {
            let s = snapshot(
                Some(status),
                &[
                    "queued",
                    "claimed",
                    "queued",
                    "completed",
                    "failed",
                    "cancelled",
                ],
            );
            let text = observation(&s).unwrap();
            assert!(text.contains(&format!("workflow={status}")));
            assert!(text.contains("queue queued=2, claimed=1"));
            assert!(!text.contains("PRIVATE"));
            if status != "completed" {
                assert!(observation(&snapshot(Some(status), &[])).is_some());
            }
        }
    }

    #[test]
    fn explicit_recovery_only_and_no_private_data() {
        let mut s = snapshot(None, &[]);
        recovery(&mut s, "saved-run");
        let text = observation(&s).unwrap();
        assert!(text.contains("recovery.runId=\"saved-run\""));
        assert!(text.contains("unknown invocation count=1"));
        assert!(text.contains("intent only"));
        assert!(text.contains("no recovery or replay"));
        assert!(text.contains("Open != Interrupted"));
        assert!(text.contains("Unknown/Safe do not authorize replay"));
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains("latest"));
    }

    #[test]
    fn controls_escaped_and_entire_large_id_omitted_within_four_kib() {
        let controls = "a\0\n\r\t\u{1b}\u{7f}\u{85}\u{9b}\u{202e}\"\\z";
        let escaped = escaped_id(controls, MAX_NOTICE_BYTES).unwrap();
        assert!(!escaped.chars().any(char::is_control));
        for code in [
            "\\u{0}",
            "\\u{1b}",
            "\\u{7f}",
            "\\u{85}",
            "\\u{9b}",
            "\\u{202e}",
        ] {
            assert!(escaped.contains(code));
        }
        let mut s = snapshot(Some("running"), &["claimed"]);
        recovery(&mut s, &"\u{202e}".repeat(512));
        let text = observation(&s).unwrap();
        assert!(text.len() <= MAX_NOTICE_BYTES);
        assert!(text.contains("entire ID omitted"));
        assert!(text.contains("inspect /durable"));
        assert!(!text.contains("\\u{202e}"));
        assert!(!text.chars().any(char::is_control));
        let id = "🙂".repeat(512);
        recovery(&mut s, &id);
        assert!(observation(&s).unwrap().contains(&id));
    }

    #[tokio::test]
    async fn failures_and_invalid_schema_are_unavailable_not_empty() {
        let text = notice_from(
            true,
            async { Err(DurableError::ReadFailure("PRIVATE/path/keys\u{1b}".into())) },
            READ_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(text.contains("unavailable (read_failure)"));
        assert!(text.contains("not an empty-state"));
        assert!(text.contains("No recovery was started"));
        assert!(!text.contains("PRIVATE"));
        assert!(!unavailable("PRIVATE/path").contains("PRIVATE"));
        let mut s = snapshot(None, &[]);
        s.state.next_queue_id = 0;
        // Invalid state must not masquerade as a harmless terminal-only queue.
        s.state.queue = snapshot(None, &["completed"]).state.queue;
        assert!(observation(&s).unwrap().contains("invalid_journal"));
        let mut s = snapshot(None, &[]);
        recovery(&mut s, "invalid\u{1b}run");
        assert!(observation(&s).unwrap().contains("invalid_journal"));
    }

    struct PendingRead {
        polls: Rc<Cell<usize>>,
        dropped: Rc<Cell<bool>>,
    }
    impl Future for PendingRead {
        type Output = Result<DurableSnapshot, DurableError>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.set(self.polls.get() + 1);
            Poll::Pending
        }
    }
    impl Drop for PendingRead {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }
    fn pending() -> (PendingRead, Rc<Cell<usize>>, Rc<Cell<bool>>) {
        let polls = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(false));
        (
            PendingRead {
                polls: polls.clone(),
                dropped: dropped.clone(),
            },
            polls,
            dropped,
        )
    }

    #[tokio::test]
    async fn timeout_and_caller_drop_cancel_read_without_spawn() {
        let (read, polls, dropped) = pending();
        let text = notice_from(true, read, Duration::ZERO).await.unwrap();
        assert!(text.contains("unavailable (timeout)"));
        assert!(text.contains("No recovery was started"));
        assert!(polls.get() > 0);
        assert!(dropped.get());
        let (read, polls, dropped) = pending();
        let mut notice = Box::pin(notice_from(true, read, READ_TIMEOUT));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(notice.as_mut().poll(&mut cx).is_pending());
        assert!(polls.get() > 0);
        drop(notice);
        assert!(dropped.get());
    }

    #[tokio::test]
    async fn disabled_does_not_poll_even_a_failing_or_pending_read() {
        let (read, polls, dropped) = pending();
        assert!(notice_from(false, read, READ_TIMEOUT).await.is_none());
        assert_eq!(polls.get(), 0);
        assert!(dropped.get());
        assert!(notice_from(
            false,
            async {
                panic!("disabled read must never be polled");
                #[allow(unreachable_code)]
                Err(DurableError::Closed)
            },
            READ_TIMEOUT
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn synthetic_memory_observation_has_no_extra_writes_and_disabled_closed_is_none() {
        use rpi_harness::{
            durable::DurableMutation,
            session::{
                memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
                types::{EntryQuery, SessionMetadata},
                Session,
            },
            types::AgentHarnessOptions,
        };
        let store = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "synthetic-notice".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let h = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(store, None),
            ..Default::default()
        })
        .await
        .unwrap();
        let service = DurableService::bind(&h, DurableActor::owner("notice-test").unwrap());
        service
            .mutate(DurableMutation {
                request_id: "setup".into(),
                expected: service.snapshot().await.unwrap().scope,
                action: serde_json::from_value(json!({"action":"enqueue", "text":"PRIVATE-queue"}))
                    .unwrap(),
                run_id: None,
            })
            .await
            .unwrap();
        let storage = h.session().storage();
        let before = service.snapshot().await.unwrap();
        let entries = storage.find_entries(&EntryQuery::default()).await.unwrap();
        let revision = storage.history_revision().await.unwrap();
        let receipt = service.request_outcome("setup").await.unwrap();
        for _ in 0..2 {
            assert!(lifecycle_notice(&h, true)
                .await
                .unwrap()
                .contains("queued=1, claimed=0"));
            assert!(lifecycle_notice(&h, false).await.is_none());
        }
        assert_eq!(service.snapshot().await.unwrap(), before);
        assert_eq!(storage.history_revision().await.unwrap(), revision);
        assert_eq!(
            storage.find_entries(&EntryQuery::default()).await.unwrap(),
            entries
        );
        assert_eq!(service.request_outcome("setup").await.unwrap(), receipt);
        h.close().await.unwrap();
        assert!(lifecycle_notice(&h, false).await.is_none());
        assert!(lifecycle_notice(&h, true)
            .await
            .unwrap()
            .contains("unavailable (closed)"));
    }
}
