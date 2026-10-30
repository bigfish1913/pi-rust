use super::*;
use crate::child_agents::ChildAgents;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use serde_json::json;

#[test]
fn durable_root_component_matches_create_rule_and_rejects_windows_devices() {
    for id in ["a", "a-b_c.d", "01234567-89ab-4cde-8fab-0123456789ab", "COM0", "LPT10"] {
        assert!(safe_child_root_component(id), "{id}");
    }
    for id in ["", ".", "..", "-abc", "abc-", "a/b", "a\\b", "C:childscope", "D:childscope", "CON", "con.txt", "NUL", "AUX", "PRN", "COM1", "lpt9.log", "COM¹", "abc ", "é"] {
        assert!(!safe_child_root_component(id), "{id}");
    }
    assert!(!safe_child_root_component(&"a".repeat(65)));
}

// Exercise the real exact-file open, not a forged storage binding. Opening a
// legacy id remains supported, but durable delegation must fail before creation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_v4_unsafe_roots_fail_child_factory_without_events_or_files() {
    for id in ["C:childscope", "D:childscope", "CON", "con.txt", "COM1", "LPT9"] {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let path = temp.path().join("opened-root.jsonl");
        let bytes = format!("{}\n", json!({"kind":"header", "version":4, "id":id,
            "createdAt": 0, "cwd":cwd}));
        std::fs::write(&path, &bytes).unwrap();
        let meta = session_metadata_at(&path).unwrap();
        let session = open_session(&meta, &cwd).await.unwrap();
        assert_eq!(session.storage().metadata().id, id);
        let provider = FauxProvider::new(FauxScript::new().with_text("must not run"));
        let parent = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
            model: provider.default_model().clone(), models: vec![provider.clone()], session,
            ..Default::default()
        }).await.unwrap();
        let before: Vec<_> = std::fs::read_dir(temp.path()).unwrap().map(|e| e.unwrap().path()).collect();
        let manager = ChildAgents::default();
        manager.set_session_factory(native_child_session_factory(temp.path().into()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        manager.set_event_sink(Some(Arc::new(move |event| {
            captured.lock().unwrap().push(event); Box::pin(async {})
        })));
        let error = manager.handle(&parent, 7, json!({"op":"start", "task":"report"})).await.unwrap_err();
        assert!(error.contains("invalid root native session id"), "{id}: {error}");
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(provider.state().call_count.load(std::sync::atomic::Ordering::Relaxed), 0);
        let after: Vec<_> = std::fs::read_dir(temp.path()).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(before, after, "no child directories/files for {id}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        parent.close().await.unwrap();
    }
}
