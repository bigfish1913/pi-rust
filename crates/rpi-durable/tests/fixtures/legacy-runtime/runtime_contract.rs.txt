use rpi_plugin_sdk::{
    callbacks::free_string, EventHandlerFn, EventTag, FreeStringFn, PluginApi, RuntimeActionId,
    StablePluginEvent, StableToolSchema, StbString, StbStringRef, StepResultTag, ToolCancelFn,
    ToolDestroyFn, ToolExecuteFn, ToolPartialCb, ToolPollFn,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Copy)]
struct ToolFns {
    execute: ToolExecuteFn,
    poll: ToolPollFn,
    destroy: ToolDestroyFn,
    free: FreeStringFn,
}
#[derive(Default)]
struct Host {
    entries: Vec<Value>,
    next_id: u32,
    fail_read: bool,
    reject_filter: bool,
    branch_reply: Option<Value>,
    read_requests: Vec<Value>,
    fail_append: bool,
    missing_ack: bool,
    invalid_json_ack: bool,
    read_delay: bool,
    block_read: bool,
    tool: Option<ToolFns>,
    events: Vec<(EventTag, EventHandlerFn)>,
}
static HOST: OnceLock<Mutex<Host>> = OnceLock::new();
static READ_STARTED: AtomicBool = AtomicBool::new(false);
static RELEASE_READ: AtomicBool = AtomicBool::new(false);
fn host() -> &'static Mutex<Host> {
    HOST.get_or_init(|| Mutex::new(Host::default()))
}

extern "C" fn register_tool(
    _: *const StableToolSchema,
    execute: ToolExecuteFn,
    poll: ToolPollFn,
    _: ToolCancelFn,
    destroy: ToolDestroyFn,
    free: FreeStringFn,
) -> i32 {
    host().lock().unwrap().tool = Some(ToolFns {
        execute,
        poll,
        destroy,
        free,
    });
    0
}
extern "C" fn register_command(
    _: StbStringRef,
    _: StbStringRef,
    _: rpi_plugin_sdk::CommandHandlerFn,
) -> i32 {
    0
}
extern "C" fn register_event(tag: EventTag, handler: EventHandlerFn, _: *mut c_void) -> i32 {
    host().lock().unwrap().events.push((tag, handler));
    0
}
extern "C" fn runtime(
    action: u32,
    input: StbStringRef,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    // The host owns this output allocation and the plugin frees it through `free_string`.
    let args = StbString {
        ptr: input.ptr as *mut _,
        len: input.len,
    }
    .to_string_lossy();
    let value: Value = serde_json::from_str(&args).unwrap();
    let mut host = host().lock().unwrap();
    if action == RuntimeActionId::GetSessionBranch as u32 {
        assert_eq!(value, json!({"customTypes":["durable-state"]}));
        host.read_requests.push(value.clone());
    }
    let read_delay = action == RuntimeActionId::GetSessionBranch as u32 && host.read_delay;
    if read_delay {
        std::thread::sleep(Duration::from_millis(100));
    }
    let block_read = action == RuntimeActionId::GetSessionBranch as u32 && host.block_read;
    if block_read {
        READ_STARTED.store(true, Ordering::Release);
        while !RELEASE_READ.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }
    let mut invalid_json_ack = false;
    let (rc, result) = match RuntimeActionId::try_from(action).unwrap() {
        RuntimeActionId::GetSessionBranch if host.reject_filter => (
            1,
            json!({"error":"unsupported GetSessionBranch customTypes"}),
        ),
        RuntimeActionId::GetSessionBranch if host.fail_read => (1, json!({"error":"read failed"})),
        RuntimeActionId::GetSessionBranch => {
            let entries: Vec<_> = host
                .entries
                .iter()
                .filter(|entry| entry["type"] == "custom" && entry["customType"] == "durable-state")
                .cloned()
                .collect();
            (
                0,
                host.branch_reply
                    .clone()
                    .unwrap_or_else(|| json!({"entries":entries,"filtered":true})),
            )
        }
        RuntimeActionId::AppendEntry if host.fail_append => {
            host.fail_append = false;
            (1, json!({"error":"append failed"}))
        }
        RuntimeActionId::AppendEntry => {
            host.next_id += 1;
            let id = format!("entry-{}", host.next_id);
            host.entries.push(
                json!({"id":id,"type":"custom","customType":"durable-state","data":value["data"]}),
            );
            invalid_json_ack = std::mem::take(&mut host.invalid_json_ack);
            let result = if host.missing_ack {
                host.missing_ack = false;
                json!({})
            } else {
                json!({"entryId":id})
            };
            (0, result)
        }
        other => panic!("unexpected runtime action: {other:?}"),
    };
    // SAFETY: the plugin supplies a live output slot for this synchronous call.
    // Ownership of the allocation transfers to it with the host free callback.
    unsafe {
        *out = StbString::from_string(if invalid_json_ack {
            "not-json".into()
        } else {
            result.to_string()
        })
    };
    rc
}

