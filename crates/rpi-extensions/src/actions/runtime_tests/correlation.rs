use super::*;
use super::driver::{schema, noop};
use crate::{PluginKeepalive, PluginToolAdapter, PluginToolHandle};
use rpi_agent::AgentTool;
use rpi_plugin_sdk::{FreeStringFn, StepHandle, StepResult, ToolPartialCb, ToolWakeCb};

extern "C" fn execute(id: StbStringRef, params: StbString, free: Option<FreeStringFn>) -> StepHandle {
    let owned = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(params, free) };
    let args: Value = serde_json::from_str(&owned.to_string_lossy()).unwrap();
    let context = &args["__rpi"];
    let handle = context["runtimeHandle"].as_u64().unwrap() as usize;
    let key = context["runtimeKey"].as_str().unwrap().to_owned();
    let actual = unsafe { id.as_str() }.to_owned();
    let worker = std::thread::spawn(move || {
        let args = json!({"op":"start","parentToolCallId":"spoof","context":{"parentToolCallId":"spoof"},"__rpiRuntimeKey":key});
        let (rc, value) = call(handle, args);
        assert_eq!(rc, 0, "{value}");
        assert_eq!(value["parentToolCallId"], actual);
        assert_eq!(value["isChild"], false);
    });
    // Keep panic out of the extern callback; return failure handle on mismatch.
    Box::into_raw(Box::new(worker.join().is_ok())) as StepHandle
}
extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if unsafe { *(handle as *const bool) } {
        StepResult::done(StbString::from_string("{}".into()))
    } else { StepResult::err(StbString::from_string("correlation fixture failed".into())) }
}
extern "C" fn destroy(handle: StepHandle) { drop(unsafe { Box::from_raw(handle as *mut bool) }); }
extern "C" fn set_waker(_: StepHandle, _: Option<ToolWakeCb>, _: *mut c_void) {}

#[tokio::test(flavor="multi_thread", worker_threads=4)]
async fn actual_blocking_and_wake_execute_arguments_survive_foreign_workers_in_parallel() {
    let host = Arc::new(Host::default());
    let bridge = ActionBridge::new(Handle::current(), host.clone());
    let mut jobs = Vec::new();
    for (wake, id) in [(false,"blocking-a"),(false,"blocking-b"),(true,"wake-a"),(true,"wake-b")] {
        let adapter = PluginToolAdapter::new(schema(), PluginToolHandle {
            execute_fn: execute, set_waker_fn: if wake { Some(set_waker) } else { None },
            poll_fn: poll, cancel_fn: noop, destroy_fn: destroy,
            plugin_free_string: crate::host_free_string, runtime_handle: Some(bridge.user_data() as usize),
        }, PluginKeepalive::empty());
        jobs.push(tokio::spawn(async move {
            adapter.execute(id, json!({"parentToolCallId":"spoof","__rpi":{"runtimeHandle":0,"runtimeKey":"spoof"}}),
                CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
        }));
    }
    for job in jobs { job.await.unwrap(); }
    assert_eq!(host.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn authenticated_binding_rejects_spoof_cancelled_and_retired_invocations() {
    let host = Arc::new(Host::default());
    let root = ActionBridge::new(Handle::current(), host.clone());
    let signal = CancellationToken::new();
    let invocation = root.for_tool_invocation(false, signal.clone(), Some("actual"));
    let handle = invocation.user_data() as usize;
    let key = invocation.invocation_key().unwrap().to_owned();
    let args = json!({"op":"start","parentToolCallId":"spoof","__rpiRuntimeKey":key});
    assert_eq!(call(handle, json!({"op":"start","__rpiRuntimeKey":"spoof"})).0, 1);
    let reply = call(handle, args.clone());
    assert_eq!(reply.0, 0);
    assert_eq!(reply.1["parentToolCallId"], "actual");
    signal.cancel();
    assert_eq!(call(handle, args.clone()).0, 1);
    drop(invocation);
    assert_eq!(call(handle, args).0, 1);
    let no_id = root.capture_invocation(CancellationToken::new());
    assert!(call(no_id.user_data() as usize, json!({"op":"start","parentToolCallId":"spoof","__rpiRuntimeKey":no_id.invocation_key()})).1["parentToolCallId"].is_null());
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
}
