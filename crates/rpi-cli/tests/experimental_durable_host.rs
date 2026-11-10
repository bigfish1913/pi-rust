//! #50: experimental v2 across real RPC navigation, normal exit and exact
//! resource boundaries. No provider, plugin, migration or new wire action.
#[allow(dead_code)]
#[path = "experimental_durable_cli/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "experimental_durable_host/process.rs"]
mod process;
use fixture::{assert_error, mutation, start_action, Fixture};
use process::Host;
use rpi_harness::durable::experimental_ledger::CUSTOM_TYPE;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, process::Command};

const FLAG: &str = "--experimental-durable-epoch";
const EPOCH: &str = "host-epoch_50.test";

fn command(f: &Fixture, extra: &[&str]) -> Command {
    let mut c = f.command(
        true,
        &[
            FLAG,
            EPOCH,
            "--package-only",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--no-tools",
        ],
    );
    c.args(extra);
    c
}
fn ready(c: &mut Command) -> (Host, Value) {
    let mut h = Host::spawn(c);
    let v = h.until(|v| v["type"] == "ready");
    assert_eq!(v["durable"]["version"], 1);
    assert_eq!(v["durable"]["ledger"]["version"], 2);
    assert_eq!(v["durable"]["ledger"]["epochId"], EPOCH);
    assert_eq!(v["durable"]["ledger"]["automaticMigration"], false);
    assert_eq!(v["durable"]["limits"]["receipts"], 256);
    (h, v)
}
fn lookup(h: &mut Host, id: &str) -> Value {
    h.data("get_durable_request", json!({"version":1,"requestId":id}))["receipt"].clone()
}
fn ledger(h: &mut Host) -> Value {
    h.data("get_session_branch", json!({"customTypes":[CUSTOM_TYPE]}))
}
// All logical session files, not just the transcript projection. Runtime lease
// metadata is intentionally excluded because reopening advances its fence.
fn bytes(f: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(p: PathBuf, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if p.is_file() {
            if p.extension().and_then(|ext| ext.to_str()) != Some("lock") {
                out.insert(p.clone(), std::fs::read(p).unwrap());
            }
        } else if p.is_dir() {
            for e in std::fs::read_dir(p).unwrap() {
                walk(e.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(f.root.path().join("sessions"), &mut out);
    assert!(!out.is_empty());
    out
}

#[test]
fn normal_eof_and_stop_reopen_the_acknowledged_v2_state_and_receipts() {
    for stop in [false, true] {
        let f = Fixture::new();
        let (mut h, _) = ready(&mut command(&f, &[]));
        let started = h.mutate("exit-start", start_action());
        let noop = h.mutate("exit-noop", start_action());
        assert_eq!(noop["receipt"]["changed"], false);
        assert_eq!(ledger(&mut h)["count"], 2);
        let saved = h.state();
        h.assert_idle();
        h.finish(stop); // EOF AND successful exit, not drop/kill after ACK.
        let path = f.session_file();
        let before = bytes(&f);
        let (mut h, _) = ready(&mut command(&f, &["--session", path.to_str().unwrap()]));
        let restored = h.state();
        assert_eq!(restored["state"], saved["state"]);
        assert_eq!(restored["executionControl"], saved["executionControl"]);
        assert_eq!(
            restored["scope"]["controlRevision"],
            saved["scope"]["controlRevision"]
        );
        assert_eq!(lookup(&mut h, "exit-start"), started["receipt"]);
        assert_eq!(lookup(&mut h, "exit-noop"), noop["receipt"]);
        h.assert_idle();
        h.finish(stop);
        assert_eq!(
            bytes(&f),
            before,
            "read-only reopen/normal exit rewrote files"
        );
        f.assert_no_provider_calls();
    }
}

#[test]
fn rpc_navigation_projects_the_selected_branch_and_rejects_old_scope() {
    for memory in [false, true] {
        let f = Fixture::new();
        let extra = if memory { vec!["--no-session"] } else { vec![] };
        let (mut h, _) = ready(&mut command(&f, &extra));
        let started = h.mutate("branch-start", start_action());
        let target = started["receipt"]["entryId"].as_str().unwrap();
        let old = h.state();
        let reset = h.mutate("branch-reset", json!({"action":"reset"}));
        assert!(h.state()["state"]["workflow"].is_null());
        assert_eq!(
            h.data("navigate", json!({"targetId":target}))["outcome"],
            "completed"
        );
        assert_eq!(h.state()["state"], started["snapshot"]["state"]);
        assert_eq!(lookup(&mut h, "branch-start"), started["receipt"]);
        assert!(lookup(&mut h, "branch-reset").is_null());
        let before = ledger(&mut h);
        assert_eq!(before["count"], 1);
        assert_error(
            &h.request(
                "durable_mutate",
                mutation("stale", old["scope"].clone(), json!({"action":"reset"})),
            ),
            "conflict",
        );
        assert_eq!(
            ledger(&mut h),
            before,
            "stale navigation guard appended a receipt"
        );
        assert!(lookup(&mut h, "stale").is_null());
        let branch = h.mutate(
            "branch-new",
            json!({"action":"enqueue","text":"selected branch"}),
        );
        assert_eq!(branch["receipt"]["actor"], "native-cli");
        assert_eq!(ledger(&mut h)["count"], 2);
        assert_eq!(
            h.data("navigate", json!({"targetId":reset["receipt"]["entryId"]}))["outcome"],
            "completed"
        );
        assert!(h.state()["state"]["workflow"].is_null());
        assert!(lookup(&mut h, "branch-new").is_null());
        assert_eq!(lookup(&mut h, "branch-reset"), reset["receipt"]);
        assert_eq!(ledger(&mut h)["count"], 2);
        h.finish(true);
        if memory {
            f.assert_no_sessions();
        }
        f.assert_no_provider_calls();
    }
}

#[test]
fn rpc_navigation_rejects_legacy_v1_and_malformed_v2_candidates_without_moving_leaf() {
    for kind in ["durable-state", "rpi.durable.control.v1", CUSTOM_TYPE] {
        let f = Fixture::new();
        let mut c = f.command(
            false,
            &[
                "--package-only",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--no-prompt-templates",
                "--no-tools",
            ],
        );
        let mut seed = Host::spawn(&mut c);
        seed.until(|v| v["type"] == "ready");
        let good = seed.data(
            "append_entry",
            json!({"customType":"test.safe.marker","data":{}}),
        );
        // append_entry correctly reserves native journal types even flag-off.
        // Seed an ordinary sibling, then corrupt only its test-owned on-disk
        // type after successful shutdown; this is not a production bypass.
        let bad = seed.data(
            "append_entry",
            json!({"customType":"test.bad.candidate","data":{}}),
        );
        assert_eq!(
            seed.data("navigate", json!({"targetId":good["entryId"]}))["outcome"],
            "completed"
        );
        seed.finish(true);
        let path = f.session_file();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut changed = 0;
        let mut rewritten = String::new();
        for line in text.lines() {
            let mut record: Value = serde_json::from_str(line).unwrap();
            if record["id"] == bad["entryId"] {
                assert_eq!(record["customType"], "test.bad.candidate");
                record["customType"] = json!(kind);
                changed += 1;
            }
            rewritten.push_str(&record.to_string());
            rewritten.push('\n');
        }
        assert_eq!(changed, 1);
        std::fs::write(&path, rewritten).unwrap();
        // Compare closed-session logical bytes; the runtime lease metadata is
        // intentionally excluded by `bytes`.
        let on_disk = bytes(&f);
        let (mut h, _) = ready(&mut command(&f, &["--session", path.to_str().unwrap()]));
        let before = h.state();
        let result = h.request("navigate", json!({"targetId":bad["entryId"]}));
        assert!(
            result["success"] == false || result["data"]["outcome"] == "failed",
            "{result}"
        );
        assert_eq!(
            h.state(),
            before,
            "rejected candidate changed the published scope/state"
        );
        assert_eq!(ledger(&mut h)["count"], 0);
        h.finish(true);
        assert_eq!(
            bytes(&f),
            on_disk,
            "rejected candidate or normal exit wrote files"
        );
        f.assert_no_provider_calls();
    }
}

fn exact(f: &Fixture, selection: Value) -> PathBuf {
    let path = f.root.path().join("project/exact.json");
    std::fs::write(&path, json!({"version":1,"tools":selection}).to_string()).unwrap();
    path
}
fn exact_command(f: &Fixture, config: &std::path::Path, extra: &[&str]) -> Command {
    let mut c = f.command(
        true,
        &[
            FLAG,
            EPOCH,
            "--approve",
            "--resource-config",
            config.to_str().unwrap(),
        ],
    );
    c.args(extra);
    c
}

#[test]
fn exact_tool_selection_and_slash_rpc_share_the_same_v2_service() {
    for (selection, expected) in [
        (
            json!({"mode":"only","names":["durable"]}),
            json!(["durable"]),
        ),
        (json!({"mode":"none"}), json!([])),
        (json!({"mode":"only","names":["read"]}), json!(["read"])),
    ] {
        let f = Fixture::new();
        let config = exact(&f, selection);
        let (mut h, event) = ready(&mut exact_command(&f, &config, &["--no-session"]));
        assert_eq!(event["resourceState"]["enabled"], true);
        let tools = h.data("get_available_tools", json!({}));
        assert_eq!(tools["activeTools"], expected, "{tools}");
        assert_eq!(
            tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| *t == "durable")
                .count(),
            1,
            "{tools}"
        );
        let commands = h.data("get_commands", json!({}));
        assert_eq!(
            commands["commands"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["name"] == "durable")
                .count(),
            1
        );
        let scope = h.state()["scope"].clone();
        let payload =
            json!({"action":"enqueue","requestId":"slash","text":"same core","expected":scope});
        let via_slash = h.data(
            "execute_command",
            json!({"command":"/durable","args":payload.to_string()}),
        );
        assert_eq!(via_slash["version"], 1);
        let receipt = via_slash["outcome"]["receipt"].clone();
        assert_eq!(receipt["actor"], "native-cli");
        assert_eq!(lookup(&mut h, "slash"), receipt);
        assert_eq!(h.state(), via_slash["outcome"]["snapshot"]);
        let action = json!({"action":"enqueue","text":"same core"});
        let replay = h.data("durable_mutate", mutation("slash", scope, action));
        assert_eq!(replay["outcome"]["replayed"], true);
        assert_eq!(replay["outcome"]["receipt"], receipt);
        assert_eq!(ledger(&mut h)["count"], 1);
        let status = h.data("execute_command", json!({"command":"durable"}));
        assert_eq!(status["state"], h.state());
        h.assert_idle();
        h.finish(true);
        f.assert_no_sessions();
        f.assert_no_provider_calls();
    }
}

#[test]
fn exact_bound_jsonl_reopen_restores_both_resource_fingerprint_and_v2_receipt() {
    let f = Fixture::new();
    let config = exact(&f, json!({"mode":"only","names":["durable"]}));
    let (mut h, event) = ready(&mut exact_command(&f, &config, &[]));
    let fingerprint = event["resourceState"]["binding"]["fingerprint"].clone();
    assert_eq!(fingerprint.as_str().unwrap().len(), 64);
    let outcome = h.mutate("bound", start_action());
    assert_eq!(ledger(&mut h)["count"], 1);
    h.finish(true);
    let path = f.session_file();
    let before = bytes(&f);
    let (mut h, event) = ready(&mut exact_command(
        &f,
        &config,
        &["--session", path.to_str().unwrap()],
    ));
    assert_eq!(
        event["resourceState"]["binding"]["fingerprint"],
        fingerprint
    );
    assert_eq!(lookup(&mut h, "bound"), outcome["receipt"]);
    assert_eq!(h.state()["state"], outcome["snapshot"]["state"]);
    assert_eq!(ledger(&mut h)["count"], 1);
    h.finish(false);
    assert_eq!(bytes(&f), before);
    f.assert_no_provider_calls();
}

#[test]
fn exact_bound_wrong_epoch_missing_or_changed_selection_reject_without_rewriting() {
    let f = Fixture::new();
    let config = exact(&f, json!({"mode":"only","names":["durable"]}));
    let (mut h, _) = ready(&mut exact_command(&f, &config, &[]));
    let outcome = h.mutate("bound", start_action());
    h.finish(true);
    let path = f.session_file();
    let before = bytes(&f);
    let mut wrong_epoch = f.command(
        true,
        &[
            FLAG,
            "wrong-epoch",
            "--approve",
            "--resource-config",
            config.to_str().unwrap(),
            "--session",
            path.to_str().unwrap(),
        ],
    );
    assert!(!Host::spawn(&mut wrong_epoch).reject().is_empty());
    assert_eq!(bytes(&f), before);
    assert!(
        !Host::spawn(&mut command(&f, &["--session", path.to_str().unwrap()]))
            .reject()
            .is_empty()
    );
    assert_eq!(bytes(&f), before);
    let original = std::fs::read(&config).unwrap();
    exact(&f, json!({"mode":"none"}));
    let error = Host::spawn(&mut exact_command(
        &f,
        &config,
        &["--session", path.to_str().unwrap()],
    ))
    .reject();
    assert!(
        error.contains("session resource selection differs"),
        "{error}"
    );
    assert_eq!(bytes(&f), before);
    std::fs::write(&config, original).unwrap();
    let (mut h, _) = ready(&mut exact_command(
        &f,
        &config,
        &["--session", path.to_str().unwrap()],
    ));
    assert_eq!(lookup(&mut h, "bound"), outcome["receipt"]);
    h.finish(true);
    assert_eq!(bytes(&f), before);
    f.assert_no_provider_calls();
}
