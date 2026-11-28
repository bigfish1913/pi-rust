//! Shared Host identity precondition. Stub ABI callbacks, not a loaded DLL or
//! a production Host extension binding / binary three-party acceptance test.
use super::*;
use crate::registry::ExtensionRegistry;
use rpi_plugin_sdk::{StepResult, ToolPartialCb};
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

#[test]
fn bound_agent_identity_is_immutable_while_legacy_late_binding_stays_shared() {
    let context = ToolCallContext::for_agent("/agent-one", "agent-one", Some("session-one".into()));
    let cloned = context.clone();
    cloned.set_session_id("wrong-late-session");
    for bound in [context, cloned] {
        assert_eq!(bound.agent_id().as_deref(), Some("agent-one"));
        assert_eq!(bound.cwd().as_deref(), Some("/agent-one"));
        assert_eq!(bound.session_id().as_deref(), Some("session-one"));
        assert_eq!(
            bound.to_json().unwrap(),
            json!({"cwd":"/agent-one","agentId":"agent-one","sessionId":"session-one"})
        );
    }
    let legacy = ToolCallContext::new("/cli");
    let cloned = legacy.clone();
    cloned.set_session_id("cli-session");
    assert_eq!(
        legacy.to_json().unwrap(),
        json!({"cwd":"/cli","sessionId":"cli-session"})
    );
    assert!(legacy.agent_id().is_none());
    assert!(ToolCallContext::default().to_json().is_none());
}

#[test]
fn bound_sessionless_is_explicit_and_forgery_is_replaced_only_on_the_plugin_copy() {
    let context = ToolCallContext::for_agent("/mini", "mini-agent", None);
    context.clone().set_session_id("ambient-or-late-session");
    let expected = json!({"cwd":"/mini","agentId":"mini-agent","sessionId":null});
    assert!(context.session_id().is_none());
    assert_eq!(context.to_json().unwrap(), expected);
    let original = json!({"action":"list","__rpi":{
        "cwd":"/forged","agentId":"forged-agent","sessionId":"forged-session",
        "runtimeKey":"forged-key","unrecognized":"not-host-metadata"
    }});
    let injected = inject_tool_context(original.clone(), &context);
    assert_eq!(injected, json!({"action":"list","__rpi":expected}));
    assert_eq!(original["__rpi"]["agentId"], "forged-agent");
    assert_eq!(
        inject_tool_context(Value::Null, &context),
        json!({"__rpi":expected})
    );
    // Preserve the existing non-object ABI, not a new wrapping convention.
    assert_eq!(inject_tool_context(json!([1]), &context), json!([1]));
    assert_eq!(inject_tool_context(json!(7), &context), json!(7));
}

struct Drive {
    params: Value,
    concurrent: bool,
}
static DRIVES: rpi_plugin_sdk::handles::ToolHandles<Drive> =
    rpi_plugin_sdk::handles::ToolHandles::new();
static ENTERED: AtomicUsize = AtomicUsize::new(0);
static DESTROYED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    // SAFETY: execute receives an owned payload with its producer deallocator.
    let raw = unsafe { OwnedString::from_raw(params, free) };
    let params = serde_json::from_str(&raw.to_string_lossy()).unwrap_or(Value::Null);
    ENTERED.fetch_add(1, Ordering::SeqCst);
    // All three actual FFI calls must overlap. Bound the stub's blocking work
    // so a broken adapter cannot strand a callback at an unbounded barrier.
    let deadline = Instant::now() + Duration::from_secs(2);
    while ENTERED.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    DRIVES.insert(Drive {
        params,
        concurrent: ENTERED.load(Ordering::SeqCst) == 3,
    })
}
extern "C" fn poll(h: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    let Some(entry) = DRIVES.get(h) else {
        return StepResult::err(StbString::from_string("invalid test handle".into()));
    };
    let drive = entry.lock();
    StepResult::done(StbString::from_string(
        json!({"content":[],"details":{
            "params":drive.params,"concurrent":drive.concurrent
        }})
        .to_string(),
    ))
}
extern "C" fn cancel(h: StepHandle) {
    DRIVES.cancel(h);
}
extern "C" fn destroy(h: StepHandle) {
    drop(DRIVES.remove(h));
    DESTROYED.fetch_add(1, Ordering::SeqCst);
}
use rpi_plugin_sdk::callbacks::free_string as free;
extern "C" fn set_waker(_: StepHandle, _: Option<rpi_plugin_sdk::ToolWakeCb>, _: *mut c_void) {}

#[tokio::test(flavor = "current_thread")]
async fn one_registry_concurrent_ffi_drivers_do_not_mix_agents_or_invent_a_session() {
    for wake in [false, true] {
        ENTERED.store(0, Ordering::SeqCst);
        DESTROYED.store(0, Ordering::SeqCst);
        let mut registry = ExtensionRegistry::new();
        assert!(!registry.register_tool(
            Tool {
                name: "context-probe".into(),
                description: "test".into(),
                parameters: rpi_ai::types::Schema::new(json!({"type":"object"})),
                constrained_sampling: None
            },
            PluginToolHandle {
                execute_fn: execute,
                poll_fn: poll,
                cancel_fn: cancel,
                destroy_fn: destroy,
                plugin_free_string: free,
                runtime_handle: None,
                set_waker_fn: wake.then_some(set_waker)
            },
        ));
        let snapshot = Arc::new(registry.snapshot());
        let keepalive = PluginKeepalive::empty();
        let identities = [
            ("agent-one", Some("session-one".to_string())),
            ("agent-two", Some("session-two".to_string())),
            ("mini-agent", None),
        ];
        let original = json!({"action":"probe","__rpi":{"agentId":"forged","cwd":"/forged","sessionId":"forged"}});
        let mut tasks = Vec::new();
        for (id, session) in identities {
            let context = ToolCallContext::for_agent(format!("/{id}"), id, session.clone());
            context.clone().set_session_id("wrong-late-session");
            let tool = &snapshot.tools()[0];
            let adapter =
                PluginToolAdapter::new(tool.tool.clone(), tool.handle(), keepalive.clone())
                    .with_context(context);
            let params = original.clone();
            tasks.push(tokio::spawn(async move {
                let result = adapter
                    .execute(id, params, CancellationToken::new(), Arc::new(|_| {}))
                    .await
                    .unwrap();
                assert_eq!(result.details["concurrent"], true);
                assert_eq!(
                    result.details["params"],
                    json!({"action":"probe","__rpi":{
                        "cwd":format!("/{id}"),"agentId":id,"sessionId":session
                    }})
                );
            }));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(DESTROYED.load(Ordering::SeqCst), 3);
        assert_eq!(original["__rpi"]["sessionId"], "forged");
    }
}
