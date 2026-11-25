/* Private Darwin host implementation detail, not an extension/SDK ABI.
 * kinfo_proc is defined by the target SDK; do not guess its layout in Rust.
 * Unlike proc_pidinfo, KERN_PROC_PGRP also supplies positive zombie states. */
#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/proc.h>
#include <sys/time.h>
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

static int process_info(int32_t pid, struct kinfo_proc *row) {
    size_t bytes = sizeof(*row);
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid};
    if (sysctl(mib, 4, row, &bytes, NULL, 0) != 0)
        return errno == ESRCH ? EAGAIN : (errno ? errno : EIO);
    if (bytes != sizeof(*row) || row->kp_proc.p_pid != pid) return EAGAIN;
    return 0;
}

static int same_process(int32_t pid, uint64_t sec, uint64_t usec) {
    struct kinfo_proc current;
    int error = process_info(pid, &current);
    if (error) return error;
    if ((uint64_t)current.kp_proc.p_starttime.tv_sec != sec ||
        (uint64_t)current.kp_proc.p_starttime.tv_usec != usec)
        return EAGAIN;
    return 0;
}

static struct rpi_host_process_member member(const struct extern_proc *proc) {
    return (struct rpi_host_process_member){
        proc->p_pid, (unsigned char)proc->p_stat,
        proc->p_starttime.tv_sec, proc->p_starttime.tv_usec,
    };
}

/* Return an errno value, never an ambiguous empty success on syscall failure. */
int rpi_host_process_census(int32_t group, struct rpi_host_process_member *out,
                            size_t capacity, size_t *written) {
    if (!written || !out || group <= 0 || capacity == 0 || capacity > 4096)
        return EINVAL;
    *written = 0;
    struct kinfo_proc *rows = calloc(capacity, sizeof(*rows));
    if (!rows) return ENOMEM;
    size_t bytes = capacity * sizeof(*rows);
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PGRP, group};
    int error = 0;
    if (sysctl(mib, 4, rows, &bytes, NULL, 0) != 0) {
        error = errno;
        if (!error) error = EIO;
    } else if (bytes >= capacity * sizeof(*rows) || bytes % sizeof(*rows)) {
        error = EOVERFLOW;
    } else {
        size_t count = bytes / sizeof(*rows);
        for (size_t i = 0; i < count; ++i) {
            if (rows[i].kp_proc.p_pid <= 0 || rows[i].kp_eproc.e_pgid != group) {
                error = EAGAIN;
                break;
            }
            out[i] = member(&rows[i].kp_proc);
        }
        if (!error) *written = count;
    }
    free(rows);
    return error;
}

int rpi_host_session_census(int32_t session, struct rpi_host_process_member *out,
                            size_t capacity, size_t *written) {
    if (!written || !out || session <= 0 || capacity == 0 || capacity > 4096)
        return EINVAL;
    *written = 0;
    struct timeval before;
    if (gettimeofday(&before, NULL) != 0) return errno;
    /* The kernel copies this raw list under the process-list lock. Unlike a
     * KERN_PROC_ALL walk it cannot silently omit an entry that vanishes while
     * we resolve individual identities, or a child still being forked. */
    pid_t pids[4096];
    int bytes = proc_listpids(PROC_ALL_PIDS, 0, pids, sizeof(pids));
    if (bytes <= 0) return errno ? errno : EIO;
    if ((size_t)bytes >= sizeof(pids) || bytes % sizeof(pids[0])) return EOVERFLOW;
    size_t used = 0;
    for (size_t i = 0; i < (size_t)bytes / sizeof(pids[0]); ++i) {
        pid_t pid = pids[i];
        if (pid == 0) continue;
        struct kinfo_proc row;
        int error = process_info(pid, &row);
        if (error) return error;
        const struct timeval start = row.kp_proc.p_starttime;
        if (timercmp(&start, &before, >=)) return EAGAIN;
        pid_t sid = getsid(pid);
        if (sid < 0 && row.kp_proc.p_stat != SZOMB)
            return errno == ESRCH ? EAGAIN : errno;
        if (row.kp_proc.p_stat == SZOMB) continue;
        error = same_process(pid, start.tv_sec, start.tv_usec);
        if (error) return error;
        if (sid != session) continue;
        if (used == capacity) return EOVERFLOW;
        out[used++] = member(&row.kp_proc);
    }
    *written = used;
    return 0;
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
