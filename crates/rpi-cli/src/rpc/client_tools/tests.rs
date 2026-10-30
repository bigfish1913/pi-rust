use super::*;
use super::super::writer::start_rpc_writer;
use rpi_tools::tools::client::{create_client_bash_tool, create_client_read_tool, create_client_write_tool};
use tokio::sync::mpsc;

mod child_identity;

struct Capture {
    events: Arc<Mutex<Vec<Value>>>,
    tx: mpsc::UnboundedSender<Value>,
    bytes: Vec<u8>,
}
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        while let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.bytes.drain(..=end).collect();
            let event: Value = serde_json::from_slice(&line).unwrap();
            self.events.lock().unwrap().push(event.clone());
            let _ = self.tx.send(event);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
struct Fixture {
    service: Arc<ClientTools>,
    rx: mpsc::UnboundedReceiver<Value>,
    events: Arc<Mutex<Vec<Value>>>,
    writer: tokio::task::JoinHandle<std::io::Result<()>>,
    root: tempfile::TempDir,
}
impl Fixture {
    fn new(timeout_ms: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd_and_allowed_roots(root.path().to_owned(), vec![]));
        let service = ClientTools::new(ClientToolsConfig::parse(&json!({"version":1,
            "readText":true,"writeText":true,"terminal":true,"requestTimeoutMs":timeout_ms}).to_string()).unwrap(), env);
        let (tx, rx) = mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (writer, task, _) = start_rpc_writer(Capture { tx, events: events.clone(), bytes: vec![] });
        service.attach(writer, "session-test".into());
        Self { service, rx, events, writer: task, root }
    }
    fn backend(&self) -> ClientBackend { ClientBackend(self.service.clone()) }
    async fn next(&mut self, method: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = self.rx.recv().await.expect("writer closed");
                if event["type"] == "client_request" {
                    assert_eq!(event["method"], method);
                    assert_eq!(event["sessionId"], "session-test");
                    return event;
                }
            }
        }).await.unwrap_or_else(|_| panic!("missing {method}"))
    }
    fn reply(&self, request: &Value, reply: Result<Value, String>) {
        assert!(self.service.broker.resolve(request["requestId"].as_str().unwrap(), reply));
    }
    async fn finish(self) {
        tokio::time::timeout(Duration::from_secs(3), self.service.shutdown()).await.unwrap();
        assert!(self.service.tasks.is_empty());
        assert!(self.service.binding.lock().unwrap().is_none());
        tokio::time::timeout(Duration::from_secs(3), self.writer).await.unwrap().unwrap().unwrap();
        let events = self.events.lock().unwrap();
        for request in events.iter().filter(|e| e["type"] == "client_request") {
            let id = request["requestId"].as_str().unwrap();
            assert!(!self.service.broker.resolve(id, Ok(Value::Null)));
            assert_eq!(events.iter().filter(|e| e["type"] == "client_request_closed" && e["requestId"] == id).count(), 1);
        }
    }
}

#[tokio::test]
async fn real_read_write_producers_use_rpc_not_local_files() {
    let mut f = Fixture::new(1000);
    let path = f.root.path().join("buffer.txt");
    std::fs::write(&path, "local must not be read or overwritten").unwrap();
    let backend: Arc<dyn ClientToolBackend> = Arc::new(f.backend());
    let read = create_client_read_tool(backend.clone());
    let task = tokio::spawn(async move {
        read.execute("read", json!({"path":"buffer.txt","offset":2,"limit":1}), CancellationToken::new(), Arc::new(|_| {})).await
    });
    let request = f.next("fs/read_text_file").await;
    assert_eq!(request["params"]["path"], path.to_string_lossy().as_ref());
    f.reply(&request, Ok(json!({"content":"client first\nclient second\n"})));
    let result = task.await.unwrap().unwrap();
    assert!(matches!(&result.content[..], [rpi_agent::types::TextContentOrImage::Text(text)] if text.text == "client second\n"));
    let write = create_client_write_tool(backend);
    let task = tokio::spawn(async move {
        write.execute("write", json!({"path":"buffer.txt","content":"client replacement"}), CancellationToken::new(), Arc::new(|_| {})).await
    });
    let request = f.next("fs/write_text_file").await;
    assert_eq!(request["params"], json!({"path":path,"content":"client replacement"}));
    f.reply(&request, Ok(json!({})));
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "local must not be read or overwritten");
    f.finish().await;
}

