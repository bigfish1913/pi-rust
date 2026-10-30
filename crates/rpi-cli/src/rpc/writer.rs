use std::io::Write;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
struct EventProjector;

impl EventProjector {
    fn project(&mut self, event: &rpi_agent::events::AgentEvent) -> Vec<u8> {
        let value = super::events::project(event);
        let assistant =
            value["message"]["role"] == "assistant" || value["message"]["kind"] == "assistant";
        // AssistantMessage.message_id is allocated by the native agent loop at
        // message creation and is persisted with the transcript. Never derive
        // an ACP identity from the live event ordinal: replay and live must use
        // the same ID, including after aborts, branching, or restarts.
        let id = match event {
            rpi_agent::events::AgentEvent::MessageStart { message_id, .. }
            | rpi_agent::events::AgentEvent::MessageUpdate { message_id, .. }
            | rpi_agent::events::AgentEvent::MessageEnd { message_id, .. } => message_id.as_deref(),
            _ => None,
        }
        .or_else(|| {
            assistant
                .then(|| value["message"]["messageId"].as_str())
                .flatten()
        });
        super::events::encode_with_message_id(event, id).unwrap_or_else(|_| b"null".to_vec())
    }
}

#[derive(Clone)]
pub(crate) struct RpcWriter {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    projector: Arc<Mutex<EventProjector>>,
    failed: tokio_util::sync::CancellationToken,
}

pub(crate) fn start_rpc_writer<W: Write + Send + 'static>(
    output: W,
) -> (
    RpcWriter,
    tokio::task::JoinHandle<std::io::Result<()>>,
    tokio_util::sync::CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let failed = tokio_util::sync::CancellationToken::new();
    let failure_signal = failed.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let result = (|| {
            while let Some(line) = rx.blocking_recv() {
                output.write_all(&line)?;
                output.flush()?;
            }
            output.flush()
        })();
        if result.is_err() {
            failure_signal.cancel();
        }
        result
    });
    (
        RpcWriter {
            sender: tx,
            projector: Arc::new(Mutex::new(EventProjector)),
            failed: failed.clone(),
        },
        task,
        failed,
    )
}

impl RpcWriter {
    /// Enqueue a complete JSON line within 200ms, or signal transport failure.
    /// Success acknowledges queue acceptance, not completion of the OS write.
    /// Dropping the send future prevents a timed-out line from arriving later;
    /// no detached delivery task is created. Cancellation cannot interrupt an
    /// already-blocked OS write in the blocking writer task.
    pub(crate) async fn write_bounded(&self, value: &serde_json::Value) -> std::io::Result<()> {
        let mut line = value.to_string().into_bytes();
        line.push(b'\n');
        self.enqueue(line).await
    }

    async fn enqueue(&self, line: Vec<u8>) -> std::io::Result<()> {
        tokio::select! {
            biased;
            _ = self.failed.cancelled() => {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "RPC writer failed"))
            }
            result = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.sender.send(line),
            ) => match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => {
                    self.failed.cancel();
                    Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "RPC writer closed"))
                }
                Err(_) => {
                    self.failed.cancel();
                    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "RPC writer queue stalled"))
                }
            }
        }
    }

    pub(crate) fn try_write_line(&self, value: &serde_json::Value) {
        if self.failed.is_cancelled() {
            return;
        }
        let mut line = value.to_string().into_bytes();
        line.push(b'\n');
        if self.sender.try_send(line).is_err() {
            self.failed.cancel();
        }
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.is_cancelled()
    }

    pub(crate) async fn write_agent_event(&self, event: &rpi_agent::events::AgentEvent) {
        if self.is_failed() { return; }
        let mut projector = self.projector.lock().await;
        let mut line = projector.project(event);
        line.push(b'\n');
        let _ = self.enqueue(line).await;
    }
}

pub(crate) async fn write_line(out: &RpcWriter, value: &serde_json::Value) {
    let _ = out.write_bounded(value).await;
}

pub(crate) async fn write_agent_event(out: &RpcWriter, event: &rpi_agent::events::AgentEvent) {
    out.write_agent_event(event).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex as StdMutex};

    #[derive(Default)]
    struct Gate {
        released: StdMutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    // Always unblock the OS-write stand-in, including on assertion failure.
    struct ReleaseOnDrop(Arc<Gate>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct StalledSink {
        gate: Arc<Gate>,
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        bytes: Arc<StdMutex<Vec<u8>>>,
    }

    impl Write for StalledSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let released = self.gate.released.lock().unwrap();
            drop(self.gate.changed.wait_while(released, |v| !*v).unwrap());
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bounded_enqueue_stalled_sink_times_out_without_late_delivery() {
        let gate = Arc::new(Gate::default());
        let _release = ReleaseOnDrop(gate.clone());
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (writer, task, failed) = start_rpc_writer(StalledSink {
            gate: gate.clone(),
            entered: Some(entered),
            bytes: bytes.clone(),
        });
        writer
            .write_bounded(&serde_json::json!("first"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        // The first line is inside write(); all 64 queue slots remain occupied.
        for n in 0..64 {
            writer.write_bounded(&serde_json::json!(n)).await.unwrap();
        }
        assert_eq!(writer.sender.capacity(), 0);
        let error = writer
            .write_bounded(&serde_json::json!("timed-out"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(failed.is_cancelled());
        // Owner acknowledgement and event paths must not wait for a queue slot
        // after a sample terminal has failed. Keep the sink blocked here.
        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            write_line(&writer, &serde_json::json!({"type":"cancel_sample"})).await;
            write_agent_event(&writer, &rpi_agent::events::AgentEvent::AgentStart).await;
        }).await.unwrap();
        assert_eq!(writer.sender.capacity(), 0);
        // Failure wins even once queue capacity becomes available.
        gate.release();
        let error = writer
            .write_bounded(&serde_json::json!("after-failure"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        drop(writer);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut expected = b"\"first\"\n".to_vec();
        for n in 0..64 {
            expected.extend_from_slice(format!("{n}\n").as_bytes());
        }
        assert_eq!(*bytes.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn bounded_enqueue_closed_receiver_signals_failure() {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let failed = tokio_util::sync::CancellationToken::new();
        let writer = RpcWriter {
            sender,
            projector: Arc::new(Mutex::new(EventProjector)),
            failed: failed.clone(),
        };
        drop(receiver);
        let error = writer
            .write_bounded(&serde_json::json!(null))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(failed.is_cancelled());
    }
}
