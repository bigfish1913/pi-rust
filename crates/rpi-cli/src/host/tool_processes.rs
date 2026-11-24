//! Staged Darwin owned-process-group primitive. NOT a full CLI cleanup receipt.
//!
//! Keep the direct Child waitable (WNOWAIT) until the group has exited: merely
//! remembering its numeric PID after wait()/try_wait() would permit reuse.
//! This covers the original group only, not setsid/setpgid escape or parent death.
//! It assumes exclusive wait ownership; unexpected ECHILD permanently revokes
//! signal authority. A lost owner is retained, including its real resource guard.
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use nix::{
    libc,
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use std::os::unix::process::{CommandExt, ExitStatusExt};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeMember {
    pid: i32,
    status: u32,
    start_sec: u64,
    start_usec: u64,
}
extern "C" {
    fn rpi_host_process_census(
        group: i32,
        members: *mut NativeMember,
        capacity: usize,
        written: *mut usize,
    ) -> i32;
}

const MAX_PINS: usize = 64;
const MAX_MEMBERS: usize = 4096;
static PINS: AtomicUsize = AtomicUsize::new(0);
static RETAINED: LazyLock<Mutex<Vec<RetainedCustody>>> = LazyLock::new(|| Mutex::new(Vec::new()));
struct PinPermit;
impl PinPermit {
    fn acquire() -> io::Result<Self> {
        let mut current = PINS.load(Ordering::Acquire);
        loop {
            if current >= MAX_PINS {
                return Err(io::Error::other("owned process custody capacity reached"));
            }
            match PINS.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(observed) => current = observed,
            }
        }
    }
}
impl Drop for PinPermit {
    fn drop(&mut self) {
        PINS.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Custody {
    child: Child,
    _resources: Arc<dyn Send + Sync>,
    _permit: PinPermit,
}
struct RetainedCustody {
    custody: Custody,
    lost_wait_authority: bool,
}
pub(crate) struct PinnedProcessGroup {
    custody: Option<Custody>,
    lost_wait_authority: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Member {
    pid: i32,
    status: u32,
    start: (u64, u64),
}
impl Drop for PinnedProcessGroup {
    fn drop(&mut self) {
        if let Some(custody) = self.custody.take() {
            // No guessed signal and no unproved resource release from Drop.
            // Capacity was reserved before spawn, so this evidence is bounded.
            RETAINED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(RetainedCustody {
                    custody,
                    lost_wait_authority: self.lost_wait_authority,
                });
        }
    }
}
impl PinnedProcessGroup {
    pub(crate) fn spawn(
        command: &mut Command,
        resources: Arc<dyn Send + Sync>,
    ) -> io::Result<Self> {
        let permit = PinPermit::acquire()?;
        command.process_group(0);
        let child = command.spawn()?;
        Ok(Self {
            custody: Some(Custody {
                child,
                _resources: resources,
                _permit: permit,
            }),
            lost_wait_authority: false,
        })
    }
    fn pid(&self) -> io::Result<i32> {
        self.custody
            .as_ref()
            .map(|c| c.child.id() as i32)
            .ok_or_else(|| io::Error::other("owned process group already released"))
    }
    /// Observe the foreground result without freeing its PID for another job.
    pub(crate) fn foreground_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.lost_wait_authority {
            return Err(io::Error::other("owned child wait authority lost"));
        }
        let pid = self.pid()?;
        // SAFETY: siginfo_t is a C output structure whose zero representation is
        // valid; waitid writes only within it. P_PID selects our exclusively held
        // Child, WNOWAIT explicitly leaves it waitable, WNOHANG never blocks.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.lost_wait_authority = true;
            }
            return Err(error);
        }
        if info.si_pid == 0 {
            return Ok(None);
        }
        if info.si_pid != pid {
            return Err(io::Error::other("unexpected waitid child identity"));
        }
        let raw = match info.si_code {
            libc::CLD_EXITED => info.si_status << 8,
            libc::CLD_KILLED => info.si_status,
            libc::CLD_DUMPED => info.si_status | 0x80,
            // Darwin reports a pending stop even with WEXITED alone. A stop,
            // trap or continue never supplies foreground completion evidence;
            // it must not prevent the keeper from finishing the frozen cohort.
            libc::CLD_STOPPED | libc::CLD_TRAPPED | libc::CLD_CONTINUED => return Ok(None),
            _ => return Err(io::Error::other("unexpected foreground wait state")),
        };
        Ok(Some(ExitStatus::from_raw(raw)))
    }
    fn signal(&mut self, signal: Signal) -> io::Result<()> {
        // Recheck exclusive wait ownership before every signal. Never fall back
        // to an arbitrary PID, and never signal after final reap/ECHILD.
        self.foreground_exit()?;
        killpg(Pid::from_raw(self.pid()?), signal).map_err(io::Error::from)
    }
    /// Blocking keeper operation; do not run on a Tokio executor thread.
    /// Freeze a stable cohort before killing: while all its members are stopped
    /// or exited and its membership/identities are unchanged, no member can fork.
    /// External SIGCONT/group joins and escaped descendants are NOT a sandboxed
    /// case and must not be claimed covered by this staged primitive.
    pub(crate) fn terminate_and_reap(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        let mut killed = false;
        loop {
            let foreground = self.foreground_exit()?;
            let group = self.pid()?;
            let members = group_members(group)?;
            if !members.iter().any(|m| m.pid == group) {
                // The unreaped Darwin leader must remain visible too. An empty
                // or permission-filtered census must not be vacuous success.
                return Err(io::Error::other(
                    "pinned owned leader absent from group census",
                ));
            }
            let all_exited = members.iter().all(|m| m.status == 5); // Darwin SZOMB
            let all_frozen = members.iter().all(|m| matches!(m.status, 4 | 5)); // SSTOP/SZOMB
            if (all_exited || all_frozen) && members == group_members(self.pid()?)? {
                if all_exited && foreground.is_some() {
                    let status = match self.custody.as_mut().unwrap().child.wait() {
                        Ok(status) => status,
                        Err(error) => {
                            self.lost_wait_authority = true;
                            return Err(error);
                        }
                    };
                    // Only this path releases the actual guard; all syscalls,
                    // census failures and timeouts leave custody in self.
                    drop(self.custody.take());
                    return Ok(status);
                }
                if !killed {
                    self.signal(Signal::SIGKILL)?;
                    killed = true;
                }
            } else if !killed {
                self.signal(Signal::SIGSTOP)?;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned group exit not proved",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
fn group_members(group: i32) -> io::Result<Vec<Member>> {
    let mut rows = vec![NativeMember::default(); MAX_MEMBERS];
    let mut count = 0;
    // SAFETY: repr(C) matches the small private SDK-compiled shim record (not
    // kinfo_proc). The initialized array holds exactly capacity writable rows;
    // the shim bounds every write and returns a positive errno on failure.
    let error =
        unsafe { rpi_host_process_census(group, rows.as_mut_ptr(), rows.len(), &mut count) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    if count > rows.len() {
        return Err(io::Error::other("owned group census overflow"));
    }
    rows.truncate(count);
    let mut members = Vec::with_capacity(count);
    for row in rows {
        if row.pid <= 0 || row.start_usec >= 1_000_000 {
            return Err(io::Error::other("invalid process group census identity"));
        }
        members.push(Member {
            pid: row.pid,
            status: row.status,
            start: (row.start_sec, row.start_usec),
        });
    }
    members.sort_by_key(|m| m.pid);
    if members.windows(2).any(|pair| pair[0].pid == pair[1].pid) {
        return Err(io::Error::other("duplicate process group census identity"));
    }
    Ok(members)
}

#[cfg(test)]
mod tests;
