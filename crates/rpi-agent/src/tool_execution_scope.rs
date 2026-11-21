//! Host-managed child execution identity and background FFI driver lifetime.
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

tokio::task_local! {
    static CURRENT: Arc<ToolExecutionScope>;
    static INVOCATION_SIGNAL: tokio_util::sync::CancellationToken;
}

/// Bind a runtime action to the originating tool invocation, even after its
/// dispatch has moved off the foreign thread. This is host-only state, not JSON.
pub async fn with_invocation_signal<F: Future>(
    signal: tokio_util::sync::CancellationToken,
    future: F,
) -> F::Output {
    INVOCATION_SIGNAL.scope(signal, future).await
}

pub fn invocation_signal() -> Option<tokio_util::sync::CancellationToken> {
    INVOCATION_SIGNAL.try_with(Clone::clone).ok()
}

/// Shared by a child's run, tools, providers and detached foreign callbacks.
#[derive(Default)]
pub struct ToolExecutionScope {
    pending: AtomicUsize,
    idle: Notify,
    context: Option<serde_json::Value>,
}
impl ToolExecutionScope {
    pub fn with_context(context: serde_json::Value) -> Self {
        Self {
            context: Some(context),
            ..Default::default()
        }
    }
    /// Bind the task-local identity without an extra async state machine. An
    /// async wrapper would reserve storage for both `future` and its moved copy
    /// inside `TaskLocalFuture`, doubling large callers' inline future size.
    /// Tokio installs the identity only while polling or dropping the future.
    pub fn run<F: Future>(self: &Arc<Self>, future: F) -> impl Future<Output = F::Output> {
        CURRENT.scope(self.clone(), future)
    }

    /// Foreground cancellation is not proof that foreign code has returned.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub fn current_scope() -> Option<Arc<ToolExecutionScope>> {
    CURRENT.try_with(Clone::clone).ok()
}

pub fn is_child_execution() -> bool {
    CURRENT.try_with(|_| ()).is_ok()
}
pub fn execution_context() -> Option<serde_json::Value> {
    CURRENT
        .try_with(|scope| scope.context.clone())
        .ok()
        .flatten()
}

/// A blocking driver owns this guard until its final foreign callback returns.
pub struct BackgroundToolGuard(Arc<ToolExecutionScope>);
impl Drop for BackgroundToolGuard {
    fn drop(&mut self) {
        if self.0.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}
pub fn track_background_tool() -> Option<BackgroundToolGuard> {
    CURRENT
        .try_with(|scope| {
            scope.pending.fetch_add(1, Ordering::AcqRel);
            BackgroundToolGuard(scope.clone())
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoping_does_not_duplicate_the_inner_future_storage() {
        let scope = Arc::new(ToolExecutionScope::default());
        let future = scope.run(std::future::ready([7_u8; 16 * 1024]));
        // A task-local binding needs a little metadata, not another payload-sized
        // slot for moving an async function's argument into its awaited future.
        assert!(std::mem::size_of_val(&future) <= 16 * 1024 + 128);
        assert!(!is_child_execution());
        assert_eq!(future.await, [7_u8; 16 * 1024]);
        assert!(!is_child_execution());
    }

    #[tokio::test]
    async fn identity_and_context_are_scoped_and_never_leak_to_the_parent() {
        let scope = Arc::new(ToolExecutionScope::with_context(
            serde_json::json!({"childAgent":false,"tools":["read"]}),
        ));
        assert!(!is_child_execution());
        scope
            .run(async {
                assert!(is_child_execution());
                assert_eq!(
                    execution_context().unwrap()["tools"],
                    serde_json::json!(["read"])
                );
                tokio::task::yield_now().await;
                assert!(Arc::ptr_eq(&current_scope().unwrap(), &scope));
            })
            .await;
        assert!(!is_child_execution());
        assert!(execution_context().is_none());
        assert!(track_background_tool().is_none());
    }
    #[tokio::test]
    async fn invocation_cancellation_crosses_dispatch_without_changing_child_identity() {
        let token = tokio_util::sync::CancellationToken::new();
        assert!(invocation_signal().is_none());
        let captured = with_invocation_signal(token.clone(), async {
            assert!(!is_child_execution());
            let child = Arc::new(ToolExecutionScope::default());
            child
                .run(async {
                    assert!(is_child_execution());
                    invocation_signal().unwrap()
                })
                .await
        })
        .await;
        assert!(invocation_signal().is_none());
        assert!(!captured.is_cancelled());
        token.cancel();
        assert!(captured.is_cancelled());
    }
    #[tokio::test]
    async fn cancellation_drops_under_the_scope_and_allows_reuse() {
        struct CheckDrop(Arc<AtomicUsize>);
        impl Drop for CheckDrop {
            fn drop(&mut self) {
                assert!(is_child_execution());
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let scope = Arc::new(ToolExecutionScope::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let future = scope.run(async {
            let _guard = CheckDrop(drops.clone());
            std::future::pending::<()>().await;
        });
        assert!(!is_child_execution());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), future)
                .await
                .is_err()
        );
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(!is_child_execution());
        scope.run(async { assert!(is_child_execution()) }).await;
        assert!(!is_child_execution());
    }

    #[tokio::test]
    async fn dropping_the_foreground_scope_does_not_finish_its_background_work() {
        let scope = Arc::new(ToolExecutionScope::default());
        let guard = scope.run(async { track_background_tool().unwrap() }).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), scope.wait_idle())
                .await
                .is_err()
        );
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), scope.wait_idle())
            .await
            .unwrap();
    }
}
