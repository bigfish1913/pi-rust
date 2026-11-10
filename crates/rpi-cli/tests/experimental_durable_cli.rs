//! #49: CLI selection of the #48 experimental ledger writer. No provider,
//! migration, scheduler, extension, or tool execution is authorized here.
#[path = "experimental_durable_cli/fixture.rs"]
mod fixture;
use fixture::{assert_error, mutation, start_action, Fixture};
use rpi_harness::durable::experimental_ledger::CUSTOM_TYPE;
use serde_json::{json, Value};

const FLAG: &str = "--experimental-durable-epoch";
const EPOCH: &str = "cli-epoch_49.test";

fn experimental(metadata: &Value) {
    assert_eq!(metadata["version"], 1, "wire version must not change");
    assert_eq!(metadata["enabled"], true);
    assert_eq!(metadata["profile"], "native-durable-ledger-v2-experimental");
    assert_eq!(
        metadata["ledger"],
        json!({
            "version":2, "epochId":EPOCH, "experimental":true,
            "selection":"explicit_host_epoch", "automaticMigration":false
        })
    );
    assert_eq!(metadata["limits"]["receipts"], 256);
}

fn lookup(host: &mut fixture::Host, id: &str) -> Value {
    host.data("get_durable_request", json!({"version":1,"requestId":id}))["receipt"].clone()
}

#[test]
fn explicit_jsonl_metadata_guarded_writer_and_same_epoch_read_only_reopen() {
    let fixture = Fixture::new();
    let (mut host, ready) = fixture.start(true, &[FLAG, EPOCH, "--no-tools"]);
    experimental(&ready["durable"]);
    let initial = host.state();
    let noop_request = mutation("noop", initial["scope"].clone(), json!({"action":"reset"}));
    let noop = host.data("durable_mutate", noop_request.clone())["outcome"].clone();
    assert_eq!(noop["receipt"]["changed"], false);
    assert_eq!(noop["receipt"]["actor"], "native-cli");
    let replay = host.data("durable_mutate", noop_request.clone())["outcome"].clone();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["receipt"], noop["receipt"]);
    assert_eq!(replay["snapshot"], noop["snapshot"]);

    let start_request = mutation("start", host.state()["scope"].clone(), start_action());
    let started = host.data("durable_mutate", start_request.clone())["outcome"].clone();
    assert_eq!(started["replayed"], false);
    assert_eq!(started["receipt"]["changed"], true);
    assert_eq!(started["receipt"]["actor"], "native-cli");
    assert_eq!(
        started["snapshot"]["state"]["workflow"]["status"],
        "running"
    );
    let replay = host.data("durable_mutate", start_request.clone())["outcome"].clone();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["receipt"], started["receipt"]);
    assert_eq!(replay["snapshot"], started["snapshot"]);
    assert_error(&host.request("durable_mutate", noop_request), "conflict");
    // Same identity is not sufficient: the original action AND guard are required.
    let mut wrong = start_request.clone();
    wrong["action"]["name"] = json!("different");
    assert_error(&host.request("durable_mutate", wrong), "conflict");
    let mut wrong = start_request;
    wrong["expected"] = host.state()["scope"].clone();
    assert_error(&host.request("durable_mutate", wrong), "conflict");
    assert_eq!(lookup(&mut host, "noop"), noop["receipt"]);
    assert_eq!(lookup(&mut host, "start"), started["receipt"]);
    let saved = host.state();
    host.assert_idle();
    let path = fixture.session_file();
    drop(host); // acknowledged writer, not graceful-shutdown persistence
    let bytes = std::fs::read(&path).unwrap();
    let records: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let durable: Vec<_> = records
        .iter()
        .filter(|r| r["customType"] == CUSTOM_TYPE)
        .collect();
    assert_eq!(
        durable.len(),
        2,
        "retry/errors/lookup must not append receipts"
    );
    assert!(!records
        .iter()
        .any(|r| r["customType"] == "rpi.durable.control.v1"));

    let inline = format!("{FLAG}={EPOCH}");
    let (mut reopened, ready) = fixture.start(
        true,
        &[&inline, "--session", path.to_str().unwrap(), "--no-tools"],
    );
    experimental(&ready["durable"]);
    let restored = reopened.state();
    assert_eq!(restored["state"], saved["state"]);
    assert_eq!(restored["executionControl"], saved["executionControl"]);
    // Binding tokens are intentionally process-local; all other scope is stable.
    let mut old_scope = saved["scope"].clone();
    let mut new_scope = restored["scope"].clone();
    old_scope.as_object_mut().unwrap().remove("bindingToken");
    new_scope.as_object_mut().unwrap().remove("bindingToken");
    assert_eq!(new_scope, old_scope);
    assert_eq!(lookup(&mut reopened, "start"), started["receipt"]);
    assert_eq!(lookup(&mut reopened, "noop"), noop["receipt"]);
    reopened.assert_idle();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(reopened);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    fixture.assert_no_provider_calls();
}

