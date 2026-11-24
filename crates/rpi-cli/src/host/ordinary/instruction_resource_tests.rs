// Included by ordinary::tests::owner_registry::admission_faults.
mod instruction_resources {
    use super::*;
    use crate::host::NativeInstructionResources;
    use serde_json::json;

    fn resources(root: &Path) -> Arc<NativeInstructionResources> {
        let path = root.join("instructions.md");
        std::fs::write(&path, "frozen {{instruction}} text").unwrap();
        Arc::new(
            NativeInstructionResources::freeze(
                serde_json::from_value(json!({
                    "version":1,"tools":{"mode":"none"},"instructions":[path]
                }))
                .unwrap(),
            )
            .unwrap(),
        )
    }
    #[test]
    fn frozen_instructions_configure_raw_prompt_without_harness_or_session() {
        let root = tempfile::tempdir().unwrap();
        let frozen = resources(root.path());
        let mut prompt = Some("base {{literal}}".to_string());
        let mut provenance = rpi_harness::types::AgentHarnessResources::empty();
        frozen.configure_parts(&mut prompt, &mut provenance);
        assert_eq!(
            prompt.as_deref(),
            Some("base {{literal}}\n\nfrozen {{instruction}} text")
        );
        assert_eq!(
            provenance.context_files,
            vec![root
                .path()
                .join("instructions.md")
                .to_string_lossy()
                .to_string()]
        );
        assert!(provenance.skills.is_none());
        assert!(provenance.prompt_templates.is_none());
    }
    async fn record(
        fixture: &Fixture,
        provider: &FauxProvider,
        resources: &NativeInstructionResources,
    ) -> AgentRecord {
        let mut spec = fixture.spec("instructions", provider, false).await;
        spec.launch.resource_profile = Some(resources.identity().into());
        fixture.catalog.register(spec).await.unwrap()
    }
    fn binding(
        record: &AgentRecord,
        provider: Arc<FauxProvider>,
        resources: Arc<NativeInstructionResources>,
        fault: Arc<SyncFault>,
    ) -> OrdinaryBinding {
        let native = binder(provider, None, None);
        let frozen = resources.clone();
        OrdinaryBinding {
            environment: None,
            binder: BindingResolver::Immediate(Box::new(move |spec, env, facts| {
                let mut options = native(spec, env, facts)?;
                resources.configure_parts(&mut options.system_prompt, &mut options.resources);
                Ok(options)
            })),
            instruction_resources: Some(Arc::new(
                crate::host::NativeBindingResources::Instructions(frozen),
            )),
            extensions: None,
            filesystem: Some(Arc::new(FaultFs {
                inner: Arc::new(OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone())),
                fault,
            })),
        }
    }
    fn binding_count(record: &AgentRecord) -> usize {
        std::str::from_utf8(&bytes(record))
            .unwrap()
            .matches("rpi.resource-selection.v1")
            .count()
    }
    async fn writer_free(record: &AgentRecord) {
        let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
        assert!(env
            .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_resource_sync_is_owned_and_starting_until_proved_even_after_waiter_abort() {
        let fixture = Fixture::new(1).await;
        let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
        let resources = resources(fixture.root.path());
        let record = record(&fixture, &provider, &resources).await;
        let fault = SyncFault::new(false);
        fault.block_once.store(true, Ordering::SeqCst);
        let service = HostService::new(fixture.catalog.clone());
        let prepared = binding(&record, provider.clone(), resources.clone(), fault.clone());
        // Readonly preflight must not append, sync, repair or take a lease.
        let before = bytes(&record);
        prepared.preflight_session(&record.spec).await.unwrap();
        assert_eq!(bytes(&record), before);
        assert_eq!(fault.calls.load(Ordering::SeqCst), 0);
        writer_free(&record).await;
        let prepared = binding(&record, provider, resources, fault.clone());
        let runner = service.clone();
        let id = record.spec.id;
        let guard = fixture.catalog.guard(&record);
        let waiter = tokio::spawn(async move {
            runner
                .restore_bound(id, guard, OrdinaryLimits::default(), prepared)
                .await
        });
        tokio::time::timeout(DEADLINE, fault.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(
            fixture.catalog.get(id).await.unwrap().state,
            AgentState::Starting
        );
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert_eq!(service.owner_count(), 1);
        assert_eq!(binding_count(&record), 1);
        assert!(!String::from_utf8_lossy(&bytes(&record)).contains("operation_started"));
        let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
        assert!(env
            .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
            .await
            .is_err());
        waiter.abort();
        let _ = waiter.await;
        fault.release.add_permits(1);
        tokio::time::timeout(DEADLINE, async {
            while service.phase(id) != Some(crate::host::service::OwnerPhase::Live) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fixture.catalog.get(id).await.unwrap().state,
            AgentState::Idle
        );
        assert_eq!(fault.calls.load(Ordering::SeqCst), 1);
        service.close_ordinary(id).await.unwrap();
        writer_free(&record).await;
        service.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn visible_resource_binding_after_sync_failure_is_reproved_without_duplicate_append() {
        let fixture = Fixture::new(1).await;
        let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
        let resources = resources(fixture.root.path());
        let initial = record(&fixture, &provider, &resources).await;
        let fault = SyncFault::new(true);
        let service = HostService::new(fixture.catalog.clone());
        let prepared = binding(&initial, provider.clone(), resources.clone(), fault.clone());
        assert!(service
            .restore_bound(
                initial.spec.id,
                fixture.catalog.guard(&initial),
                OrdinaryLimits::default(),
                prepared
            )
            .await
            .is_err());
        assert_eq!(service.owner_count(), 0);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
        assert_eq!(binding_count(&initial), 1);
        assert!(!String::from_utf8_lossy(&bytes(&initial)).contains("operation_started"));
        writer_free(&initial).await;
        let closed = fixture.catalog.get(initial.spec.id).await.unwrap();
        assert_eq!(closed.state, AgentState::Closed);
        fault.fail.store(false, Ordering::SeqCst);
        let prepared = binding(&closed, provider.clone(), resources.clone(), fault.clone());
        service
            .restore_bound(
                closed.spec.id,
                fixture.catalog.guard(&closed),
                OrdinaryLimits::default(),
                prepared,
            )
            .await
            .unwrap();
        assert_eq!(binding_count(&closed), 1);
        assert_eq!(fault.calls.load(Ordering::SeqCst), 2);
        service.close_ordinary(closed.spec.id).await.unwrap();
        let calls_before_rejection = fault.calls.load(Ordering::SeqCst);
        // A corrupted schema inventory must fail before torn-tail repair or sync.
        let path = &closed.spec.session.as_ref().unwrap().path;
        let text = String::from_utf8(bytes(&closed)).unwrap();
        let mut lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for line in &mut lines {
            if line.pointer("/data/key") == Some(&json!("rpi.resource-selection.v1")) {
                line["data"]["value"]["inventory"]["toolSchemasSha256"] = json!("0".repeat(64));
            }
        }
        let mut corrupted = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        corrupted.push_str("{torn");
        std::fs::write(path, &corrupted).unwrap();
        let closed = fixture.catalog.get(closed.spec.id).await.unwrap();
        let prepared = binding(&closed, provider, resources, fault.clone());
        assert!(prepared.preflight_session(&closed.spec).await.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), corrupted);
        assert_eq!(fault.calls.load(Ordering::SeqCst), calls_before_rejection);
        writer_free(&closed).await;
        service.shutdown().await.unwrap();
    }
}
