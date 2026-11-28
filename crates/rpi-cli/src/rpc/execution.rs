//! Projection of the reliable harness execution stream. Durable RunEnd remains
//! a plugin callback; only lease release grants transport execution admission.
use super::writer::{write_agent_event, write_line, write_observed_agent_event, RpcWriter};
use rpi_harness::execution::{ExecutionEvent, ExecutionIdentity, ExecutionOutcome};
use serde_json::{json, Value};

/// Child-launch opt-in: old hosts ignore it; old clients never set it. Do not
/// infer client support from an installed plugin's version (extcore is a parent).
pub const LIFECYCLE_ENV: &str = "RPI_RPC_EXECUTION_LIFECYCLE";
pub const LIFECYCLE_VERSION: &str = "correlated-v1";

pub(crate) fn lifecycle_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new(LIFECYCLE_VERSION))
}

// A panic/caller loss before command settlement must close the transport,
// including failures before a harness execution lease was ever acquired.
pub(crate) struct PendingSettlement(Option<tokio_util::sync::CancellationToken>);
impl PendingSettlement {
    pub fn new(failed: tokio_util::sync::CancellationToken) -> Self {
        Self(Some(failed))
    }
    pub fn complete(mut self) {
        self.0.take();
    }
}
impl Drop for PendingSettlement {
    fn drop(&mut self) {
        if let Some(failed) = self.0.take() {
            failed.cancel();
        }
    }
}

pub(crate) struct Observation<'a>(&'a rpi_harness::agent_harness::AgentHarness);
impl<'a> Observation<'a> {
    pub fn new(
        harness: &'a rpi_harness::agent_harness::AgentHarness,
        observer: rpi_harness::execution::ExecutionObserver,
    ) -> Self {
        harness.set_execution_observer(Some(observer));
        Self(harness)
    }
}
impl Drop for Observation<'_> {
    fn drop(&mut self) {
        self.0.set_execution_observer(None);
    }
}

pub(crate) fn attach(value: &mut Value, identity: &ExecutionIdentity) {
    value["executionId"] = json!(identity.execution_id);
    value["requestId"] = json!(identity.request_id);
    value["lane"] = json!(identity.lane);
}

pub(crate) async fn forward(out: &RpcWriter, event: ExecutionEvent, correlated: bool) {
    match event {
        ExecutionEvent::SessionProgress(event) => write_agent_event(out, &event).await,
        ExecutionEvent::Started { identity } => {
            if !correlated {
                return;
            }
            let mut value = json!({"type":"execution_started", "runId":null});
            attach(&mut value, &identity);
            write_line(out, &value).await;
        }
        ExecutionEvent::Progress {
            identity,
            run_id,
            event,
        } => {
            if correlated {
                write_observed_agent_event(out, &event, &identity, &run_id).await;
            } else {
                write_agent_event(out, &event).await;
            }
        }
        ExecutionEvent::Released {
            identity,
            run_id,
            outcome,
            recovery_required,
            pending_resume,
        } => {
            if correlated {
                let mut released = json!({"type":"execution_released", "runId":run_id,
                    "recoveryRequired":recovery_required});
                attach(&mut released, &identity);
                write_line(out, &released).await;
            }
            if recovery_required {
                // No durable proof: never turn RAII lease release into a
                // successful settlement or silently permit another prompt.
                write_line(out, &json!({"type":"host_error", "error":"execution finalization failed; reopen the native host for recovery"})).await;
                out.failure().cancel();
                return;
            }
            // RPC-owned operations settle only after their command wrapper has
            // finished cleanup. This event only releases that native lease.
            if !correlated || identity.request_id.is_some() {
                return;
            }
            let (outcome, final_text, error, summary) = match outcome {
                Some(ExecutionOutcome::Run(outcome)) => {
                    let (outcome, text, error) = crate::modes::rpc_outcome_str(&outcome);
                    (outcome, text, error, None)
                }
                Some(ExecutionOutcome::Compact(outcome)) => {
                    let value = crate::modes::compaction_json(
                        &rpi_harness::agent_harness::CompactionResult {
                            run_id: run_id.clone(),
                            outcome,
                        },
                    );
                    let tag = match value["outcome"].as_str() {
                        Some("compacted") => "compacted",
                        Some("declined") => "declined",
                        Some("aborted") => "aborted",
                        _ => "failed",
                    };
                    (
                        tag,
                        None,
                        value["error"].as_str().map(str::to_owned),
                        value["summary"].as_str().map(str::to_owned),
                    )
                }
                Some(ExecutionOutcome::Skipped) => ("skipped", None, None, None),
                None => (
                    "failed",
                    None,
                    Some("execution stopped before starting a durable operation".into()),
                    None,
                ),
            };
            let mut settled = json!({"type":"agent_settled", "runId":run_id,
                "outcome":outcome, "aborted":outcome == "aborted", "error":error,
                "finalText":final_text, "summary":summary, "pendingResume":pending_resume,
                "reclaimed":null});
            attach(&mut settled, &identity);
            write_line(out, &settled).await;
        }
        ExecutionEvent::Barrier(done) => {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_requires_an_exact_explicit_child_opt_in() {
        use std::ffi::OsStr;
        for value in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("1")),
            Some(OsStr::new("correlated-v2")),
        ] {
            assert!(!lifecycle_enabled(value));
        }
        assert!(lifecycle_enabled(Some(OsStr::new("correlated-v1"))));
    }

    #[tokio::test]
    async fn accepted_command_loss_fails_closed_even_before_execution_admission() {
        let failed = tokio_util::sync::CancellationToken::new();
        let task_failed = failed.clone();
        let task = tokio::spawn(async move {
            let _pending = PendingSettlement::new(task_failed);
            panic!("simulated command failure before harness admission");
        });
        assert!(task.await.is_err());
        assert!(failed.is_cancelled());
        let healthy = tokio_util::sync::CancellationToken::new();
        PendingSettlement::new(healthy.clone()).complete();
        assert!(!healthy.is_cancelled());
    }
}
