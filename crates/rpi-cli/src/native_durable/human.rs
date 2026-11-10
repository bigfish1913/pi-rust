//! TUI-only plain text. Existing JSON uses the wire admission/mutation path;
//! `runs` is a read-only view of the same core's branch-proof index.
#[path = "human/pages.rs"]
mod pages;
#[path = "human/runs.rs"]
mod runs;
use std::fmt::Write as _;

use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{
        DurableExecutionSnapshot, DurableOutcome, DurableReceipt, DurableScope, DurableSnapshot,
    },
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use super::{command, valid_identity, Failure};

const MAX_TEXT_BYTES: usize = 16 * 1024;
const SCHEMA: &str = "Durable view unavailable: invalid or unrecognized native durable v1 response schema; no empty state inferred.";
const SAFETY: &str = "Manual queue, not workers. No scheduler, delegation, or exactly-once guarantee. Unknown outcomes and Safe replay metadata are not permission to execute. No automatic recovery or replay.\n";
const DETAILS: &str = "SUMMARY: details omitted to keep the complete human view within 16 KiB. Use manual RPC v1 get_durable_state (explicit runId for execution) for omitted details; get_durable_request for a stored receipt/original guard.\n";
const HELP: &str = r#"Native durable (current session branch/lane; plain text)
/durable                         Observe workflow and manual queue
/durable help                    This help (also ? or --help)
/durable runs                    Read verified branch-run metadata (no latest default)
/durable runs-page               Read one raw-record window of verified runs
/durable runs-page <cursor JSON> Continue ONLY with the copied next-page cursor
/durable {"action":"status","runId":"run-42"}
/durable {"action":"status","requestId":"review-001"}
Status runId and requestId are mutually exclusive. No latest-run inference.
runs is TUI-only, not a JSON/tool/RPC action. It scans at most 64 lane starts
(including sibling, non-run and forgotten starts); oversize history is an error.
Unproven candidates are counted, not listed as branch members or interrupted.
Text may omit whole run rows with explicit counts; no run ID is truncated.
Choose an explicit runId; this command never selects or recovers one for you.
runs-page is a separate TUI view: 64 raw records per explicit call. Empty rows
with a next-page command are NOT a complete empty history. Copy that command
unchanged; scope movement conflicts. Explicitly use runs-page without a cursor
to restart, never mix scopes. The entire page must fit 16 KiB or it fails without
showing partial rows/continuation. Use RPC get_durable_runs for larger pages.

Flat JSON actions: status, queue, start, checkpoint, pause, resume, complete,
fail, reset, enqueue, claim, finish, cancel, recover, forget_run.
Every mutation requires an explicit business requestId (1-128 ASCII letters,
digits, dot, underscore, hyphen), distinct from transport/provider call IDs.
Example mutation, submit ONCE:
/durable {"action":"enqueue","requestId":"review-001","text":"Review patch","expected":{"sessionId":"session-7","lane":"main","bindingToken":"binding-9","leafId":null,"historyRevision":12,"entryId":null,"controlRevision":0}}
expected is optional on the flat command; when supplied use the entire observed
scope unchanged, NOT these example values. The formatter never fills guards.
Recover and forget_run require BOTH explicit runId and requestId, e.g.
/durable {"action":"recover","runId":"run-42","requestId":"recover-002"}
/durable {"action":"forget_run","runId":"run-42","requestId":"hide-003"}
Do not retry unchecked arguments. On CAS conflict read new state and make an
explicit decision; no automatic retry. On uncertain commit look up the known
requestId and retain the original guard; absence of a receipt is not proof of
no effect. Receipt lookup is scoped to current actor and branch.

Queue is manual: claim before doing work, then finish using the returned item ID.
No scheduler, delegation, workers, or exactly-once guarantee. Unknown outcomes
and Safe replay metadata are NOT permission. Open != Interrupted; Interrupted
requires a native committed finish record. Run completed != workflow completed.
recover records INTENT ONLY and executes nothing. forget_run hides a business
projection, not native audit or get_result. reset clears only the workflow,
not the queue or execution journal. No automatic recovery or replay.
"#;