#[test]
fn experimental_memory_supports_more_than_v1_receipt_budget() {
    let fixture = Fixture::new();
    let inline = format!("{FLAG}={EPOCH}");
    let (mut host, ready) = fixture.start(true, &[&inline, "--no-session", "--no-tools"]);
    experimental(&ready["durable"]);
    let mut first = Value::Null;
    for i in 0..65 {
        let outcome = host.mutate(&format!("noop-{i}"), json!({"action":"reset"}));
        assert_eq!(outcome["replayed"], false);
        assert_eq!(outcome["receipt"]["changed"], false);
        assert_eq!(outcome["receipt"]["actor"], "native-cli");
        if i == 0 {
            first = outcome["receipt"].clone();
        }
    }
    assert_eq!(lookup(&mut host, "noop-0"), first);
    host.assert_idle();
    drop(host);
    fixture.assert_no_sessions();
    fixture.assert_no_provider_calls();
}

#[test]
fn request_actor_cannot_override_the_trusted_native_cli_owner() {
    let fixture = Fixture::new();
    let (mut host, _) = fixture.start(true, &[FLAG, EPOCH, "--no-session", "--no-tools"]);
    let before = host.state();
    for actor in [json!("native-cli"), json!("owner"), json!({"owner":true})] {
        let mut request = mutation("forged", before["scope"].clone(), start_action());
        request["actor"] = actor;
        assert_error(&host.request("durable_mutate", request), "invalid_request");
        assert!(lookup(&mut host, "forged").is_null());
        assert_eq!(host.state(), before);
    }
    for field in ["epochId", "ledger", "nativeDurableActor"] {
        let mut request = mutation("forged-policy", before["scope"].clone(), start_action());
        request[field] = json!("another-policy");
        assert_error(&host.request("durable_mutate", request), "invalid_request");
        assert!(lookup(&mut host, "forged-policy").is_null());
        assert_eq!(host.state(), before);
    }
    let outcome = host.mutate("trusted", start_action());
    assert_eq!(outcome["receipt"]["actor"], "native-cli");
    host.assert_idle();
    drop(host);
    fixture.assert_no_provider_calls();
}

