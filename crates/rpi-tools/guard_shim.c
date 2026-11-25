
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <linux/limits.h>

static int resolve_path(const char *path, char *out, size_t cap) {
    char buf[PATH_MAX];
    if (realpath(path, buf)) {
        snprintf(out, cap, "%s", buf);
        return 1;
    }
    /* Target does not exist yet (e.g. a to-be-created .lock): resolve the
       literal path against the process cwd so relative aliases still match. */
    if (path[0] == '/') {
        snprintf(out, cap, "%s", path);
    } else if (getcwd(buf, sizeof(buf))) {
        snprintf(out, cap, "%s/%s", buf, path);
    } else {
        snprintf(out, cap, "%s", path);
    }
    return 1;
}

static int deny(const char *path) {
    if (!path || !*path) return 0;
    const char *env = getenv("RPI_ACTIVE_SESSION_FILES");
    if (!env || !*env) return 0;
    char resolved[PATH_MAX];
    if (!resolve_path(path, resolved, sizeof(resolved))) return 0;
    size_t want = strlen(resolved);
    const char *p = env;
    while (*p) {
        size_t n = strcspn(p, ":");
        if (n == want && !memcmp(p, resolved, n)) {
            errno = EPERM;
            return 1;
        }
        p += n;
        if (*p == ':') p++;
    }
    return 0;
}
static int write_flags(int flags) {
    return (flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND)) != 0
        || (flags & O_ACCMODE) != O_RDONLY;
}
static int write_mode(const char *mode) {
    return mode && (strchr(mode, 'w') || strchr(mode, 'a') || strchr(mode, '+'));
}

int open(const char *path, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny(path)) return -1;
    if (flags & O_CREAT) return real(path, flags, m);
    return real(path, flags);
}
int open64(const char *path, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open64");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny(path)) return -1;
    if (flags & O_CREAT) return real(path, flags, m);
    return real(path, flags);
}
static int deny_at(int dirfd, const char *path) {
    if (!path || !*path || path[0] == '/' || dirfd == AT_FDCWD) return deny(path);
    /* A relative name paired with a non-cwd dirfd resolves against that fd.
       Recover its path from /proc so an obfuscated `os.open(..., dir_fd=d)`
       cannot slip past the cwd-based comparison. */
    char fdpath[64], base[PATH_MAX], joined[PATH_MAX * 2];
    snprintf(fdpath, sizeof(fdpath), "/proc/self/fd/%d", dirfd);
    ssize_t n = readlink(fdpath, base, sizeof(base) - 1);
    if (n <= 0) return deny(path);
    base[n] = 0;
    snprintf(joined, sizeof(joined), "%s/%s", base, path);
    return deny(joined);
}

int openat(int dirfd, const char *path, int flags, ...) {
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny_at(dirfd, path)) return -1;
    if (flags & O_CREAT) return real(dirfd, path, flags, m);
    return real(dirfd, path, flags);
}
int openat64(int dirfd, const char *path, int flags, ...) {
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat64");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny_at(dirfd, path)) return -1;
    if (flags & O_CREAT) return real(dirfd, path, flags, m);
    return real(dirfd, path, flags);
}
int creat(const char *path, mode_t mode) {
    static int (*real)(const char *, mode_t);
    if (!real) real = dlsym(RTLD_NEXT, "creat");
    if (deny(path)) return -1;
    return real(path, mode);
}
FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen");
    if (write_mode(mode) && deny(path)) return NULL;
    return real(path, mode);
}
FILE *fopen64(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen64");
    if (write_mode(mode) && deny(path)) return NULL;
    return real(path, mode);
}
int truncate(const char *path, off_t length) {
    static int (*real)(const char *, off_t);
    if (!real) real = dlsym(RTLD_NEXT, "truncate");
    if (deny(path)) return -1;
    return real(path, length);
}
int unlink(const char *path) {
    static int (*real)(const char *);
    if (!real) real = dlsym(RTLD_NEXT, "unlink");
    if (deny(path)) return -1;
    return real(path);
}
int rename(const char *oldpath, const char *newpath) {
    static int (*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "rename");
    if (deny(oldpath) || deny(newpath)) return -1;
    return real(oldpath, newpath);
}
