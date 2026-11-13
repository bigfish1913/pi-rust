//! Stateless TUI continuation. Whole page or error: never skip undisplayed rows.
use super::{dto, error_text, execution_control, quoted, MAX_TEXT_BYTES, SAFETY};
use crate::native_durable::{parse_json, rpc, Failure, MAX_REQUEST_BYTES};
use rpi_harness::{agent_harness::AgentHarness, durable::DurableExecutionPage};
use serde_json::{json, Value};
use std::fmt::Write as _;

pub(super) async fn command(h: &AgentHarness, args: &str) -> Result<String, String> {
    // Raw whitespace counts before actor binding, JSON decoding or service reads.
    if args.len() > MAX_REQUEST_BYTES {
        return Err(error_text(&Failure::new(
            "limit_exceeded",
            "Durable request exceeds 64 KiB",
        )));
    }
    let tail = args
        .trim()
        .strip_prefix("runs-page")
        .expect("selected verb")
        .trim();
    let cursor = if tail.is_empty() {
        Value::Null
    } else {
        parse_json(tail).map_err(|error| error_text(&error))?
    };
    // Shared cursor validation and trusted actor binding; exactly one core page.
    // This TUI caller is already selected by the native-only command registry.
    let payload = rpc(
        h,
        &json!({"type":"get_durable_runs","version":1,"cursor":cursor}),
        true,
    )
    .await
    .map_err(|error| error_text(&error))?;
    let page: DurableExecutionPage = dto(&payload["page"])?;
    render(&page)
}

fn render(page: &DurableExecutionPage) -> Result<String, String> {
    let mut out =
        String::from("Native durable execution page (read-only, explicit continuation)\n");
    writeln!(out, "Captured scope {}", quoted(&page.scope)).unwrap();
    writeln!(out, "Scanned raw records: {} | verified rows: {} | unavailable candidates: {} (NOT an interrupted count).",
        page.scanned_records, page.runs.len(), page.unavailable_runs).unwrap();
    execution_control(
        &mut out,
        page.control.forgotten_run_ids.len(),
        page.control.recovery_request.as_ref().map(|request| {
            (
                request.run_id.as_str(),
                request.unknown_invocation_ids.len(),
                request.started_seq,
                request.observed_history_revision,
            )
        }),
    );
    for run in &page.runs {
        writeln!(
            out,
            "Run {} | status {} | started {} | finished {} | finish record {}",
            quoted(&run.id),
            quoted(&run.status),
            run.started_seq,
            quoted(&run.finished_seq),
            quoted(&run.finished_record_id)
        )
        .unwrap();
    }
    if page.runs.is_empty() {
        out.push_str("No verified run rows in THIS raw-record window; this is not a complete empty-history claim.\n");
    }
    if let Some(next) = &page.next {
        writeln!(
            out,
            "Next page command: /durable runs-page {}",
            quoted(next)
        )
        .unwrap();
        out.push_str("Continue only by explicitly copying that command. Cursor is unsigned position, not authorization or complete-history proof.\n");
    } else {
        out.push_str("No next cursor in this captured scope. No automatic latest-run selection.\n");
    }
    out.push_str("All verified rows in this page are displayed; none omitted. On conflict explicitly restart with /durable runs-page; never mix scopes. Open != Interrupted; run completed != workflow completed.\n");
    out.push_str(SAFETY);
    if out.len() > MAX_TEXT_BYTES {
        // The caller receives only this error, not the built rows or continuation.
        // Otherwise following next would silently skip rows omitted from display.
        return Err("Durable execution page view unavailable: complete page/scope/cursor/safety exceeds 16 KiB. No partial rows or next cursor displayed. This was read-only; no empty history inferred. Use RPC v1 get_durable_runs for the larger page.".into());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "pages/tests.rs"]
mod tests;
