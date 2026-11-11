#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
    use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
    use rpi_harness::session::types::*;
    use rpi_tools::env::AppendFile;
    use rpi_tools::{FileContent, FileError, FileInfo, FileSystem};
    use std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    fn entry(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(id, 0).into(),
                terminate: None,
            },
        }
    }
    fn base(id: &str) -> RecordBase {
        RecordBase {
            id: id.into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        }
    }
    fn frame(id: &str) -> LaneRecord {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: base(id),
            run_id: "run".into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        })
    }
    fn abort(id: &str) -> LaneRecord {
        LaneRecord::AbortRequested(AbortRequestedRecord {
            base: base(id),
            run_id: "run".into(),
        })
    }
    fn header() -> JsonlV4Header {
        JsonlV4Header::new("independent".into(), 1, "/".into(), None, None, None)
    }
    #[derive(Default)]
    struct Gate {
        armed: AtomicBool,
        fail_after_write: AtomicBool,
        entered: Notify,
        release: Notify,
    }
    struct GatedFs {
        inner: Arc<dyn FileSystem>,
        gate: Arc<Gate>,
    }
    struct GatedAppend {
        inner: Arc<dyn FileSystem>,
        path: String,
        gate: Arc<Gate>,
    }
    #[async_trait]
    impl AppendFile for GatedAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            self.inner
                .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
                .await?;
            if self.gate.fail_after_write.swap(false, Ordering::SeqCst) {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled failure after bytes became visible",
                ));
            }
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            Ok(())
        }
    }
    #[async_trait]
    impl FileSystem for GatedFs {
        fn cwd(&self) -> &Path {
            self.inner.cwd()
        }
        async fn absolute_path(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.absolute_path(p, c).await
        }
        async fn join_path(
            &self,
            p: &[&str],
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.join_path(p, c).await
        }
        async fn read_text_file(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<String, FileError> {
            self.inner.read_text_file(p, c).await
        }
        async fn read_text_lines(
            &self,
            p: &str,
            n: Option<usize>,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<String>, FileError> {
            self.inner.read_text_lines(p, n, c).await
        }
        async fn read_binary_file(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<u8>, FileError> {
            self.inner.read_binary_file(p, c).await
        }
        async fn write_file(
            &self,
            p: &str,
            b: FileContent,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.write_file(p, b, c).await
        }
        async fn append_file(
            &self,
            p: &str,
            b: FileContent,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.append_file(p, b, c).await
        }
        async fn open_append_file(
            &self,
            p: &str,
        ) -> Result<Option<Box<dyn AppendFile>>, FileError> {
            Ok(Some(Box::new(GatedAppend {
                inner: self.inner.clone(),
                path: p.into(),
                gate: self.gate.clone(),
            })))
        }
        async fn rename_file(
            &self,
            p: &str,
            q: &str,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.rename_file(p, q, c).await
        }
        async fn file_info(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<FileInfo, FileError> {
            self.inner.file_info(p, c).await
        }
        async fn list_dir(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<Vec<FileInfo>, FileError> {
            self.inner.list_dir(p, c).await
        }
        async fn canonical_path(
            &self,
            p: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.canonical_path(p, c).await
        }
        async fn exists(&self, p: &str, c: Option<&CancellationToken>) -> Result<bool, FileError> {
            self.inner.exists(p, c).await
        }
        async fn create_dir(
            &self,
            p: &str,
            b: bool,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.create_dir(p, b, c).await
        }
        async fn remove(
            &self,
            p: &str,
            b: bool,
            f: bool,
            c: Option<&CancellationToken>,
        ) -> Result<(), FileError> {
            self.inner.remove(p, b, f, c).await
        }
        async fn create_temp_dir(
            &self,
            p: Option<&str>,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.create_temp_dir(p, c).await
        }
        async fn create_temp_file(
            &self,
            p: &str,
            s: &str,
            c: Option<&CancellationToken>,
        ) -> Result<PathBuf, FileError> {
            self.inner.create_temp_file(p, s, c).await
        }
        async fn cleanup(&self) {
            self.inner.cleanup().await;
        }
    }

    async fn cancel_shape(kind: &'static str) {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let s = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                "/test.jsonl",
                header(),
                clock.clone(),
                ids.clone(),
            )
            .await
            .unwrap(),
        );
        s.append_entry(entry("seed"), "main").await.unwrap();
        s.append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: Some("seed".into()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
        if kind == "move" {
            s.create_lane("side", None).await.unwrap();
        }
        gate.armed.store(true, Ordering::SeqCst);
        let job = tokio::spawn({
            let s = s.clone();
            async move {
                match kind {
                    "entry" => s.append_entry(entry("cancelled"), "main").await.map(|_| ()),
                    "frames" => s
                        .append_records(vec![frame("f1"), frame("f2")])
                        .await
                        .map(|_| ()),
                    "mixed" => s
                        .append_records(vec![frame("f1"), abort("a1")])
                        .await
                        .map(|_| ()),
                    "pair" => s
                        .append_record_and_entry(abort("before"), entry("cancelled"), "main")
                        .await
                        .map(|_| ()),
                    "triple" => s
                        .append_record_entry_and_record(
                            abort("before"),
                            entry("cancelled"),
                            "main",
                            abort("after"),
                        )
                        .await
                        .map(|_| ()),
                    "name" => s.set_name(Some("kept name")).await,
                    "label" => s.set_label("seed", Some("kept label")).await,
                    "lane" => s.create_lane("created", Some("seed")).await,
                    "move" => s.move_lane("side", Some("seed")).await,
                    _ => unreachable!(),
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect(kind);
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        gate.release.notify_one();
        tokio::time::timeout(
            Duration::from_secs(5),
            s.append_entry(entry("acknowledged"), "main"),
        )
        .await
        .expect(kind)
        .unwrap();
        let loaded = JsonlSessionStorage::load(fs.clone(), "/test.jsonl", clock, ids)
            .await
            .expect(kind);
        assert!(
            loaded.get_entry("acknowledged").await.unwrap().is_some(),
            "{kind}"
        );
        assert_eq!(
            s.get_log(&LogOptions::default()).await.unwrap(),
            loaded.get_log(&LogOptions::default()).await.unwrap(),
            "{kind}"
        );
        let lines = fs.read_text_file("/test.jsonl", None).await.unwrap();
        let seqs: Vec<_> = lines
            .lines()
            .skip(1)
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert!(
            seqs.windows(2).all(|p| p[0] + 1 == p[1]),
            "{kind}: {seqs:?}"
        );
        match kind {
            "name" => assert_eq!(
                loaded.get_name().await.unwrap().as_deref(),
                Some("kept name")
            ),
            "label" => assert_eq!(
                loaded.get_label("seed").await.unwrap().as_deref(),
                Some("kept label")
            ),
            "lane" | "move" => assert!(loaded.get_lanes().await.unwrap().iter().any(|l| l.lane
                == if kind == "lane" { "created" } else { "side" }
                && l.leaf_id.as_deref() == Some("seed"))),
            _ => {}
        }
    }
    #[tokio::test]
    async fn all_jsonl_write_shapes_survive_post_write_cancellation() {
        for kind in [
            "entry", "frames", "mixed", "pair", "triple", "name", "label", "lane", "move",
        ] {
            cancel_shape(kind).await;
        }
    }

    #[tokio::test]
    async fn jsonl_drain_waits_for_an_admitted_cancelled_write_before_worker_first_poll() {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let s = JsonlSessionStorage::create(
            fs,
            "/test.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        s.append_entry(entry("seed"), "main").await.unwrap();
        gate.armed.store(true, Ordering::SeqCst);
        let mut admitted = Box::pin(s.append_entry(entry("admitted"), "main"));
        assert!(futures::poll!(admitted.as_mut()).is_pending());
        drop(admitted);
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), s.drain())
            .await
            .unwrap();
        assert!(
            s.get_entry("admitted").await.unwrap().is_some(),
            "drain returned before the admitted owner's reducer commit"
        );
    }

    #[tokio::test]
    async fn jsonl_ambiguous_append_failure_cannot_poison_a_later_success() {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let s = JsonlSessionStorage::create(
            fs.clone(),
            "/test.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        s.append_entry(entry("seed"), "main").await.unwrap();
        gate.fail_after_write.store(true, Ordering::SeqCst);
        assert!(s.append_entry(entry("uncertain"), "main").await.is_err());
        if s.append_entry(entry("acknowledged"), "main").await.is_ok() {
            let loaded = JsonlSessionStorage::load(fs, "/test.jsonl", clock, ids)
                .await
                .expect("later successful append must leave a readable journal");
            assert!(loaded.get_entry("acknowledged").await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn jsonl_cancelled_tail_waiter_is_not_left_running() {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let s = Arc::new(
            JsonlSessionStorage::create(
                fs,
                "/test.jsonl",
                header(),
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )
            .await
            .unwrap(),
        );
        s.append_entry(entry("seed"), "main").await.unwrap();
        gate.armed.store(true, Ordering::SeqCst);
        let active = tokio::spawn({
            let s = s.clone();
            async move { s.append_entry(entry("active"), "main").await }
        });
        gate.entered.notified().await;
        let mut waiting = Box::pin(s.append_entry(entry("unadmitted"), "main"));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(waiting);
        gate.release.notify_one();
        active.await.unwrap().unwrap();
        s.drain().await;
        assert!(s.get_entry("unadmitted").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn jsonl_drain_orders_after_an_earlier_tail_waiter() {
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let s = Arc::new(
            JsonlSessionStorage::create(
                fs,
                "/test.jsonl",
                header(),
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )
            .await
            .unwrap(),
        );
        s.append_entry(entry("seed"), "main").await.unwrap();
        gate.armed.store(true, Ordering::SeqCst);
        let active = tokio::spawn({
            let s = s.clone();
            async move { s.append_entry(entry("active"), "main").await }
        });
        gate.entered.notified().await;
        let mut queued = Box::pin({
            let s = s.clone();
            async move { s.append_entry(entry("queued"), "main").await }
        });
        assert!(futures::poll!(queued.as_mut()).is_pending());
        let queued = tokio::spawn(queued);
        let mut drain = Box::pin(s.drain());
        assert!(futures::poll!(drain.as_mut()).is_pending());
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap();
        assert!(
            s.get_entry("queued").await.unwrap().is_some(),
            "drain skipped an earlier tail waiter"
        );
        active.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn jsonl_conflict_after_an_exact_duplicate_never_rewrites_file() {
        for same_id in [false, true] {
            let fs: Arc<dyn FileSystem> =
                Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
            let clock = Arc::new(FakeClock::new());
            let ids = Arc::new(CounterIdGenerator::new());
            let s = JsonlSessionStorage::create(
                fs.clone(),
                "/bad.jsonl",
                header(),
                clock.clone(),
                ids.clone(),
            )
            .await
            .unwrap();
            s.append_entry(entry("first"), "main").await.unwrap();
            let original = fs.read_text_file("/bad.jsonl", None).await.unwrap();
            let line = original.lines().nth(1).unwrap();
            let mut conflicting: serde_json::Value = serde_json::from_str(line).unwrap();
            if same_id {
                conflicting["message"]["content"] = serde_json::json!("different payload");
            } else {
                conflicting["id"] = serde_json::json!("different-id");
            }
            assert_ne!(
                rpi_harness::session::jsonl::codec::parse_mutation(line).unwrap(),
                rpi_harness::session::jsonl::codec::parse_mutation(&conflicting.to_string())
                    .unwrap()
            );
            let raw = format!("{original}{line}\n{conflicting}\n");
            fs.write_file("/bad.jsonl", FileContent::Text(raw.clone()), None)
                .await
                .unwrap();
            assert!(
                JsonlSessionStorage::load(fs.clone(), "/bad.jsonl", clock, ids)
                    .await
                    .is_err(),
                "same_id={same_id}"
            );
            assert_eq!(
                fs.read_text_file("/bad.jsonl", None).await.unwrap(),
                raw,
                "same_id={same_id}"
            );
        }
    }

    #[tokio::test]
    async fn jsonl_exact_duplicate_is_checked_before_parent_normalization() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let s = JsonlSessionStorage::create(
            fs.clone(),
            "/normalized.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        s.append_entry(entry("first"), "main").await.unwrap();
        s.append_entry(entry("tail"), "main").await.unwrap();
        let original = fs.read_text_file("/normalized.jsonl", None).await.unwrap();
        let lines: Vec<_> = original.lines().collect();
        let mut changed: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        changed["parentId"] = serde_json::Value::Null;
        let raw = format!("{}\n{}\n{changed}\n{changed}\n", lines[0], lines[1]);
        fs.write_file("/normalized.jsonl", FileContent::Text(raw), None)
            .await
            .unwrap();
        let loaded = JsonlSessionStorage::load(fs, "/normalized.jsonl", clock, ids)
            .await
            .unwrap();
        assert!(loaded.get_entry("tail").await.unwrap().is_some());
    }
}