pub(crate) async fn interactive_command(h: &AgentHarness, args: &str) -> Result<String, String> {
    if matches!(args.trim(), "help" | "?" | "--help") {
        return Ok(HELP.into());
    }
    if args.trim() == "runs" {
        return runs::command(h, args).await;
    }
    if args.split_whitespace().next() == Some("runs-page") {
        return pages::command(h, args).await;
    }
    // Exact original arguments, exactly one admission. No follow-up reads/retries.
    let value = command(h, args).await.map_err(|e| error_text(&e))?;
    render(&value)
}

// JSON quoting covers C0, quotes and backslashes. Also escape DEL/C1, which
// serde_json deliberately leaves literal. Never emit terminal controls in data.
fn quoted<T: Serialize + ?Sized>(value: &T) -> String {
    let json = serde_json::to_string(value).expect("durable DTO serializes");
    let mut out = String::new();
    for ch in json.chars() {
        if ch.is_control() {
            write!(out, "\\u{:04x}", ch as u32).unwrap();
        } else {
            out.push(ch);
        }
    }
    out
}
fn dto<T: DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|_| SCHEMA.into())
}
fn required(value: &Value, fields: &[&str]) -> Result<(), String> {
    if value.is_object() && fields.iter().all(|key| value.get(key).is_some()) {
        Ok(())
    } else {
        Err(SCHEMA.into())
    }
}
fn scope_shape(value: &Value) -> Result<(), String> {
    required(value, super::SCOPE_FIELDS)
}
fn snapshot(value: &Value) -> Result<DurableSnapshot, String> {
    required(value, &["scope", "state", "executionControl"])?;
    scope_shape(&value["scope"])?;
    // ControlState has serde defaults for legacy restoration, not for a UI wire
    // response. An empty object must never become a fabricated empty workflow.
    required(
        &value["state"],
        &["revision", "nextQueueId", "workflow", "queue"],
    )?;
    let s: DurableSnapshot = dto(value)?;
    s.state.validate().map_err(|_| SCHEMA.to_owned())?;
    s.execution_control
        .validate()
        .map_err(|_| SCHEMA.to_owned())?;
    if s.scope.control_revision != s.state.revision {
        return Err(SCHEMA.into());
    }
    Ok(s)
}
fn receipt(value: &Value) -> Result<DurableReceipt, String> {
    required(value, &["expected"])?;
    scope_shape(&value["expected"])?;
    let r: DurableReceipt = dto(value)?;
    if !valid_identity(&r.request_id) || !valid_identity(&r.actor) {
        return Err(SCHEMA.into());
    }
    Ok(r)
}

