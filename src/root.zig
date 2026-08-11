const std = @import("std");

const linux = std.os.linux;
const block_size = 4096;

fn close(fd: linux.fd_t) void {
    std.debug.assert(linux.errno(linux.close(fd)) == .SUCCESS);
}

fn expectCompletion(ring: *linux.IoUring, user_data: u64, result: i32) !void {
    const completion = try ring.copy_cqe();
    try std.testing.expectEqual(user_data, completion.user_data);
    try std.testing.expectEqual(linux.E.SUCCESS, completion.err());
    try std.testing.expectEqual(result, completion.res);
    try std.testing.expectEqual(@as(u32, 0), completion.flags);
}

test "direct io_uring write survives fsync and reopen" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    var ring = try linux.IoUring.init(4, 0);
    defer ring.deinit();

    var written: [block_size]u8 align(block_size) = undefined;
    @memset(&written, 0xa5);

    {
        const fd = try std.posix.openat(temporary_directory.dir.handle, "backing", .{
            .ACCMODE = .RDWR,
            .CREAT = true,
            .EXCL = true,
            .DIRECT = true,
            .CLOEXEC = true,
        }, 0o600);
        defer close(fd);

        _ = try ring.write(1, fd, &written, 0);
        try std.testing.expectEqual(@as(u32, 1), try ring.submit());
        try expectCompletion(&ring, 1, block_size);

        _ = try ring.fsync(2, fd, 0);
        try std.testing.expectEqual(@as(u32, 1), try ring.submit());
        try expectCompletion(&ring, 2, 0);
    }

    var read: [block_size]u8 align(block_size) = undefined;
    {
        const fd = try std.posix.openat(temporary_directory.dir.handle, "backing", .{
            .DIRECT = true,
            .CLOEXEC = true,
        }, 0);
        defer close(fd);

        _ = try ring.read(3, fd, .{ .buffer = &read }, 0);
        try std.testing.expectEqual(@as(u32, 1), try ring.submit());
        try expectCompletion(&ring, 3, block_size);
    }

    try std.testing.expectEqualSlices(u8, &written, &read);
}
