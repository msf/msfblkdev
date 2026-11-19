#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <liburing.h>
#include <linux/magic.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

enum { BLOCK = 4096 };

static bool observed(const char *op, ssize_t result, ssize_t expected, bool want_eio)
{
    int error = result < 0 ? errno : 0;
    printf("op=%s result=%zd errno=%d expected=%zd expected_errno=%d\n",
           op, result, error, want_eio ? (ssize_t)-1 : expected, want_eio ? EIO : 0);
    return want_eio ? result == -1 && error == EIO : result == expected;
}

static bool exercise(int fd, bool fail_write, bool fail_sync)
{
    _Alignas(BLOCK) unsigned char data[BLOCK], readback[BLOCK];
    memset(data, 0xa5, sizeof(data));
    if (!observed("pwrite", pwrite(fd, data, BLOCK, 0), BLOCK, fail_write))
        return false;
    if (fail_write && !observed("pwrite.retry", pwrite(fd, data, BLOCK, 0), BLOCK, false))
        return false;
    if (!observed("fsync", fsync(fd), 0, fail_sync))
        return false;
    if (fail_sync) {
        // Dirty the same block again so QEMU can't elide the second flush as clean.
        memset(data, 0x5a, sizeof(data));
        if (!observed("pwrite.after-fsync", pwrite(fd, data, BLOCK, 0), BLOCK, false) ||
            !observed("fsync.retry", fsync(fd), 0, false))
            return false;
    }
    if (!observed("pread", pread(fd, readback, BLOCK, 0), BLOCK, false))
        return false;
    bool match = memcmp(data, readback, BLOCK) == 0;
    printf("readback_match=%d offset=0 bytes=%d byte=0x%02x\n", match, BLOCK, data[0]);
    return match;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "preflight") == 0) {
        struct io_uring ring;
        int rc = io_uring_queue_init(2, &ring, 0);
        if (rc < 0) {
            fprintf(stderr, "io_uring_queue_init: %s\n", strerror(-rc));
            return 1;
        }
        io_uring_queue_exit(&ring);
        return 0;
    }
    if (argc != 3 || (strcmp(argv[1], "normal") && strcmp(argv[1], "write") &&
                      strcmp(argv[1], "fsync"))) {
        fprintf(stderr, "usage: probe-io preflight | {normal|write|fsync} FUSE-EXPORT\n");
        return 2;
    }
    int fd = open(argv[2], O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_DIRECT);
    if (fd < 0) {
        perror("open O_DIRECT");
        return 1;
    }
    struct statfs fs;
    struct stat st;
    if (fstatfs(fd, &fs) || fs.f_type != FUSE_SUPER_MAGIC || fstat(fd, &st) ||
        !S_ISREG(st.st_mode) || st.st_size != BLOCK || st.st_uid != getuid()) {
        fprintf(stderr, "expected an owned %d-byte regular FUSE export\n", BLOCK);
        return 1;
    }
    bool ok = exercise(fd, !strcmp(argv[1], "write"), !strcmp(argv[1], "fsync"));
    return observed("close", close(fd), 0, false) && ok ? 0 : 1;
}
