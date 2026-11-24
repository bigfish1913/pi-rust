/* Private Darwin host implementation detail, not an extension/SDK ABI.
 * kinfo_proc is defined by the target SDK; do not guess its layout in Rust.
 * Unlike proc_pidinfo, KERN_PROC_PGRP also supplies positive zombie states. */
#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/proc.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>

struct rpi_host_process_member {
    int32_t pid;
    uint32_t status;
    uint64_t start_sec;
    uint64_t start_usec;
};
_Static_assert(sizeof(struct rpi_host_process_member) == 24, "host census ABI");

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
            out[i].pid = rows[i].kp_proc.p_pid;
            out[i].status = (unsigned char)rows[i].kp_proc.p_stat;
            out[i].start_sec = rows[i].kp_proc.p_starttime.tv_sec;
            out[i].start_usec = rows[i].kp_proc.p_starttime.tv_usec;
        }
        if (!error) *written = count;
    }
    free(rows);
    return error;
}
