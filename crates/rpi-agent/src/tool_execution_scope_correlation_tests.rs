use super::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn root_binding_is_not_child_authority_and_parallel_ids_do_not_cross() {
    let mut jobs = Vec::new();
    for id in ["parent-a", "parent-b"] {
        jobs.push(tokio::spawn(with_invocation_binding(
            InvocationToolCallId::new(id), CancellationToken::new(), async move {
                tokio::task::yield_now().await;
                assert!(!is_child_execution());
                assert_eq!(invocation_tool_call_id().unwrap().as_str(), id);
                assert!(execution_context().is_none());
                let scope = Arc::new(ToolExecutionScope::default());
                scope.run(async {
                    assert!(is_child_execution());
                    assert_eq!(invocation_tool_call_id().unwrap().as_str(), id);
                }).await;
                with_invocation_binding(None, CancellationToken::new(), async {
                    assert!(invocation_tool_call_id().is_none());
                }).await;
                assert_eq!(invocation_tool_call_id().unwrap().as_str(), id);
            },
        )));
    }
    for job in jobs { job.await.unwrap(); }
    assert!(invocation_tool_call_id().is_none());
    assert!(!is_child_execution());
}

#[test]
fn actual_tool_id_validation_is_byte_bounded_and_nonempty() {
    for id in ["", " \n", &"x".repeat(1025), &"é".repeat(513)] {
        assert!(InvocationToolCallId::new(id).is_none());
    }
    assert!(InvocationToolCallId::new(&"é".repeat(512)).is_some());
    for byte in (0u8..=31).chain(std::iter::once(127)) {
        let id = format!("parent-{}-id", char::from(byte));
        assert!(InvocationToolCallId::new(&id).is_none());
    }
}