#[tokio::test]
async fn read_write_errors_cancel_drop_timeout_and_shutdown_never_fallback() {
    for mode in ["error", "malformed", "cancel", "drop", "timeout", "shutdown"] {
        let mut f = Fixture::new(if mode == "timeout" { 20 } else { 1000 });
        let backend = f.backend();
        let cancel = CancellationToken::new();
        let owned_cancel = cancel.clone();
        let task = tokio::spawn(async move { backend.read_text("missing", owned_cancel).await });
        let request = f.next("fs/read_text_file").await;
        match mode {
            "error" => f.reply(&request, Err("permission denied by client".into())),
            "malformed" => f.reply(&request, Ok(json!({"content":42}))),
            "cancel" => cancel.cancel(),
            "drop" => task.abort(),
            "shutdown" => { f.service.shutdown().await; }
            _ => {}
        }
        match task.await {
            Ok(result) => assert!(result.is_err(), "{mode}"),
            Err(error) => assert!(mode == "drop" && error.is_cancelled()),
        }
        f.finish().await;
    }
    let mut f = Fixture::new(1000);
    let backend = f.backend();
    let task = tokio::spawn(async move { backend.write_text("not-created", "x", CancellationToken::new()).await });
    let request = f.next("fs/write_text_file").await;
    f.reply(&request, Err("write permission denied".into()));
    assert!(task.await.unwrap().unwrap_err().contains("permission denied"));
    assert!(!f.root.path().join("not-created").exists());
    f.finish().await;
}

#[tokio::test]
async fn real_bash_producer_creates_waits_outputs_and_releases() {
    let mut f = Fixture::new(1000);
    let bash = create_client_bash_tool(Arc::new(f.backend()));
    let task = tokio::spawn(async move {
        bash.execute("bash", json!({"command":"echo client","timeout":1}), CancellationToken::new(), Arc::new(|_| {})).await
    });
    let create = f.next("terminal/create").await;
    assert_eq!(create["params"]["command"], "bash");
    assert_eq!(create["params"]["args"], json!(["-c","echo client"]));
    assert_eq!(create["params"]["cwd"], json!(f.root.path()));
    f.reply(&create, Ok(json!({"terminalId":"terminal-1"})));
    let wait = f.next("terminal/wait_for_exit").await;
    f.reply(&wait, Ok(json!({"exitCode":0})));
    let output = f.next("terminal/output").await;
    f.reply(&output, Ok(json!({"output":"client stdout","truncated":true})));
    let release = f.next("terminal/release").await;
    f.reply(&release, Ok(json!({})));
    let result = task.await.unwrap().unwrap();
    let [rpi_agent::types::TextContentOrImage::Text(text)] = &result.content[..] else { panic!("expected text") };
    assert!(text.text.contains("client stdout"));
    assert!(text.text.contains("truncated"));
    f.finish().await;
}

#[tokio::test]
async fn terminal_cancel_drop_timeout_shutdown_always_kill_and_release() {
    for mode in ["cancel", "drop-wait", "drop-create", "timeout", "shutdown", "shutdown-create"] {
        let mut f = Fixture::new(1000);
        let backend = f.backend();
        let cancel = CancellationToken::new();
        let owned_cancel = cancel.clone();
        let task = tokio::spawn(async move { backend.exec("echo client", Some(0.05), owned_cancel).await });
        let mut task = Some(task);
        let create = f.next("terminal/create").await;
        if mode == "drop-create" {
            let dropped = task.take().unwrap();
            dropped.abort();
            assert!(dropped.await.unwrap_err().is_cancelled());
        }
        let shutdown = if mode == "shutdown-create" {
            let service = f.service.clone();
            let shutdown = tokio::spawn(async move { service.shutdown().await });
            f.service.stop.cancelled().await;
            Some(shutdown)
        } else { None };
        f.reply(&create, Ok(json!({"terminalId":"owned-terminal"})));
        let mut shutdown = shutdown;
        if !matches!(mode, "drop-create" | "shutdown-create") {
            let _wait = f.next("terminal/wait_for_exit").await;
            match mode {
                "cancel" => cancel.cancel(),
                "drop-wait" => task.as_ref().unwrap().abort(),
                "shutdown" => {
                    let service = f.service.clone();
                    shutdown = Some(tokio::spawn(async move { service.shutdown().await }));
                }
                _ => {}
            }
        }
        let kill = f.next("terminal/kill").await;
        assert_eq!(kill["params"], json!({"terminalId":"owned-terminal"}));
        // Even a denied kill must still release the terminal.
        f.reply(&kill, Err("kill denied".into()));
        let release = f.next("terminal/release").await;
        assert_eq!(release["params"], json!({"terminalId":"owned-terminal"}));
        f.reply(&release, Ok(json!({})));
        if let Some(task) = task { assert!(task.await.map(|result| result.is_err()).unwrap_or(true)); }
        if let Some(shutdown) = shutdown { shutdown.await.unwrap(); }
        f.finish().await;
    }
}

