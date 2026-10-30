use super::*;
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host { mutations: Mutex<Vec<Value>> }
#[async_trait::async_trait]
impl RuntimeActionHost for Host {
    async fn send_message(&self, args: Value) -> Result<Value, String> { self.mutations.lock().unwrap().push(args); Ok(Value::Null) }
    async fn send_user_message(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn append_entry(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn set_active_tools(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn set_session_name(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn set_model(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn set_thinking_level(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn compact(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn new_session(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn fork(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn navigate_tree(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn switch_session(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn reload(&self, args: Value) -> Result<Value, String> { self.send_message(args).await }
    async fn get_active_tools(&self, _: Value) -> Result<Value, String> { Ok(json!({"tools":["root-tool"]})) }
    async fn get_thinking_level(&self, _: Value) -> Result<Value, String> { Ok(Value::Null) }
    async fn get_system_prompt(&self, _: Value) -> Result<Value, String> { Ok(Value::Null) }
    async fn get_execution_context(&self, _: Value) -> Result<Value, String> {
        // These fields cannot be supplied by the post-dispatch JSON overlay:
        // prove real task-local scope and cancellation survived Handle::spawn.
        Ok(json!({"sessionId":"root", "scoped":rpi_agent::tool_execution_scope::is_child_execution(),
            "signal":rpi_agent::tool_execution_scope::invocation_signal().is_some()}))
    }
}

fn ffi(handle: usize, action: RuntimeActionId, args: Value) -> (i32, Value) {
    let mut out = StbString::empty();
    let rc = trampoline_runtime_action(action.into(), StbStringRef::from_str(&args.to_string()), &mut out, handle as *mut c_void);
    let text = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string)) }.to_string_lossy();
    (rc, serde_json::from_str(&text).unwrap())
}
fn open(id: &str) -> Value {
    json!({"op":"open","requestId":id,"toolCallId":id,"ui":{"type":"confirm","title":"ok?"}})
}
async fn child(bridge: &Arc<ActionBridge>, signal: CancellationToken) -> Arc<ActionBridge> {
    Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(json!({"sessionId":"child","childAgent":false})))
        .run(async { bridge.capture_invocation(signal) }).await
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn ffi_child_ordinary_dispatch_denies_root_state_and_restores_trusted_scope() {
    let host = Arc::new(Host::default());
    let bridge = ActionBridge::new(Handle::current(), host.clone());
    let invocation = child(&bridge, CancellationToken::new()).await;
    let registration = bridge.user_data() as usize;
    let explicit = invocation.user_data() as usize;
    let denied = [RuntimeActionId::SendMessage, RuntimeActionId::SendUserMessage, RuntimeActionId::AppendEntry,
        RuntimeActionId::SetActiveTools, RuntimeActionId::SetModel, RuntimeActionId::SetSessionName,
        RuntimeActionId::SetThinkingLevel, RuntimeActionId::Compact, RuntimeActionId::NewSession,
        RuntimeActionId::Fork, RuntimeActionId::SwitchSession, RuntimeActionId::NavigateTree,
        RuntimeActionId::GetSessionBranch, RuntimeActionId::GetActiveTools, RuntimeActionId::Reload,
        RuntimeActionId::GetSystemPrompt, RuntimeActionId::ChildAgent, RuntimeActionId::SideQuestion,
        RuntimeActionId::SetStatus, RuntimeActionId::SetEditorText, RuntimeActionId::UiComponent];
    // Real foreign worker, with forged identity/runtime replacement parameters.
    std::thread::spawn(move || {
        for action in denied {
            assert_eq!(ffi(explicit, action, json!({"childAgent":false,"sessionId":"root","runtimeHandle":registration,
                "__rpiRuntimeKey":"forged","tools":["evil"],"text":"bad","customType":"bad","op":"start"})).0, 1, "{action:?}");
        }
        let (rc, context) = ffi(explicit, RuntimeActionId::GetExecutionContext, json!({"childAgent":false}));
        assert_eq!(rc, 0);
        assert_eq!(context["sessionId"], "child");
        assert_eq!(context["scoped"], true);
        assert_eq!(context["signal"], true);
    }).join().unwrap();
    // Cached registration from either tool driver's TLS resolves invocation.
    invocation.with_invocation(|| {
        assert_eq!(ffi(registration, RuntimeActionId::SendMessage, json!({})).0, 1);
        assert_eq!(ffi(registration, RuntimeActionId::GetExecutionContext, json!({})).1["sessionId"], "child");
    });
    assert!(host.mutations.lock().unwrap().is_empty());
    drop(invocation);
    assert_eq!(ffi(explicit, RuntimeActionId::SendMessage, json!({})).0, 1); // expired
    assert_eq!(ffi(registration, RuntimeActionId::SendMessage, json!({"text":"root"})).0, 0);
    assert_eq!(host.mutations.lock().unwrap().len(), 1);
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn ffi_workers_fail_closed_but_explicit_scoped_ui_and_root_authority_work() {
    let mailbox = UiDialogMailbox::new(); mailbox.attach();
    let bridge = ActionBridge::with_ui_dialog(Handle::current(), Arc::new(Host::default()), mailbox.clone());
    let signal = CancellationToken::new();
    let invocation = child(&bridge, signal.clone()).await;
    let registration = bridge.user_data() as usize;
    let explicit = invocation.user_data() as usize;
    std::thread::spawn(move || {
        assert_eq!(ffi(registration, RuntimeActionId::UiDialog, open("ambiguous")).0, 1);
        assert_eq!(ffi(registration, RuntimeActionId::AppendEntry, json!({})).0, 1);
        assert_eq!(ffi(explicit, RuntimeActionId::UiDialog, open("child-ui")).0, 0);
    }).join().unwrap();
    assert_eq!(mailbox.take_pending().unwrap().session_id.as_deref(), Some("child"));
    assert!(mailbox.take_pending().is_none());
    // A genuine host-captured root command can still interact while child lives.
    let command = bridge.capture_invocation(CancellationToken::new());
    command.with_invocation(|| assert_eq!(ffi(registration, RuntimeActionId::UiDialog, open("root-ui")).0, 0));
    assert_eq!(mailbox.take_pending().unwrap().session_id, None);
    signal.cancel();
    assert_eq!(ffi(explicit, RuntimeActionId::UiDialog, open("late")).0, 1);
    assert_eq!(ffi(explicit, RuntimeActionId::GetExecutionContext, json!({})).0, 1);
    assert_eq!(ffi(explicit, RuntimeActionId::UiDialog, json!({"op":"cancel","requestId":"child-ui"})).0, 0);
    // Cancellation does not make a retained foreign callback become root.
    assert_eq!(ffi(registration, RuntimeActionId::UiDialog, open("still-ambiguous")).0, 1);
    drop(invocation);
    assert_eq!(ffi(explicit, RuntimeActionId::UiDialog, open("expired")).0, 1);
    assert_eq!(ffi(registration, RuntimeActionId::UiDialog, open("root-again")).0, 0);
    assert_eq!(mailbox.take_pending().unwrap().session_id, None);
}