fn dispatch(event: StablePluginEvent) -> i32 {
    let handlers = host().lock().unwrap().events.clone();
    handlers
        .into_iter()
        .filter(|(tag, _)| *tag == event.tag)
        .map(|(_, handler)| handler(event, std::ptr::null_mut()))
        .find(|rc| *rc != 0)
        .unwrap_or(0)
}
fn empty(tag: EventTag) -> i32 {
    dispatch(StablePluginEvent::empty(tag))
}
fn action(params: Value) -> Result<Value, String> {
    let tool = host().lock().unwrap().tool.unwrap();
    let handle = (tool.execute)(
        StbStringRef::from_str("contract"),
        StbString::from_string(params.to_string()),
        Some(free_string),
    );
    let result = (tool.poll)(handle, None::<ToolPartialCb>, std::ptr::null_mut());
    // SAFETY: each branch reads only the union member selected by its tag;
    // the plugin's free callback releases that owned string exactly once.
    let answer = match result.tag {
        StepResultTag::Done => unsafe {
            let value = result.done_payload().result;
            let text = value.to_string_lossy();
            (tool.free)(value);
            Ok(serde_json::from_str::<Value>(&text).unwrap())
        },
        StepResultTag::Err => unsafe {
            let value = result.err_payload().message;
            let text = value.to_string_lossy();
            (tool.free)(value);
            Err(text)
        },
        StepResultTag::Pending => panic!("durable tool unexpectedly pending"),
    };
    (tool.destroy)(handle);
    answer
}
fn canonical_snapshot() -> Value {
    json!({
        "id":"snapshot", "type":"custom", "customType":"durable-state",
        "data":{
            "format":"rpi-durable-journal", "version":1, "kind":"snapshot",
            "baseRevision":0, "revision":0,
            "state":{
                "workflow":{
                    "name":"canonical loaded before SessionStart", "status":"running",
                    "steps":[{"id":"s","title":"S","status":"pending"}]
                },
                "run":{
                    "id":"run-1", "status":"running",
                    "startedRevision":0, "updatedRevision":0, "tools":[]
                },
                "nextRunId":1
            }
        }
    })
}
fn legacy() -> Value {
    json!({"id":"legacy","type":"custom","customType":"durable-state","data":{"workflow":null}})
}

fn assert_restore_failure(expected: &str) {
    let before_entries = host().lock().unwrap().entries.clone();
    let before_reads = host().lock().unwrap().read_requests.len();
    assert_ne!(empty(EventTag::SessionTree), 0);
    assert_eq!(host().lock().unwrap().read_requests.len(), before_reads + 1);
    for params in [
        json!({"action":"status"}),
        json!({"action":"enqueue","text":"blocked"}),
    ] {
        let result = action(params).unwrap();
        let error = result["details"]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(result["details"].get("workflow").is_none());
    }
    assert_ne!(empty(EventTag::AgentStart), 0);
    assert_eq!(host().lock().unwrap().entries, before_entries);
    // Failed stores never lazily retry, let alone retry without the filter.
    assert_eq!(host().lock().unwrap().read_requests.len(), before_reads + 1);
}

