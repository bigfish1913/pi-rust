//! Approval flow for the sandbox-aware execution environment.
//!
//! Uses the real TUI bridge but a fake broker: the tool must ask, the test
//! answers, and only then may the write happen.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_cli::sandbox_approval::{install, TuiApprovalBridge};
use rpi_cli::sandbox_env::wrap;
use rpi_sandbox::{
    compile_policy, ApprovalScope, PolicyContext, PolicyOverrides, SandboxMode, SandboxPolicy,
    SandboxSettings, WriteBroker, WriteOp,
};
use rpi_tools::{FileContent, OsExecutionEnv};

#[derive(Default)]
struct FakeBroker {
    approvals: Mutex<Vec<(PathBuf, ApprovalScope)>>,
    writes: Mutex<Vec<PathBuf>>,
}

impl WriteBroker for FakeBroker {
    fn approve(
        &self,
        path: &Path,
        operations: &[WriteOp],
        scope: ApprovalScope,
        _recursive: bool,
    ) -> Result<(), String> {
        assert!(!operations.is_empty());
        self.approvals
            .lock()
            .unwrap()
            .push((path.to_path_buf(), scope));
        Ok(())
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, data).map_err(|error| error.to_string())?;
        self.writes.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }

    fn revoke(&self, _path: &Path, _recursive: bool) -> Result<(), String> {
        Ok(())
    }

    fn append(&self, _path: &Path, _data: &[u8]) -> Result<(), String> {
        Err("unexpected append".into())
    }
    fn truncate(&self, _path: &Path, _len: u64) -> Result<(), String> {
        Err("unexpected truncate".into())
    }
    fn delete(&self, _path: &Path) -> Result<(), String> {
        Err("unexpected delete".into())
    }
    fn rename(&self, _path: &Path, _target: &Path) -> Result<(), String> {
        Err("unexpected rename".into())
    }
    fn mkdir(&self, _path: &Path) -> Result<(), String> {
        Err("unexpected mkdir".into())
    }
}

fn policy(allowed: &Path, temp: &Path) -> Arc<SandboxPolicy> {
    let context = PolicyContext {
        cwd: allowed.to_path_buf(),
        home: Some(allowed.to_path_buf()),
        temp: temp.to_path_buf(),
    };
    let settings = SandboxSettings {
        enabled: Some(true),
        mode: Some(SandboxMode::Auto),
        ..SandboxSettings::default()
    };
    let compiled = compile_policy(
        Some(&settings),
        None,
        &PolicyOverrides::default(),
        &context,
        &[],
    );
    Arc::new(compiled)
}

async fn next_job(bridge: &Arc<TuiApprovalBridge>) -> rpi_cli::sandbox_approval::ApprovalJob {
    for _ in 0..600 {
        if let Some(job) = bridge.take_pending() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("approval job never appeared");
}

#[tokio::test(flavor = "multi_thread")]
async fn approved_write_uses_the_broker_and_denied_write_stays_put() {
    let allowed = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let policy = policy(allowed.path(), temp.path());
    let base = Arc::new(OsExecutionEnv::with_cwd(allowed.path().to_path_buf()));
    let broker = Arc::new(FakeBroker::default());
    let (env, _) = wrap(base, policy, broker.clone());
    let bridge = install();

    // ---- allow once ----
    let target = outside.path().join("approved.txt");
    let env_task = env.clone();
    let target_task = target.clone();
    let task = tokio::spawn(async move {
        env_task
            .write_file(
                target_task.to_str().unwrap(),
                FileContent::Text("approved".to_string()),
                None,
            )
            .await
    });
    let job = next_job(&bridge).await;
    assert_eq!(job.request.operation, WriteOp::Create);
    assert_eq!(job.request.path, target);
    job.respond
        .send(rpi_sandbox::ApprovalDecision::Allow {
            scope: ApprovalScope::Once,
            recursive: false,
        })
        .unwrap();
    task.await.unwrap().expect("approved write must succeed");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved");
    assert_eq!(broker.writes.lock().unwrap().len(), 1);
    assert_eq!(broker.approvals.lock().unwrap().len(), 1);

    // ---- deny ----
    let denied = outside.path().join("denied.txt");
    let env_task = env.clone();
    let target_task = denied.clone();
    let task = tokio::spawn(async move {
        env_task
            .write_file(
                target_task.to_str().unwrap(),
                FileContent::Text("no".to_string()),
                None,
            )
            .await
    });
    let job = next_job(&bridge).await;
    job.respond
        .send(rpi_sandbox::ApprovalDecision::Deny)
        .unwrap();
    let error = task.await.unwrap().expect_err("denied write must fail");
    assert_eq!(error.code, rpi_tools::FileErrorCode::PermissionDenied);
    assert!(!denied.exists());
    assert_eq!(broker.writes.lock().unwrap().len(), 1);
}
