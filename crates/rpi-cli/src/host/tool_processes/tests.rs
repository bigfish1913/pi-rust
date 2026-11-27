use super::*;
use crate::resource_management::{mutation_lease, shared_lease};
use std::io::BufRead;
use std::process::Stdio;

fn command(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}
fn observed(job: &mut PinnedProcessGroup) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = job.foreground_exit().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "owned foreground did not exit");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn settle(job: &mut PinnedProcessGroup) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match job.terminate_and_reap(Duration::from_millis(200)) {
            Ok(status) => return status,
            Err(error) if Instant::now() < deadline => {
                // Census membership may disappear between list and detail;
                // retry still owns the exact waitable child and real guard.
                eprintln!("owned fixture census retry: {error}");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("owned fixture did not settle: {error}"),
        }
    }
}
fn resources(root: &std::path::Path) -> Arc<dyn Send + Sync> {
    Arc::new(vec![shared_lease(root).unwrap()])
}

#[test]
fn foreground_exit_keeps_pid_waitable_and_real_lease_until_explicit_group_exit_and_reap() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut job = PinnedProcessGroup::spawn(&mut command("exit 7"), resources(&root)).unwrap();
    assert_eq!(observed(&mut job).code(), Some(7));
    assert_eq!(job.foreground_exit().unwrap().unwrap().code(), Some(7));
    let pid = job.pid().unwrap();
    assert!(group_members(pid)
        .unwrap()
        .iter()
        .any(|m| m.pid == pid && m.status == 5));
    assert!(mutation_lease(&root).is_err());
    assert_eq!(settle(&mut job).code(), Some(7));
    assert!(mutation_lease(&root).is_ok());
    assert!(job.foreground_exit().is_err());
    assert!(
        job.signal(Signal::SIGKILL).is_err(),
        "no numeric signal after reap"
    );
}

#[test]
fn exited_foreground_does_not_hide_a_live_background_group_member() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut job = PinnedProcessGroup::spawn(
        &mut command("/bin/sleep 30 & printf '%s\\n' \"$!\"; exit 0"),
        resources(&root),
    )
    .unwrap();
    let stdout = job.custody.as_mut().unwrap().child.stdout.take().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .unwrap();
    let background: i32 = line.trim().parse().unwrap();
    assert!(observed(&mut job).success());
    let group = job.pid().unwrap();
    assert!(group_members(group)
        .unwrap()
        .iter()
        .any(|m| m.pid == background && m.status != 5));
    assert!(mutation_lease(&root).is_err());
    assert!(settle(&mut job).success());
    // The original foreground result is preserved; the background really exited.
    // Any remaining original-group entry can only be an exited zombie, not code
    // still able to mutate resources. No arbitrary PID receives a fallback kill.
    assert!(group_members(group).unwrap().iter().all(|m| m.status == 5));
    assert!(mutation_lease(&root).is_ok());
}

#[test]
fn missing_wait_authority_revokes_signals_and_drop_retains_custody_instead_of_releasing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut job = PinnedProcessGroup::spawn(&mut command("exit 0"), resources(&root)).unwrap();
    observed(&mut job);
    // Deliberately violate exclusive-wait ownership, without any descendant.
    // Production exposes no Child or try_wait accessor that could do this.
    job.custody.as_mut().unwrap().child.wait().unwrap();
    assert!(job.foreground_exit().is_err());
    assert!(job.lost_wait_authority);
    assert!(job.signal(Signal::SIGKILL).is_err());
    let pid = job.pid().unwrap();
    drop(job);
    assert!(mutation_lease(&root).is_err());
    // Reclaim only this test's already-reaped, descendant-free fixture. This is
    // not a production Unknown reset or fabricated group-settlement capability.
    let custody = {
        let mut retained = RETAINED.lock().unwrap();
        let slot = retained
            .iter()
            .position(|c| c.custody.child.id() == pid as u32)
            .unwrap();
        retained.swap_remove(slot)
    };
    assert!(
        custody.lost_wait_authority,
        "revocation evidence survives Drop"
    );
    drop(custody);
    assert!(mutation_lease(&root).is_ok());
}

#[test]
fn stopped_cleanup_timeout_retains_guard_and_can_retry_real_exit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut job =
        PinnedProcessGroup::spawn(&mut command("exec /bin/sleep 30"), resources(&root)).unwrap();
    assert!(job.foreground_exit().unwrap().is_none());
    // The real kernel snapshot must report truncation conservatively too.
    let mut row = NativeMember::default();
    let mut count = 0;
    // SAFETY: capacity is exactly one valid initialized output record.
    let error = unsafe { rpi_host_process_census(job.pid().unwrap(), &mut row, 1, &mut count) };
    assert_eq!(error, libc::EOVERFLOW);
    assert_eq!(count, 0);
    assert_eq!(
        job.terminate_and_reap(Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(mutation_lease(&root).is_err());
    assert_eq!(settle(&mut job).signal(), Some(libc::SIGKILL));
    assert!(mutation_lease(&root).is_ok());
}

#[test]
fn terminating_one_group_does_not_signal_another_owned_group() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut first =
        PinnedProcessGroup::spawn(&mut command("exec /bin/sleep 30"), resources(&root)).unwrap();
    let mut other =
        PinnedProcessGroup::spawn(&mut command("exec /bin/sleep 30"), resources(&root)).unwrap();
    assert_ne!(first.pid().unwrap(), other.pid().unwrap());
    assert_eq!(settle(&mut first).signal(), Some(libc::SIGKILL));
    assert!(other.foreground_exit().unwrap().is_none());
    assert!(
        mutation_lease(&root).is_err(),
        "other real owner still holds the shared resource"
    );
    assert_eq!(settle(&mut other).signal(), Some(libc::SIGKILL));
    assert!(mutation_lease(&root).is_ok());
}

#[test]
fn spawn_failure_does_not_leave_a_child_or_retain_the_resource_guard() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut missing = Command::new(root.join("missing-owned-process-fixture"));
    assert!(PinnedProcessGroup::spawn(&mut missing, resources(&root)).is_err());
    assert!(mutation_lease(&root).is_ok());
}

#[test]
fn dropped_waitable_child_and_guard_remain_together_until_actual_test_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut job = PinnedProcessGroup::spawn(&mut command("exit 0"), resources(&root)).unwrap();
    observed(&mut job);
    let pid = job.pid().unwrap();
    drop(job);
    assert!(mutation_lease(&root).is_err());
    let retained = {
        let mut retained = RETAINED.lock().unwrap();
        let slot = retained
            .iter()
            .position(|c| c.custody.child.id() == pid as u32)
            .unwrap();
        retained.swap_remove(slot)
    };
    assert!(!retained.lost_wait_authority);
    // Test-only transfer of the still-valid custody; production exposes no
    // reset/recovery API. Even here only genuine census+exit+reap may unlock.
    let mut recovered = PinnedProcessGroup {
        custody: Some(retained.custody),
        lost_wait_authority: retained.lost_wait_authority,
    };
    assert!(recovered.foreground_exit().unwrap().unwrap().success());
    assert!(mutation_lease(&root).is_err());
    assert!(settle(&mut recovered).success());
    assert!(mutation_lease(&root).is_ok());
}
