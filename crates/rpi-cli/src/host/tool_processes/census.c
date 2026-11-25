/* Private Darwin host implementation detail, not an extension/SDK ABI.
 * kinfo_proc is defined by the target SDK; do not guess its layout in Rust.
 * Unlike proc_pidinfo, KERN_PROC_PGRP also supplies positive zombie states. */
#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/proc.h>
#include <errno.h>
#include <libproc.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>

struct rpi_host_process_member {
    int32_t pid;
    uint32_t status;
    uint64_t start_sec;
    uint64_t start_usec;
};
_Static_assert(sizeof(struct rpi_host_process_member) == 24, "host census ABI");

static int same_process(int32_t pid, uint64_t sec, uint64_t usec) {
    struct kinfo_proc current;
    size_t bytes = sizeof(current);
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid};
    if (sysctl(mib, 4, &current, &bytes, NULL, 0) != 0)
        return errno ? errno : EIO;
    if (bytes != sizeof(current) || current.kp_proc.p_pid != pid ||
        (uint64_t)current.kp_proc.p_starttime.tv_sec != sec ||
        (uint64_t)current.kp_proc.p_starttime.tv_usec != usec)
        return EAGAIN;
    return 0;
}

/* Return an errno value, never an ambiguous empty success on syscall failure. */
static int census(int selector, int32_t id, struct rpi_host_process_member *out,
                  size_t capacity, size_t *written) {
    if (!written || !out || id <= 0 || capacity == 0 || capacity > 4096)
        return EINVAL;
    *written = 0;
    struct kinfo_proc *rows = calloc(capacity, sizeof(*rows));
    if (!rows) return ENOMEM;
    size_t bytes = capacity * sizeof(*rows);
    int mib[] = {CTL_KERN, KERN_PROC, selector, selector == KERN_PROC_ALL ? 0 : id};
    int error = 0;
    if (sysctl(mib, 4, rows, &bytes, NULL, 0) != 0) {
        error = errno;
        if (!error) error = EIO;
    } else if (bytes >= capacity * sizeof(*rows) || bytes % sizeof(*rows)) {
        error = EOVERFLOW;
    } else {
        size_t count = bytes / sizeof(*rows);
        size_t used = 0;
        for (size_t i = 0; i < count; ++i) {
            const struct extern_proc *proc = &rows[i].kp_proc;
            if (selector == KERN_PROC_ALL) {
                if (proc->p_pid == 0 || proc->p_stat == SZOMB) continue;
                pid_t sid = getsid(proc->p_pid);
                if (sid < 0) {
                    error = errno == ESRCH ? EAGAIN : errno;
                    break;
                }
                /* A vanished or reused live PID could have left a new child
                 * outside the original snapshot. Retry the whole census. */
                error = same_process(proc->p_pid, proc->p_starttime.tv_sec,
                                     proc->p_starttime.tv_usec);
                if (error) break;
                if (sid != id) continue;
            } else if (proc->p_pid <= 0 || rows[i].kp_eproc.e_pgid != id) {
                error = EAGAIN;
                break;
            }
            out[used].pid = proc->p_pid;
            out[used].status = (unsigned char)proc->p_stat;
            out[used].start_sec = proc->p_starttime.tv_sec;
            out[used].start_usec = proc->p_starttime.tv_usec;
            ++used;
        }
        if (!error) *written = used;
    }
    free(rows);
    return error;
}

int rpi_host_process_census(int32_t group, struct rpi_host_process_member *out,
                            size_t capacity, size_t *written) {
    return census(KERN_PROC_PGRP, group, out, capacity, written);
}

int rpi_host_session_census(int32_t session, struct rpi_host_process_member *out,
                            size_t capacity, size_t *written) {
    return census(KERN_PROC_ALL, session, out, capacity, written);
}

/* XNU proc_info_private.h defines flavor 17 (PROC_PIDUNIQIDENTIFIERINFO)
 * and its fixed 56-byte result. Fetch pidversion before checking the snapshot
 * identity again, then let the kernel reject a reused PID at signal time. */
struct rpi_darwin_proc_identity {
    uint8_t uuid[16];
    uint64_t unique_id;
    uint64_t parent_id;
    int32_t pid_version;
    int32_t original_parent_version;
    uint64_t reserved[2];
};
_Static_assert(sizeof(struct rpi_darwin_proc_identity) == 56, "Darwin proc identity ABI");

int rpi_host_signal_session_member(int32_t session, int32_t pid, uint64_t sec,
                                   uint64_t usec, int signum) {
    struct rpi_darwin_proc_identity identity;
    int bytes = proc_pidinfo(pid, 17, 0, &identity, sizeof(identity));
    if (bytes != sizeof(identity)) return bytes == 0 && errno ? errno : EIO;
    pid_t sid = getsid(pid);
    if (sid < 0) return errno;
    if (sid != session) return ESRCH;
    int error = same_process(pid, sec, usec);
    if (error) return error;
    audit_token_t token = {0};
    token.val[5] = pid;
    token.val[7] = identity.pid_version;
    return proc_signal_with_audittoken(&token, signum);
}