#[tokio::test]
async fn shutdown_during_pending_cleanup_reaps_all_tasks_without_clearing_broker() {
    let mut f = Fixture::new(1000);
    let backend = f.backend();
    let task = tokio::spawn(async move { backend.exec("echo client", Some(0.01), CancellationToken::new()).await });
    let create = f.next("terminal/create").await;
    f.reply(&create, Ok(json!({"terminalId":"pending-cleanup"})));
    let _wait = f.next("terminal/wait_for_exit").await;
    let kill = f.next("terminal/kill").await;
    let service = f.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    f.reply(&kill, Ok(json!({})));
    let release = f.next("terminal/release").await;
    f.reply(&release, Ok(json!({})));
    shutdown.await.unwrap();
    assert!(task.await.unwrap().is_err());
    // Repeated shutdown and admission after shutdown are safe.
    assert!(f.backend().read_text("x", CancellationToken::new()).await.is_err());
    f.finish().await;
}

#[tokio::test]
async fn unresponsive_creation_and_cleanup_are_bounded_on_shutdown() {
    for reply_create in [false, true] {
        let mut f = Fixture::new(20);
        let backend = f.backend();
        let task = tokio::spawn(async move { backend.exec("echo client", Some(0.01), CancellationToken::new()).await });
        let create = f.next("terminal/create").await;
        if reply_create {
            f.reply(&create, Ok(json!({"terminalId":"unresponsive"})));
            let _ = f.next("terminal/wait_for_exit").await;
            let _ = f.next("terminal/kill").await;
        }
        tokio::time::timeout(Duration::from_secs(2), f.service.shutdown()).await.unwrap();
        assert!(task.await.unwrap().is_err());
        if reply_create {
            assert!(f.events.lock().unwrap().iter().any(|e| e["method"] == "terminal/release"));
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn config_capabilities_roots_and_ambiguous_paths_fail_closed() {
    for config in [json!({}), json!({"version":2}), json!({"version":1,"requestTimeoutMs":0}),
        json!({"version":1,"requestTimeoutMs":60001}), json!({"version":1,"unknown":true}),
        json!({"version":1,"workspaceRoots":["relative"]})] {
        assert!(ClientToolsConfig::parse(&config.to_string()).is_err());
    }
    let config = ClientToolsConfig::parse(r#"{"version":1}"#).unwrap();
    assert!(!config.read_text && !config.write_text && !config.terminal);
    let f = Fixture::new(1000);
    let outside = tempfile::tempdir().unwrap();
    let backend = f.backend();
    let cancel = CancellationToken::new();
    for path in ["".to_owned(), "file:///outside".into(), "~/.secret".into(), "bad\0path".into(),
        outside.path().join("secret").to_string_lossy().into_owned(), "../escape".into()] {
        assert!(backend.read_text(&path, cancel.clone()).await.is_err(), "{path:?}");
        assert!(backend.write_text(&path, "x", cancel.clone()).await.is_err(), "{path:?}");
    }
    #[cfg(windows)]
    for path in [r"C:drive-relative", r"\root-relative", r"C:\Windows\outside"] {
        assert!(backend.read_text(path, cancel.clone()).await.is_err());
    }
    assert!(backend.exec(&format!("cat \"{}\"", outside.path().join("secret").display()), None, cancel.clone()).await.is_err());
    assert!(f.events.lock().unwrap().is_empty());
    // Disabled capabilities cannot contact the client even with an attached transport.
    let env = Arc::new(OsExecutionEnv::with_cwd_and_allowed_roots(f.root.path().to_owned(), vec![]));
    let disabled = ClientBackend(ClientTools::new(config, env));
    assert!(disabled.read_text("x", cancel.clone()).await.unwrap_err().contains("disabled"));
    assert!(disabled.write_text("x", "x", cancel.clone()).await.unwrap_err().contains("disabled"));
    assert!(disabled.exec("echo x", None, cancel).await.unwrap_err().contains("disabled"));
    // Detached operation is an error, never a local read.
    let detached = ClientBackend(ClientTools::new(f.service.config.clone(), f.service.env.clone()));
    assert!(detached.read_text("x", CancellationToken::new()).await.unwrap_err().contains("attached RPC"));
    detached.shutdown().await;
    f.finish().await;
}
