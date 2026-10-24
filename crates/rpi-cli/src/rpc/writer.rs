//! Single-owner bounded stdout writer for the native RPC transport.
use std::io::Write;

#[derive(Clone)]
pub(crate) struct RpcWriter(tokio::sync::mpsc::Sender<Vec<u8>>);

pub(crate) fn start_rpc_writer<W: Write + Send + 'static>(
    mut output: W,
) -> (
    RpcWriter,
    tokio::task::JoinHandle<std::io::Result<()>>,
    tokio_util::sync::CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let failed = tokio_util::sync::CancellationToken::new();
    let failure_signal = failed.clone();
    let task = tokio::task::spawn_blocking(move || {
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
    (RpcWriter(tx), task, failed)
}

pub(crate) async fn write_line(out: &RpcWriter, value: &serde_json::Value) {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    let _ = out.0.send(line).await;
}
