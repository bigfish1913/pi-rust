//! Native auxiliary-task custody, not evidence of OS-process or SDK settlement.
//!
//! Admission is sealed independently of TaskTracker (whose close permits spawn).
//! An admitted operation may finish preparing and spawn its own continuations
//! after sealing; its token prevents an empty interval during that hand-off.
//! TaskTracker waits for actual future destruction, not a job's UI status.
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::{oneshot, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::{task_tracker::TaskTrackerToken, TaskTracker};

pub(crate) struct CliTaskScope {
    closed: Mutex<bool>,
    tracker: TaskTracker,
    unknown: Arc<AtomicBool>,
    signal: CancellationToken,
    preparations: Arc<Semaphore>,
}
impl Default for CliTaskScope {
    fn default() -> Self {
        Self {
            closed: Mutex::new(false),
            tracker: TaskTracker::new(),
            unknown: Arc::new(AtomicBool::new(false)),
            signal: CancellationToken::new(),
            preparations: Arc::new(Semaphore::new(32)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CliTaskAdmission {
    tracker: TaskTracker,
    _token: TaskTrackerToken,
    unknown: Arc<AtomicBool>,
}
struct Completion {
    complete: bool,
    unknown: Arc<AtomicBool>,
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.complete {
            self.unknown.store(true, Ordering::Release);
        }
    }
}
impl CliTaskAdmission {
    /// This capability is only for continuations of an already admitted call.
    /// Keeping it alive also keeps wait() pending, even between task spawns.
    pub(crate) fn spawn<F>(&self, runtime: &Handle, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Construct before spawning: destruction before the first poll is not
        // successful cleanup either. Tracker drops the entire future before
        // publishing that the task has exited.
        let mut completion = Completion {
            complete: false,
            unknown: self.unknown.clone(),
        };
        self.tracker.spawn_on(
            async move {
                future.await;
                completion.complete = true;
                drop(completion);
            },
            runtime,
        );
    }
    pub(crate) fn mark_unknown(&self) {
        self.unknown.store(true, Ordering::Release);
    }
}
impl CliTaskScope {
    pub(crate) fn admit(&self) -> Result<CliTaskAdmission, String> {
        let closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        if *closed {
            return Err("native auxiliary resources are retiring".into());
        }
        Ok(CliTaskAdmission {
            tracker: self.tracker.clone(),
            _token: self.tracker.token(),
            unknown: self.unknown.clone(),
        })
    }
    pub(crate) fn signal(&self) -> CancellationToken {
        self.signal.clone()
    }
    pub(crate) fn seal(&self) {
        {
            let mut closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
            *closed = true;
            self.tracker.close();
        }
        self.signal.cancel();
    }
    pub(crate) async fn wait(&self) -> Result<(), String> {
        self.tracker.wait().await;
        if self.unknown.load(Ordering::Acquire) {
            Err("native auxiliary task cleanup is uncertain".into())
        } else {
            Ok(())
        }
    }
    /// The reply waiter does not own or cancel preparation. Preserve the
    /// originating task-local identity/cancellation across this native spawn.
    pub(crate) async fn call<T, F>(&self, future: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let slot = self
            .preparations
            .clone()
            .try_acquire_owned()
            .map_err(|_| "native auxiliary preparation capacity reached".to_owned())?;
        let admission = self.admit()?;
        let scope = rpi_agent::tool_execution_scope::current_scope();
        let signal = rpi_agent::tool_execution_scope::invocation_signal();
        let (tx, rx) = oneshot::channel();
        admission.spawn(&Handle::current(), async move {
            let _slot = slot;
            let scoped = async move {
                match scope {
                    Some(scope) => scope.run(future).await,
                    None => future.await,
                }
            };
            let result = match signal {
                Some(signal) => {
                    rpi_agent::tool_execution_scope::with_invocation_signal(signal, scoped).await
                }
                None => scoped.await,
            };
            let _ = tx.send(result);
        });
        // Do not hold an admission capability while merely awaiting the reply.
        drop(admission);
        rx.await
            .map_err(|_| "native auxiliary preparation did not complete".into())
    }
}

#[cfg(test)]
mod tests;