enum View {
    Control(DurableSnapshot),
    Execution(DurableExecutionSnapshot),
    Outcome(DurableOutcome),
    Receipt(Option<DurableReceipt>),
}
fn decode(value: &Value) -> Result<View, String> {
    let map = value.as_object().ok_or_else(|| SCHEMA.to_owned())?;
    if value["version"].as_u64() != Some(1) || map.len() != 2 {
        return Err(SCHEMA.into());
    }
    if let Some(v) = map.get("state") {
        if v.get("run").is_some() {
            required(v, &["scope", "run", "control"])?;
            scope_shape(&v["scope"])?;
            let s: DurableExecutionSnapshot = dto(v)?;
            s.control.validate().map_err(|_| SCHEMA.to_owned())?;
            if let Some(run) = &s.run {
                run.validate().map_err(|_| SCHEMA.to_owned())?;
            }
            return Ok(View::Execution(s));
        }
        return snapshot(v).map(View::Control);
    }
    if let Some(v) = map.get("outcome") {
        let o: DurableOutcome = dto(v)?;
        snapshot(&v["snapshot"])?;
        receipt(&v["receipt"])?;
        return Ok(View::Outcome(o));
    }
    if let Some(v) = map.get("receipt") {
        return if v.is_null() {
            Ok(View::Receipt(None))
        } else {
            receipt(v).map(|r| View::Receipt(Some(r)))
        };
    }
    Err(SCHEMA.into())
}
fn scope(out: &mut String, s: &DurableScope) {
    writeln!(
        out,
        "Session {} | lane {} | history revision {} | control revision {} | entry {}",
        quoted(&s.session_id),
        quoted(&s.lane),
        s.history_revision,
        s.control_revision,
        quoted(&s.entry_id)
    )
    .unwrap();
}
fn execution_control(out: &mut String, forgotten: usize, intent: Option<(&str, usize, u64, u64)>) {
    writeln!(
        out,
        "Forgotten business projections: {forgotten} (audit/get_result not erased)."
    )
    .unwrap();
    if let Some((run_id, unknown, started_seq, history_revision)) = intent {
        writeln!(out, "Recovery INTENT ONLY: explicit run {} | unknown invocations {} | started sequence {} | observed history revision {}. No execution.",
            quoted(run_id), unknown, started_seq, history_revision).unwrap();
    } else {
        out.push_str("Recovery intent: none recorded in this control view; no execution.\n");
    }
}
fn control(out: &mut String, s: &DurableSnapshot, summary: bool) {
    scope(out, &s.scope);
    if let Some(w) = &s.state.workflow {
        let counts = ["pending", "in_progress", "completed"].map(|status| {
            w.steps
                .iter()
                .filter(|step| serde_json::to_value(step.status).unwrap() == status)
                .count()
        });
        writeln!(
            out,
            "Workflow {} | status {} | steps {} (pending {}, in_progress {}, completed {})",
            quoted(&w.name),
            quoted(&w.status),
            w.steps.len(),
            counts[0],
            counts[1],
            counts[2]
        )
        .unwrap();
        if !summary {
            writeln!(out, "  reason {}", quoted(&w.reason)).unwrap();
            for step in &w.steps {
                writeln!(
                    out,
                    "  Step {} | {} | title {} | note {}",
                    quoted(&step.id),
                    quoted(&step.status),
                    quoted(&step.title),
                    quoted(&step.note)
                )
                .unwrap();
            }
        }
    } else {
        out.push_str("Workflow: none in this control view.\n");
    }
    out.push_str("Manual queue (not workers): ");
    for status in ["queued", "claimed", "completed", "failed", "cancelled"] {
        let count = s
            .state
            .queue
            .iter()
            .filter(|q| q.status.as_str() == status)
            .count();
        write!(out, "{status}={count} ").unwrap();
    }
    out.push('\n');
    if !summary {
        for q in &s.state.queue {
            writeln!(
                out,
                "  Item {} | {} | actor {} | text {} | note {} | revisions {}..{}",
                quoted(&q.id),
                quoted(&q.status),
                quoted(&q.claimed_by),
                quoted(&q.text),
                quoted(&q.note),
                q.created_revision,
                q.updated_revision
            )
            .unwrap();
        }
    }
    let c = &s.execution_control;
    execution_control(
        out,
        c.forgotten_run_ids.len(),
        c.recovery_request.as_ref().map(|r| {
            (
                r.run_id.as_str(),
                r.unknown_invocation_ids.len(),
                r.started_seq,
                r.observed_history_revision,
            )
        }),
    );
}
fn execution(out: &mut String, s: &DurableExecutionSnapshot, summary: bool) {
    scope(out, &s.scope);
    if let Some(r) = &s.run {
        let unknown = r
            .tools
            .iter()
            .filter(|t| serde_json::to_value(&t.status).unwrap() == "unknown")
            .count();
        writeln!(
            out,
            "Explicit run {} | status {} | tools {} | unknown outcomes {}",
            quoted(&r.id),
            quoted(&r.status),
            r.tools.len(),
            unknown
        )
        .unwrap();
        writeln!(out, "Native started sequence {} | finished record {} | finished sequence {} | final entry {} | assistant entry {}",
            r.started_seq, quoted(&r.finished_record_id), quoted(&r.finished_seq), quoted(&r.final_entry_id), quoted(&r.assistant_entry_id)).unwrap();
        if !summary {
            for t in &r.tools {
                writeln!(out, "  Invocation {} | callId {} | name {} | status {} | replay {} | result reference {} | reserved result {} | assistant entry {} | tool index {}",
                    quoted(&t.invocation_id), quoted(&t.call_id), quoted(&t.name), quoted(&t.status), quoted(&t.replay), quoted(&t.result_entry_id), quoted(&t.reserved_result_entry_id), quoted(&t.assistant_entry_id), t.tool_index).unwrap();
            }
        }
    } else {
        out.push_str("Explicit run: null, hidden business projection only; not a deletion or a latest-run lookup. No claim about missing native facts or whether operations occurred.\n");
    }
    out.push_str("Open != Interrupted: Interrupted requires a native committed finish record. Run completed != workflow completed.\n");
    let c = &s.control;
    execution_control(
        out,
        c.forgotten_run_ids.len(),
        c.recovery_request.as_ref().map(|r| {
            (
                r.run_id.as_str(),
                r.unknown_invocation_ids.len(),
                r.started_seq,
                r.observed_history_revision,
            )
        }),
    );
}
fn ack(out: &mut String, r: &DurableReceipt, kind: &str, summary: bool) {
    // Serialize the enum to get its stable wire action name, never Debug or the
    // whole action (which could contain arbitrarily large business text).
    let action = serde_json::to_value(&r.action).expect("action serializes");
    writeln!(out, "{kind}: action {} | requestId {} | actor {} | changed {} | control revision {} | entry {} | runId {}",
        quoted(&action["action"]), quoted(&r.request_id), quoted(&r.actor), r.changed, r.control_revision, quoted(&r.entry_id), quoted(&r.run_id)).unwrap();
    if !summary {
        writeln!(
            out,
            "Original guard (not a new guard): {}",
            quoted(&r.expected)
        )
        .unwrap();
    }
    match action["action"].as_str() {
        Some("recover") => out.push_str("recover: INTENT ONLY; no tools or provider execution.\n"),
        Some("forget_run") => out.push_str("forget_run: hides the business projection; does not erase native audit or get_result.\n"),
        Some("reset") => out.push_str("reset: workflow only; queue and execution journal retained.\n"),
        _ => {}
    }
}
fn format_view(view: &View, summary: bool) -> String {
    let mut out = String::from("Native durable v1\n");
    if summary {
        out.push_str(DETAILS);
    }
    match view {
        View::Control(s) => control(&mut out, s, summary),
        View::Execution(s) => execution(&mut out, s, summary),
        View::Outcome(o) => {
            ack(&mut out, &o.receipt, if o.replayed { "Deduplicated stored ACK (not a new write)" } else { "Mutation committed ACK" }, summary);
            out.push_str("Acknowledged mutation snapshot (not an additional status read):\n");
            control(&mut out, &o.snapshot, summary);
        }
        View::Receipt(Some(r)) => {
            ack(&mut out, r, "Stored ACK lookup (not a fresh commit)", summary);
            scope(&mut out, &r.expected);
        }
        View::Receipt(None) => out.push_str("No current-actor/current-branch receipt is available. This is not proof of no effect. Use the known business requestId and retain the original guard; do not retry unchecked arguments.\n"),
    }
    out.push_str(SAFETY);
    out
}
fn render(value: &Value) -> Result<String, String> {
    let view = decode(value)?;
    let full = format_view(&view, false);
    if full.len() <= MAX_TEXT_BYTES {
        return Ok(full);
    }
    let summary = format_view(&view, true);
    if summary.len() <= MAX_TEXT_BYTES {
        return Ok(summary);
    }
    // Atomic failure, never truncate a request ID, workflow name, or receipt.
    Err("Durable human view unavailable: even the explicit summary exceeds 16 KiB. A successful wire mutation may already be committed; inspect manual RPC v1 get_durable_request using the original business requestId and guard, or get_durable_state for details. Do not retry unchecked arguments.".into())
}
fn error_text(error: &Failure) -> String {
    let mut hint = String::new();
    if let Some(id) = error.request_id.as_deref().filter(|id| valid_identity(id)) {
        writeln!(hint, "Business requestId {}. Receipt lookup: /durable {{\"action\":\"status\",\"requestId\":{}}}", quoted(id), quoted(id)).unwrap();
    }
    if error.code == "commit_uncertain" {
        hint.push_str("Commit uncertain: do not assume uncommitted. Inspect the receipt using the known requestId and retain the original guard; do not retry unchecked arguments. A missing receipt is not proof of no effect.\n");
    } else if error.code == "conflict" {
        hint.push_str(
            "CAS conflict: read new state and make an explicit decision, not an automatic retry.\n",
        );
    }
    let text = format!(
        "Durable unavailable/error: code {} | message {}\n{hint}No empty state inferred.\n",
        quoted(&error.code),
        quoted(&error.message)
    );
    if text.len() <= MAX_TEXT_BYTES {
        text
    } else {
        let code = quoted(&error.code);
        let code = if code.len() <= 128 {
            code.as_str()
        } else {
            "\"oversize code omitted\""
        };
        format!("Durable unavailable/error: code {code} | message omitted because the diagnostic exceeds 16 KiB. No empty state inferred; commit may already have occurred. Do not retry unchecked arguments.\n{hint}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_harness::types::AgentHarnessOptions;
    use serde_json::json;

    async fn memory() -> AgentHarness {
        AgentHarness::create(AgentHarnessOptions::default())
            .await
            .unwrap()
    }
    fn sample_scope() -> DurableScope {
        DurableScope {
            session_id: "session-7".into(),
            lane: "main".into(),
            binding_token: "binding-9".into(),
            leaf_id: None,
            history_revision: 12,
            history_generation: 0,
            entry_id: None,
            control_revision: 0,
        }
    }
    fn sample() -> DurableSnapshot {
        dto(&json!({"scope":sample_scope(),"state":{"revision":0,"nextQueueId":0,"workflow":null,"queue":[]},"executionControl":{}})).unwrap()
    }
    fn wire(s: &DurableSnapshot) -> Value {
        json!({"version":1,"state":s})
    }
    async fn call(h: &AgentHarness, v: Value) -> Result<String, String> {
        interactive_command(h, &v.to_string()).await
    }
    #[tokio::test]
    async fn static_help_does_not_touch_harness_or_admission() {
        let h = memory().await;
        let before = super::super::service(&h).unwrap().snapshot().await.unwrap();
        for args in ["help", " ? ", "\n--help\t"] {
            assert_eq!(interactive_command(&h, args).await.unwrap(), HELP);
        }
        assert_eq!(
            before,
            super::super::service(&h).unwrap().snapshot().await.unwrap()
        );
        // Help still succeeds after the harness is closed (no service access).
        h.close().await.unwrap();
        assert_eq!(interactive_command(&h, "help").await.unwrap(), HELP);
    }
    #[tokio::test]
    async fn real_workflow_queue_stored_ack_reset_and_guard_are_unchanged() {
        let h = memory().await;
        let initial = super::super::command(&h, "").await.unwrap()["state"]["scope"].clone();
        let start = json!({"action":"start","requestId":"start-001","expected":initial,"name":"Review","steps":[{"id":"check","title":"Verify"}]});
        let first = call(&h, start.clone()).await.unwrap();
        assert!(first.contains("Mutation committed ACK"));
        assert!(first.contains("pending"));
        assert!(call(&h, start)
            .await
            .unwrap()
            .contains("Deduplicated stored ACK"));
        call(
            &h,
            json!({"action":"enqueue","requestId":"queue-001","text":"Review code"}),
        )
        .await
        .unwrap();
        let claimed = call(
            &h,
            json!({"action":"claim","requestId":"claim-001","item":"item-1"}),
        )
        .await
        .unwrap();
        assert!(claimed.contains("\"native-cli\""));
        call(&h, json!({"action":"finish","requestId":"finish-001","item":"item-1","status":"completed"})).await.unwrap();
        let reset = call(&h, json!({"action":"reset","requestId":"reset-001"}))
            .await
            .unwrap();
        assert!(reset.contains("reset: workflow only"));
        assert!(reset.contains("completed=1"));
        let lookup = call(&h, json!({"action":"status","requestId":"start-001"}))
            .await
            .unwrap();
        assert!(lookup.contains("Stored ACK lookup (not a fresh commit)"));
        let before = super::super::command(&h, "").await.unwrap();
        let conflict = call(
            &h,
            json!({"action":"reset","requestId":"stale-001","expected":initial}),
        )
        .await
        .unwrap_err();
        assert!(conflict.contains("CAS conflict"));
        assert!(call(
            &h,
            json!({"action":"status","runId":"run-42","requestId":"start-001"})
        )
        .await
        .is_err());
        assert_eq!(before, super::super::command(&h, "").await.unwrap());
        h.close().await.unwrap();
    }
    #[test]
    fn malformed_schemas_never_become_empty_state() {
        for v in [
            Value::Null,
            json!({}),
            json!({"version":2,"state":sample()}),
            json!({"version":1,"state":{}}),
            json!({"version":1,"state":{"scope":sample_scope(),"state":{},"executionControl":{}}}),
            json!({"version":1,"receipt":{}}),
            json!({"version":1,"other":null}),
        ] {
            assert!(render(&v).unwrap_err().contains("schema"));
        }
        assert!(render(&wire(&sample())).unwrap().contains("Workflow: none"));
    }
    #[test]
    fn six_native_run_statuses_and_three_tool_statuses_are_explicit() {
        for status in [
            "open",
            "interrupted",
            "completed",
            "aborted",
            "failed",
            "declined",
        ] {
            let terminal = status != "open";
            let tools: Vec<_> = ["unknown", "completed", "failed"].into_iter().enumerate().map(|(i, status)| json!({
                "invocationId":format!("invocation-{i}"),"assistantEntryId":"assistant-1","toolIndex":i,"callId":format!("call-{i}"),"name":"write","reservedResultEntryId":format!("result-{i}"),"resultEntryId":(status != "unknown").then(|| format!("result-{i}")),"status":status,"replay":if i == 0 { "safe" } else { "never" }
            })).collect();
            let s: DurableExecutionSnapshot = dto(&json!({"scope":sample_scope(),"control":{},"run":{
                "id":"run-42","startedSeq":1,"finishedRecordId":terminal.then_some("finished-1"),"finishedSeq":terminal.then_some(3),"status":status,"finalEntryId":terminal.then_some("final-1"),"assistantEntryId":null,"tools":tools
            }})).unwrap();
            let text = render(&json!({"version":1,"state":s})).unwrap();
            assert!(text.contains(&format!("status {}", quoted(&status))));
            for needle in [
                "unknown outcomes 1",
                "\"safe\"",
                "\"never\"",
                "not permission",
                "Open != Interrupted",
                "native committed finish record",
                "completed != workflow completed",
                "Invocation \"invocation-0\"",
                "callId \"call-0\"",
            ] {
                assert!(text.contains(needle), "{needle}");
            }
        }
    }
    #[test]
    fn recovery_intent_and_hidden_projection_never_infer_latest_or_execute() {
        let mut s = sample();
        s.execution_control.recovery_request = Some(dto(&json!({"runId":"explicit-run","startedSeq":1,"observedHistoryRevision":12,"unknownInvocationIds":["invocation-1"]})).unwrap());
        let text = render(&wire(&s)).unwrap();
        assert!(text.contains(
            "Recovery INTENT ONLY: explicit run \"explicit-run\" | unknown invocations 1"
        ));
        let e: DurableExecutionSnapshot = dto(&json!({"scope":sample_scope(),"run":null,"control":{"forgottenRunIds":["private-hidden-id"]}})).unwrap();
        let text = render(&json!({"version":1,"state":e})).unwrap();
        assert!(text.contains("hidden business projection only"));
        assert!(text.contains("not a deletion or a latest-run lookup"));
        assert!(text.contains("projections: 1"));
        assert!(!text.contains("private-hidden-id"));
    }
    #[test]
    fn huge_unicode_view_uses_atomic_summary_and_preserves_ack() {
        let mut s = sample();
        let name = "界".repeat(500);
        let steps: Vec<_> = (0..24).map(|i| json!({"id":format!("step-{i}"),"title":"文".repeat(1000),"status":"pending","note":"注".repeat(3995) + "\u{1b}\n\u{7f}\u{85}\u{9b}"})).collect();
        s.state.workflow =
            Some(dto(&json!({"name":name,"status":"running","steps":steps})).unwrap());
        let id = "r".repeat(128);
        let r = DurableReceipt {
            request_id: id.clone(),
            actor: "native-cli".into(),
            expected: sample_scope(),
            action: dto(&json!({"action":"start","name":name,"steps":[]})).unwrap(),
            run_id: None,
            entry_id: "entry-1".into(),
            control_revision: 0,
            changed: true,
        };
        for replayed in [false, true] {
            let value = json!({"version":1,"outcome":DurableOutcome { receipt: r.clone(), snapshot: s.clone(), replayed }});
            assert!(value.to_string().len() <= super::super::MAX_RESPONSE_BYTES);
            let text = render(&value).unwrap();
            assert!(text.len() <= MAX_TEXT_BYTES);
            assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
            for needle in [
                "SUMMARY",
                "ACK",
                &id,
                &name,
                "steps 24",
                "get_durable_state",
                "Session",
                "control revision",
            ] {
                assert!(text.contains(needle));
            }
        }
    }
    #[test]
    fn controls_are_quoted_and_error_hints_bounded() {
        let dangerous = "x\u{1b}\n\u{7f}\u{85}\u{9b}y";
        let mut s = sample();
        s.scope.session_id = dangerous.into();
        let text = render(&wire(&s)).unwrap();
        assert!(text.contains("\\u001b\\n\\u007f\\u0085\\u009b"));
        assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
        let e = Failure {
            code: "commit_uncertain".into(),
            message: dangerous.repeat(4000),
            request_id: Some("known-001".into()),
        };
        let text = error_text(&e);
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert!(text.contains(r#"/durable {"action":"status","requestId":"known-001"}"#));
        assert!(text.contains("do not assume uncommitted"));
        assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
        for code in ["execution_unavailable", "limit_exceeded", "read_failure"] {
            let text = error_text(&Failure {
                code: code.into(),
                message: dangerous.into(),
                request_id: Some("bad\nrequest".into()),
            });
            assert!(text.contains(code));
            assert!(!text.contains("Receipt lookup"));
            assert!(text.contains("No empty state inferred"));
        }
    }
    #[test]
    fn oversize_summary_fails_explicitly_without_partial_business_strings() {
        let mut s = sample();
        s.scope.session_id = "界".repeat(MAX_TEXT_BYTES);
        let error = render(&wire(&s)).unwrap_err();
        assert!(error.contains("even the explicit summary exceeds 16 KiB"));
        assert!(error.contains("may already be committed"));
        assert!(error.len() <= MAX_TEXT_BYTES);
        assert!(!error.contains('界'));
    }
    #[test]
    fn missing_receipt_is_not_proof_of_no_effect() {
        let text = render(&json!({"version":1,"receipt":null})).unwrap();
        for needle in [
            "current-actor/current-branch",
            "not proof of no effect",
            "known business requestId",
            "original guard",
        ] {
            assert!(text.contains(needle));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn faux_completed_read_and_forget_keep_result_without_extra_execution() {
        use rpi_ai::providers::faux::{FauxProvider, FauxScript};
        use rpi_harness::agent_harness::{AgentLane, HarnessRunOutcome};
        let provider = FauxProvider::new(FauxScript::new().with_text("synthetic result"));
        let h = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
        let finished = h.prompt_text("synthetic prompt", vec![]).await.unwrap();
        let audit = h.get_result(&finished.run_id).await.unwrap().unwrap();
        let query = json!({"action":"status","runId":finished.run_id});
        assert!(call(&h, query.clone())
            .await
            .unwrap()
            .contains("status \"completed\""));
        let forget = call(
            &h,
            json!({"action":"forget_run","runId":finished.run_id,"requestId":"forget-001"}),
        )
        .await
        .unwrap();
        assert!(forget.contains("does not erase native audit or get_result"));
        assert!(call(&h, query)
            .await
            .unwrap()
            .contains("hidden business projection only"));
        let after = h.get_result(&finished.run_id).await.unwrap().unwrap();
        assert_eq!(after.run_id, audit.run_id);
        match (after.outcome, audit.outcome) {
            (
                HarnessRunOutcome::Completed {
                    leaf_id: a,
                    final_entry_id: b,
                    ..
                },
                HarnessRunOutcome::Completed {
                    leaf_id: c,
                    final_entry_id: d,
                    ..
                },
            ) => assert_eq!((a, b), (c, d)),
            other => panic!("native result changed: {other:?}"),
        }
        assert_eq!(
            provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        h.close().await.unwrap();
    }
}