#[test]
fn runtime_contract_is_sequential_and_fail_closed() {
    let api = PluginApi {
        abi_version: rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED,
        struct_size: std::mem::size_of::<PluginApi>() as u32,
        free_string,
        register_tool: Some(register_tool),
        register_command: Some(register_command),
        register_shortcut: None,
        register_flag: None,
        register_provider: None,
        register_message_renderer: None,
        register_markdown_transformer: None,
        register_entry_renderer: None,
        register_event_handler: Some(register_event),
        register_resources_discover: None,
        runtime_action: runtime,
        dispatch_event: None,
        user_data: std::ptr::null_mut(),
        declare: None,
        register_before_agent_start: None,
        register_oauth_provider: None,
    };
    assert_eq!(rpi_durable::rpi_plugin_register(&api), 0);

    host().lock().unwrap().entries = vec![canonical_snapshot()];
    host().lock().unwrap().read_delay = true;
    let first = std::thread::spawn(|| action(json!({"action":"status"})).unwrap());
    let second = std::thread::spawn(|| action(json!({"action":"status"})).unwrap());
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    host().lock().unwrap().read_delay = false;
    assert_eq!(first["details"]["run"]["status"], "interrupted");
    assert_eq!(second["details"]["run"]["status"], "interrupted");
    assert_eq!(host().lock().unwrap().entries.len(), 2);

    assert_eq!(
        action(json!({"action":"status"})).unwrap()["details"]["workflow"]["name"],
        "canonical loaded before SessionStart"
    );
    assert_eq!(host().lock().unwrap().entries.len(), 2);
    assert_eq!(empty(EventTag::SessionTree), 0);
    action(json!({"action":"enqueue","text":"persist"})).unwrap();

    host().lock().unwrap().fail_read = true;
    assert_ne!(empty(EventTag::SessionTree), 0);
    let before_blocked = host().lock().unwrap().entries.clone();
    let blocked = action(json!({"action":"enqueue","text":"blocked"})).unwrap();
    assert!(blocked["details"]["error"].is_string());
    assert_eq!(host().lock().unwrap().entries, before_blocked);
    host().lock().unwrap().fail_read = false;
    host().lock().unwrap().entries.clear();
    assert_eq!(empty(EventTag::SessionTree), 0);

    let before_failure = action(json!({"action":"status"})).unwrap();
    let before_failure_entries = host().lock().unwrap().entries.clone();
    host().lock().unwrap().fail_append = true;
    assert!(action(json!({"action":"enqueue","text":"retry"})).is_err());
    assert_eq!(action(json!({"action":"status"})).unwrap(), before_failure);
    assert_eq!(host().lock().unwrap().entries, before_failure_entries);
    action(json!({"action":"enqueue","text":"retry"})).unwrap();
    host().lock().unwrap().missing_ack = true;
    assert!(action(json!({"action":"enqueue","text":"ack"})).is_err());
    let acknowledged_unknown = host().lock().unwrap().entries.clone();
    let no_retry = action(json!({"action":"enqueue","text":"no blind retry"})).unwrap();
    assert!(no_retry["details"]["error"].is_string());
    assert_eq!(host().lock().unwrap().entries, acknowledged_unknown);
    assert_eq!(empty(EventTag::SessionTree), 0);
    let recovered = action(json!({"action":"status"})).unwrap();
    assert_eq!(recovered["details"]["queue"][1]["text"], "ack");
    action(json!({"action":"enqueue","text":"after reconciliation"})).unwrap();
    host().lock().unwrap().invalid_json_ack = true;
    assert!(action(json!({"action":"enqueue","text":"committed with malformed ack"})).is_err());
    let malformed_ack_entries = host().lock().unwrap().entries.clone();
    let blocked =
        action(json!({"action":"enqueue","text":"must block after malformed ack"})).unwrap();
    assert!(blocked["details"]["error"].is_string());
    assert_eq!(host().lock().unwrap().entries, malformed_ack_entries);
    assert_eq!(empty(EventTag::SessionTree), 0);
    let recovered = action(json!({"action":"status"})).unwrap();
    assert_eq!(
        recovered["details"]["queue"][3]["text"],
        "committed with malformed ack"
    );

    let mut corrupt = host().lock().unwrap().entries.clone();
    corrupt.last_mut().unwrap()["data"]["version"] = json!(999);
    host().lock().unwrap().entries = corrupt;
    assert_ne!(empty(EventTag::SessionTree), 0);
    let corrupt_failed = action(json!({"action":"enqueue","text":"corrupt"})).unwrap();
    assert!(corrupt_failed["details"]["error"].is_string());

    // Legacy snapshots no longer initialize or upgrade the branch.
    host().lock().unwrap().entries = vec![legacy()];
    assert_restore_failure("invalid durable journal record");
    host().lock().unwrap().entries.clear();
    assert_eq!(empty(EventTag::SessionTree), 0);
    action(json!({"action":"enqueue","text":"canonical"})).unwrap();
    let canonical = host().lock().unwrap().entries.clone();
    let mut downgrade = canonical.clone();
    downgrade.push(legacy());
    host().lock().unwrap().entries = downgrade;
    assert_restore_failure("invalid durable journal record");

    host().lock().unwrap().entries = canonical.clone();
    for filtered in [
        None,
        Some(json!(false)),
        Some(json!("true")),
        Some(json!(1)),
        Some(Value::Null),
    ] {
        let mut reply = json!({"entries":canonical});
        if let Some(filtered) = filtered {
            reply["filtered"] = filtered;
        }
        host().lock().unwrap().branch_reply = Some(reply);
        assert_restore_failure("filtered: true");
    }
    host().lock().unwrap().branch_reply = None;
    host().lock().unwrap().reject_filter = true;
    assert_restore_failure("unsupported GetSessionBranch customTypes");
    host().lock().unwrap().reject_filter = false;
    for reply in [
        json!({"filtered":true}),
        json!({"filtered":true,"entries":{}}),
        json!({"filtered":true,"entries":[null]}),
        json!({"filtered":true,"entries":[{"type":"message"}]}),
        json!({"filtered":true,"entries":[{
            "id":"old-runtime", "type":"custom", "customType":"rpi-durable-runtime",
            "data":{"run":null,"queue":[]}
        }]}),
    ] {
        host().lock().unwrap().branch_reply = Some(reply);
        assert_restore_failure("branch restore failed");
    }
    host().lock().unwrap().branch_reply = None;
    assert_eq!(empty(EventTag::SessionStart), 0);
    assert_eq!(
        action(json!({"action":"status"})).unwrap()["details"]["queue"][0]["text"],
        "canonical"
    );

    // Conforming hosts exclude obsolete runtime entries, with no migration.
    host().lock().unwrap().entries = vec![json!({
        "id":"obsolete", "type":"custom", "customType":"rpi-durable-runtime",
        "data":{"run":{"id":"old"},"queue":[{"text":"not migrated"}]}
    })];
    assert_eq!(empty(EventTag::SessionTree), 0);
    assert!(
        action(json!({"action":"status"})).unwrap()["details"]["queue"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    host().lock().unwrap().entries = canonical;
    RELEASE_READ.store(false, Ordering::Release);
    host().lock().unwrap().block_read = true;
    let restore = std::thread::spawn(|| empty(EventTag::SessionTree));
    for _ in 0..10_000 {
        if READ_STARTED.load(Ordering::Acquire) {
            break;
        }
        std::thread::yield_now();
    }
    assert!(READ_STARTED.load(Ordering::Acquire));

    let (sent, received) = std::sync::mpsc::channel();
    let mutation = std::thread::spawn(move || {
        sent.send(action(
            json!({"action":"enqueue","text":"wait for restore"}),
        ))
        .unwrap();
    });
    assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
    RELEASE_READ.store(true, Ordering::Release);
    assert_eq!(restore.join().unwrap(), 0);
    let mutation_result = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(mutation_result.is_ok());
    mutation.join().unwrap();
    host().lock().unwrap().block_read = false;

    READ_STARTED.store(false, Ordering::Release);
    RELEASE_READ.store(false, Ordering::Release);
    // A protocol failure must publish the failed store while holding the same
    // restore lock as a transport failure, before waiting mutations resume.
    host().lock().unwrap().branch_reply = Some(json!({"entries":[]}));
    host().lock().unwrap().block_read = true;
    let before_failed_restore = host().lock().unwrap().entries.clone();
    let restore = std::thread::spawn(|| empty(EventTag::SessionTree));
    for _ in 0..10_000 {
        if READ_STARTED.load(Ordering::Acquire) {
            break;
        }
        std::thread::yield_now();
    }
    assert!(READ_STARTED.load(Ordering::Acquire));
    let (sent, received) = std::sync::mpsc::channel();
    let mutation = std::thread::spawn(move || {
        sent.send(action(json!({"action":"enqueue","text":"must not append"})))
            .unwrap();
    });
    assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
    RELEASE_READ.store(true, Ordering::Release);
    assert_ne!(restore.join().unwrap(), 0);
    let mutation_result = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(mutation_result.is_ok());
    assert!(mutation_result.unwrap()["details"]["error"].is_string());
    mutation.join().unwrap();
    assert_eq!(host().lock().unwrap().entries, before_failed_restore);
    host().lock().unwrap().branch_reply = None;
    host().lock().unwrap().block_read = false;
    assert_eq!(empty(EventTag::SessionTree), 0);

    host().lock().unwrap().entries.clear();
    assert_eq!(empty(EventTag::SessionTree), 0);
}
