// Pure validation / owned asynchronous assembly. No DLL/profile loading yet.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn plan(
    record: &AgentRecord,
    provider: Arc<FauxProvider>,
    validated: Arc<AtomicUsize>,
    assembled: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    mode: &'static str,
) -> OrdinaryBinding {
    let expected = record.spec.clone();
    let env = Arc::new(
        OsExecutionEnv::with_cwd(expected.launch.cwd.clone()).with_environment(
            [(
                "HOME".to_string(),
                expected.launch.cwd.to_string_lossy().into_owned(),
            )]
            .into(),
        ),
    );
    OrdinaryBinding::new_deferred(
        env,
        Box::new(move |spec, env, facts| {
            validated.fetch_add(1, Ordering::SeqCst);
            assert_eq!(spec, &expected);
            assert_eq!(env.cwd(), expected.launch.cwd);
            assert_eq!(facts.metadata.id, expected.session.as_ref().unwrap().id);
            if mode == "validator-panic" {
                panic!("injected pure validation panic");
            }
            if mode == "validator-error" {
                return Err(BackendError::Binding("resource authority rejected"));
            }
            Ok(Box::pin(async move {
                assembled.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = gate {
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                }
                if mode == "panic" {
                    panic!("injected owned assembly panic");
                }
                if mode == "error" {
                    return Err(BackendError::Binding("owned assembly rejected"));
                }
                let mut options = AgentHarnessOptions {
                    model: provider.default_model().clone(),
                    models: vec![provider],
                    ..Default::default()
                };
                if mode == "bad-model" {
                    options.model.id = "not-the-registered-model".into();
                }
                if mode == "bad-tools" {
                    options.active_tool_names = vec!["not-granted".into()];
                }
                if mode == "bad-mode" {
                    options.drive = rpi_harness::types::DrivingMode::Manual;
                }
                Ok(options)
            }))
        }),
    )
    .unwrap()
}
pub(super) async fn writer_free(record: &AgentRecord) {
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let lease = env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    drop(lease);
}

#[tokio::test(flavor = "current_thread")]
async fn readonly_preflight_and_stale_guard_never_poll_deferred_assembly() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("deferred-pure", &provider, false).await;
    let v = Arc::new(AtomicUsize::new(0));
    let a = Arc::new(AtomicUsize::new(0));
    let path = &record.spec.session.as_ref().unwrap().path;
    let bytes = std::fs::read(path).unwrap();
    plan(&record, provider.clone(), v.clone(), a.clone(), None, "ok")
        .preflight_session(&record.spec)
        .await
        .unwrap();
    assert_eq!(v.load(Ordering::SeqCst), 1);
    assert_eq!(a.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.catalog.get(record.spec.id).await.unwrap(), record);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    writer_free(&record).await;
    let mut stale = fixture.catalog.guard(&record);
    stale.revision += 1;
    assert!(OrdinaryAgent::restore_bound(
        fixture.catalog.clone(),
        record.spec.id,
        stale,
        OrdinaryLimits::default(),
        plan(&record, provider.clone(), v.clone(), a.clone(), None, "ok")
    )
    .await
    .is_err());
    assert_eq!(v.load(Ordering::SeqCst), 1);
    assert_eq!(a.load(Ordering::SeqCst), 0);
    let owner = OrdinaryAgent::restore_bound(
        fixture.catalog.clone(),
        record.spec.id,
        fixture.catalog.guard(&record),
        OrdinaryLimits::default(),
        plan(&record, provider, v.clone(), a.clone(), None, "ok"),
    )
    .await
    .unwrap();
    assert_eq!(v.load(Ordering::SeqCst), 2);
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert!(owner
        .client()
        .snapshot()
        .await
        .unwrap()
        .record
        .state
        .is_resident());
    owner.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn failed_validation_and_assembly_do_not_repair_torn_history_or_keep_writer() {
    for mode in [
        "validator-error",
        "validator-panic",
        "error",
        "panic",
        "bad-model",
        "bad-tools",
        "bad-mode",
    ] {
        let fixture = Fixture::new(1).await;
        let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
        let record = fixture.record(mode, &provider, false).await;
        let path = &record.spec.session.as_ref().unwrap().path;
        let mut bytes = std::fs::read(path).unwrap();
        bytes.extend_from_slice(b"{\"torn\"");
        std::fs::write(path, &bytes).unwrap();
        let v = Arc::new(AtomicUsize::new(0));
        let a = Arc::new(AtomicUsize::new(0));
        let result = OrdinaryAgent::restore_bound(
            fixture.catalog.clone(),
            record.spec.id,
            fixture.catalog.guard(&record),
            OrdinaryLimits::default(),
            plan(&record, provider, v.clone(), a.clone(), None, mode),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(BackendError::Binding(_)) | Err(BackendError::TaskStopped)
            ),
            "{mode}"
        );
        assert_eq!(v.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.load(Ordering::SeqCst),
            usize::from(!mode.starts_with("validator"))
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes, "{mode}: no repair");
        assert_eq!(
            fixture.catalog.get(record.spec.id).await.unwrap().state,
            AgentState::Closed
        );
        writer_free(&record).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn service_keeps_owned_assembly_and_starting_capacity_after_waiter_abort() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("deferred-owned", &provider, false).await;
    let v = Arc::new(AtomicUsize::new(0));
    let a = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new();
    let binding = plan(&record, provider, v, a.clone(), Some(gate.clone()), "ok");
    let owned = service.clone();
    let id = record.spec.id;
    let guard = fixture.catalog.guard(&record);
    let caller = tokio::spawn(async move {
        owned
            .restore_bound(id, guard, OrdinaryLimits::default(), binding)
            .await
    });
    tokio::time::timeout(DEADLINE, gate.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(service.phase(id), Some(OwnerPhase::Starting));
    assert_eq!(
        fixture.catalog.get(id).await.unwrap().state,
        AgentState::Starting
    );
    writer_free(&record).await;
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    assert_eq!(service.phase(id), Some(OwnerPhase::Starting));
    assert_eq!(a.load(Ordering::SeqCst), 1);
    gate.release.add_permits(1);
    wait_phase(&service, id, Some(OwnerPhase::Live)).await;
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
    service.close_ordinary(id).await.unwrap();
    writer_free(&record).await;
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_saved_identity_is_rechecked_after_successful_readonly_preflight() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("fresh-facts", &provider, false).await;
    let v = Arc::new(AtomicUsize::new(0));
    let a = Arc::new(AtomicUsize::new(0));
    plan(&record, provider.clone(), v.clone(), a.clone(), None, "ok")
        .preflight_session(&record.spec)
        .await
        .unwrap();
    let path = &record.spec.session.as_ref().unwrap().path;
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let mut header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["id"] = serde_json::json!("changed-after-readonly-preflight");
    let bytes = format!(
        "{}\n{}",
        serde_json::to_string(&header).unwrap(),
        lines.map(|line| format!("{line}\n")).collect::<String>()
    )
    .into_bytes();
    std::fs::write(path, &bytes).unwrap();
    assert!(OrdinaryAgent::restore_bound(
        fixture.catalog.clone(),
        record.spec.id,
        fixture.catalog.guard(&record),
        OrdinaryLimits::default(),
        plan(&record, provider, v.clone(), a.clone(), None, "ok")
    )
    .await
    .is_err());
    assert_eq!(v.load(Ordering::SeqCst), 1);
    assert_eq!(a.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    writer_free(&record).await;
}