#[test]
fn wrong_epoch_and_v1_reopen_reject_before_ready_without_changing_jsonl() {
    let fixture = Fixture::new();
    let (mut host, _) = fixture.start(true, &[FLAG, EPOCH, "--no-tools"]);
    host.mutate("persisted", start_action());
    let path = fixture.session_file();
    drop(host);
    let before = std::fs::read(&path).unwrap();
    for flags in [
        vec![FLAG, "another-epoch", "--session", path.to_str().unwrap()],
        vec!["--session", path.to_str().unwrap()],
    ] {
        fixture.rejection(true, &flags, false);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    fixture.assert_no_provider_calls();
}

#[test]
fn v1_and_synthetic_legacy_reject_v2_selection_without_conversion() {
    let fixture = Fixture::new();
    let (mut host, _) = fixture.start(true, &["--no-tools"]);
    host.mutate("v1", start_action());
    let path = fixture.session_file();
    drop(host);
    let before = std::fs::read(&path).unwrap();
    fixture.rejection(
        true,
        &[FLAG, EPOCH, "--session", path.to_str().unwrap()],
        false,
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let legacy = fixture.root.path().join("legacy.jsonl");
    fixture.synthetic_legacy(&legacy);
    let before = std::fs::read(&legacy).unwrap();
    fixture.rejection(
        true,
        &[FLAG, EPOCH, "--session", legacy.to_str().unwrap()],
        false,
    );
    assert_eq!(std::fs::read(&legacy).unwrap(), before);
    fixture.assert_no_provider_calls();
}

#[test]
fn synthetic_mixed_legacy_and_v2_reject_without_repair() {
    let fixture = Fixture::new();
    let (mut host, _) = fixture.start(true, &[FLAG, EPOCH, "--no-tools"]);
    host.mutate("v2", start_action());
    let path = fixture.session_file();
    drop(host);
    // Test-owned corruption only: retain the real committed v2 receipt and append
    // a storage-valid legacy custom entry in the same selected lane.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut records: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut legacy = records
        .iter()
        .find(|v| v["customType"] == CUSTOM_TYPE)
        .unwrap()
        .clone();
    let seq = records
        .iter()
        .filter_map(|v| v["seq"].as_u64())
        .max()
        .unwrap();
    let parent = legacy["id"].clone();
    legacy["seq"] = json!(seq + 1);
    legacy["id"] = json!("synthetic-mixed-legacy");
    legacy["parentId"] = parent;
    legacy["customType"] = json!("durable-state");
    legacy["data"] = json!({"synthetic":true});
    records.push(legacy);
    let mixed = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, mixed.as_bytes()).unwrap();
    let before = std::fs::read(&path).unwrap();
    fixture.rejection(
        true,
        &[FLAG, EPOCH, "--session", path.to_str().unwrap()],
        false,
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    fixture.assert_no_provider_calls();
}

#[test]
fn v1_and_disabled_metadata_are_exactly_the_previous_profile() {
    let fixture = Fixture::new();
    let (mut disabled, disabled_ready) = fixture.start(false, &["--no-session", "--no-tools"]);
    let metadata = disabled_ready["durable"].clone();
    assert_eq!(
        metadata,
        json!({
            "version":1, "profile":"native-durable-v1", "enabled":false,
            "commands":["get_durable_state","durable_mutate","get_durable_request","get_durable_runs"],
            "scope":"current_lane", "execution":"explicit_run",
            "executionPages":{"version":1,"command":"get_durable_runs",
                "cursor":"scope_bound_exclusive","maxRecords":64,"automatic":false},
            "scheduler":false,
            "modelContext":{"version":1,"delivery":"per_request_ephemeral_user",
                "oversize":"explicit_summary","failure":"unavailable_observation"},
            "prototype":true,
            "limits":{"inFlight":8,"requestBytes":65536,"responseBytes":491520,
                "contextBytes":16384,"idBytes":4096,"receipts":64,"recordBytes":1048576}
        })
    );
    assert_eq!(metadata["limits"]["receipts"], 64);
    assert!(metadata.get("ledger").is_none());
    assert_error(
        &disabled.request("get_durable_state", json!({"version":1})),
        "disabled",
    );
    drop(disabled);
    let (mut enabled, ready) = fixture.start(true, &["--no-session", "--no-tools"]);
    let mut comparable = ready["durable"].clone();
    comparable["enabled"] = json!(false);
    assert_eq!(comparable, metadata);
    assert_eq!(
        enabled.mutate("v1", start_action())["receipt"]["actor"],
        "native-cli"
    );
    enabled.assert_idle();
    drop(enabled);
    fixture.assert_no_sessions();
    fixture.assert_no_provider_calls();
}

#[test]
fn invalid_epoch_selection_is_usage_error_before_any_session_creation() {
    let invalid = "do-not-echo-private/epoch";
    let too_long = "x".repeat(129);
    for (enabled, args) in [
        (true, vec![FLAG]),
        (true, vec!["--experimental-durable-epoch="]),
        (true, vec![FLAG, ""]),
        (true, vec![FLAG, " "]),
        (true, vec![FLAG, "非ASCII"]),
        (true, vec![FLAG, invalid]),
        (true, vec![FLAG, too_long.as_str()]),
        (true, vec![FLAG, EPOCH, FLAG, EPOCH]),
        (
            true,
            vec![FLAG, EPOCH, "--experimental-durable-epoch=second"],
        ),
        (false, vec![FLAG, EPOCH]),
    ] {
        let fixture = Fixture::new();
        let stderr = fixture.rejection(enabled, &args, true);
        assert!(stderr.contains(FLAG), "missing flag diagnostic: {stderr}");
        assert!(
            !stderr.contains(invalid),
            "diagnostic echoed untrusted epoch"
        );
        assert!(
            !stderr.contains("非ASCII"),
            "diagnostic echoed invalid epoch"
        );
        assert!(
            !stderr.contains(&too_long),
            "diagnostic echoed oversize epoch"
        );
        fixture.assert_no_sessions();
    }
}

#[test]
fn safe_early_dispatch_rejections_do_not_read_or_convert_test_owned_files() {
    let fixture = Fixture::new();
    let legacy = fixture.root.path().join("saved.jsonl");
    fixture.synthetic_legacy(&legacy);
    let before = std::fs::read(&legacy).unwrap();
    let remote = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    remote.set_nonblocking(true).unwrap();
    let address = remote.local_addr().unwrap().to_string();
    let stderr = fixture.raw_rejection(
        &["--connect", &address, "--native-durable", FLAG, EPOCH],
        true,
    );
    assert!(stderr.contains(FLAG), "{stderr}");
    assert!(
        matches!(remote.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "early rejection contacted the controlled remote endpoint"
    );
    // Parent inspected the early guard and its passing unit tests before adding
    // these probes. They must exit before dispatch, leases or credential loading.
    for args in [
        vec!["--server", "--native-durable", FLAG, EPOCH],
        vec!["--web", "--native-durable", FLAG, EPOCH],
        vec!["dev", "--native-durable", FLAG, EPOCH],
        vec!["resource", "list", "--native-durable", FLAG, EPOCH],
        vec!["auth", "--native-durable", FLAG, EPOCH],
        vec!["--resource-capabilities", "--native-durable", FLAG, EPOCH],
        vec![
            "--resource-session-read",
            legacy.to_str().unwrap(),
            "--native-durable",
            FLAG,
            EPOCH,
        ],
    ] {
        let stderr = fixture.raw_rejection(&args, true);
        assert!(stderr.contains(FLAG), "{stderr}");
        assert_eq!(std::fs::read(&legacy).unwrap(), before);
        fixture.assert_no_sessions();
        assert!(!fixture.root.path().join("project/synthetic-store").exists());
    }
}
