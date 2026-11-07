//! TUI-only branch discovery. Never a new wire action or a run-selection policy.
use super::{error_text, execution_control, quoted, scope, MAX_TEXT_BYTES, SAFETY};
use crate::native_durable::{service, Failure, MAX_REQUEST_BYTES};
use rpi_harness::{agent_harness::AgentHarness, durable::DurableExecutionIndexSnapshot};
use std::fmt::Write as _;

pub(super) async fn command(h: &AgentHarness, args: &str) -> Result<String, String> {
    // Count raw whitespace before actor binding or any read, as JSON commands do.
    if args.len() > MAX_REQUEST_BYTES {
        return Err(error_text(&Failure::new(
            "limit_exceeded",
            "Durable request exceeds 64 KiB",
        )));
    }
    let service = service(h).map_err(|error| error_text(&error))?;
    let index = service
        .execution_index()
        .await
        .map_err(|error| error_text(&Failure::from(error)))?;
    render(&index)
}

fn display_summary(shown: usize, total: usize) -> String {
    format!("Displayed {shown} of {total} verified run rows; omitted {} whole rows for 16 KiB. No IDs truncated; no pagination. Choose an explicit runId. Open != Interrupted; run completed != workflow completed.\n", total - shown)
}

fn render(index: &DurableExecutionIndexSnapshot) -> Result<String, String> {
    let mut out = String::from("Native durable branch run index (read-only)\n");
    scope(&mut out, &index.scope);
    writeln!(out, "Captured leaf {}.", quoted(&index.scope.leaf_id)).unwrap();
    writeln!(out, "Verified runs: {} | unavailable candidates: {} (unproven on this branch, including siblings; NOT an interrupted count).",
        index.runs.len(), index.unavailable_runs).unwrap();
    out.push_str("Descending native start sequence, not latest-run selection.\n");
    execution_control(
        &mut out,
        index.control.forgotten_run_ids.len(),
        index.control.recovery_request.as_ref().map(|request| {
            (
                request.run_id.as_str(),
                request.unknown_invocation_ids.len(),
                request.started_seq,
                request.observed_history_revision,
            )
        }),
    );
    out.push_str("Observe an explicit ID with /durable {\"action\":\"status\",\"runId\":\"<native-run-id>\"}. recover/forget_run still require an explicit business requestId.\n");
    if index.runs.is_empty() {
        out.push_str("No proven, non-forgotten run rows in this captured branch view.\n");
    }
    let mut shown = 0;
    for run in &index.runs {
        let row = format!(
            "Run {} | status {} | started {} | finished {} | finish record {}\n",
            quoted(&run.id),
            quoted(&run.status),
            run.started_seq,
            quoted(&run.finished_seq),
            quoted(&run.finished_record_id),
        );
        let footer = display_summary(shown + 1, index.runs.len());
        if out.len() + row.len() + footer.len() + SAFETY.len() > MAX_TEXT_BYTES {
            break;
        }
        out.push_str(&row);
        shown += 1;
    }
    out.push_str(&display_summary(shown, index.runs.len()));
    out.push_str(SAFETY);
    if out.len() > MAX_TEXT_BYTES {
        // Mandatory scope/counts/safety must fit together. Never truncate a data
        // identity or silently report an empty history to make the view fit.
        return Err("Durable run index view unavailable: required scope/safety summary exceeds 16 KiB. This was read-only; no empty run history inferred.".into());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "runs/tests.rs"]
mod tests;
