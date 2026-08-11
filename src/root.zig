const std = @import("std");

const linux = std.os.linux;
const block_size = 4096;
const descriptor_checksum_offset = block_size - @sizeOf(u64);

const CheckpointSlot = enum(u8) {
    green,
    blue,
};

const Layout = struct {
    physical_map_blocks: u32,
    checksum_map_blocks: u32,
    green_physical_map_start: u32,
    green_checksum_map_start: u32,
    blue_physical_map_start: u32,
    blue_checksum_map_start: u32,
    log_start: u32,
};

fn close(fd: linux.fd_t) void {
    std.debug.assert(linux.errno(linux.close(fd)) == .SUCCESS);
}

fn completeExactly(ring: *linux.IoUring, user_data: u64, result: i32) !void {
    const completion = try ring.copy_cqe();
    if (completion.user_data != user_data or completion.flags != 0) return error.UnexpectedCompletion;
    if (completion.err() != .SUCCESS) return error.InputOutput;
    if (completion.res != result) return error.UnexpectedIoLength;
}

fn layoutFor(volume_blocks: u32) Layout {
    const physical_map_blocks: u32 = @intCast((@as(u64, volume_blocks) * @sizeOf(u32) + block_size - 1) / block_size);
    const checksum_map_blocks: u32 = @intCast((@as(u64, volume_blocks) * @sizeOf(u64) + block_size - 1) / block_size);
    const green_physical_map_start = 2;
    const green_checksum_map_start = green_physical_map_start + physical_map_blocks;
    const blue_physical_map_start = green_checksum_map_start + checksum_map_blocks;
    const blue_checksum_map_start = blue_physical_map_start + physical_map_blocks;

    return .{
        .physical_map_blocks = physical_map_blocks,
        .checksum_map_blocks = checksum_map_blocks,
        .green_physical_map_start = green_physical_map_start,
        .green_checksum_map_start = green_checksum_map_start,
        .blue_physical_map_start = blue_physical_map_start,
        .blue_checksum_map_start = blue_checksum_map_start,
        .log_start = blue_checksum_map_start + checksum_map_blocks,
    };
}

fn randomVolumeId() !u64 {
    var encoded: [@sizeOf(u64)]u8 = undefined;
    var offset: usize = 0;
    while (offset < encoded.len) {
        const result = linux.getrandom(encoded[offset..].ptr, encoded.len - offset, 0);
        switch (linux.errno(result)) {
            .SUCCESS => {},
            .INTR => continue,
            else => return error.RandomUnavailable,
        }
        if (result == 0) return error.RandomUnavailable;
        offset += result;
    }
    return std.mem.readInt(u64, &encoded, .little);
}

fn encodeCheckpointDescriptor(
    descriptor: *[block_size]u8,
    slot: CheckpointSlot,
    generation: u64,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    layout: Layout,
) void {
    @memset(descriptor, 0);
    @memcpy(descriptor[0..4], "VBLC");
    descriptor[4] = 1;
    descriptor[5] = @intFromEnum(slot);
    std.mem.writeInt(u16, descriptor[6..8], 1, .little);
    std.mem.writeInt(u64, descriptor[8..16], volume_id, .little);
    std.mem.writeInt(u32, descriptor[16..20], block_size, .little);
    std.mem.writeInt(u32, descriptor[20..24], volume_blocks, .little);
    std.mem.writeInt(u64, descriptor[24..32], backing_blocks, .little);
    std.mem.writeInt(u64, descriptor[32..40], generation, .little);
    std.mem.writeInt(u32, descriptor[48..52], layout.log_start - 1, .little);

    const physical_map_start = if (slot == .green) layout.green_physical_map_start else layout.blue_physical_map_start;
    const checksum_map_start = if (slot == .green) layout.green_checksum_map_start else layout.blue_checksum_map_start;
    std.mem.writeInt(u32, descriptor[52..56], physical_map_start, .little);
    std.mem.writeInt(u32, descriptor[56..60], layout.physical_map_blocks, .little);
    std.mem.writeInt(u32, descriptor[60..64], checksum_map_start, .little);
    std.mem.writeInt(u32, descriptor[64..68], layout.checksum_map_blocks, .little);
    descriptor[68] = 1;
    std.mem.writeInt(u64, descriptor[descriptor_checksum_offset..], std.hash.XxHash3.hash(0, descriptor), .little);
}

pub fn format(dir_fd: linux.fd_t, backing_path: []const u8, volume_bytes: u64) !void {
    if (volume_bytes == 0 or volume_bytes % block_size != 0) return error.InvalidVolumeSize;

    const volume_blocks_u64 = volume_bytes / block_size;
    if (volume_blocks_u64 >= @as(u64, 1) << 31) return error.InvalidVolumeSize;
    const volume_blocks: u32 = @intCast(volume_blocks_u64);
    const layout = layoutFor(volume_blocks);

    const fd = try std.posix.openat(dir_fd, backing_path, .{
        .ACCMODE = .RDWR,
        .DIRECT = true,
        .CLOEXEC = true,
    }, 0);
    defer close(fd);

    const backing_size_result = linux.lseek(fd, 0, linux.SEEK.END);
    if (linux.errno(backing_size_result) != .SUCCESS) return error.BackingSizeUnavailable;
    const backing_bytes: u64 = backing_size_result;
    if (backing_bytes % block_size != 0) return error.InvalidBackingSize;
    const backing_blocks = backing_bytes / block_size;
    if (backing_blocks < @as(u64, layout.log_start) + 2) return error.BackingTooSmall;

    const volume_id = try randomVolumeId();
    var descriptors: [2 * block_size]u8 align(block_size) = undefined;
    encodeCheckpointDescriptor(descriptors[0..block_size], .green, 1, volume_id, volume_blocks, backing_blocks, layout);
    encodeCheckpointDescriptor(descriptors[block_size..], .blue, 2, volume_id, volume_blocks, backing_blocks, layout);

    var ring = try linux.IoUring.init(2, 0);
    defer ring.deinit();

    _ = try ring.write(1, fd, &descriptors, 0);
    if (try ring.submit() != 1) return error.UnexpectedSubmissionCount;
    try completeExactly(&ring, 1, descriptors.len);

    _ = try ring.fsync(2, fd, 0);
    if (try ring.submit() != 1) return error.UnexpectedSubmissionCount;
    try completeExactly(&ring, 2, 0);
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
        try completeExactly(&ring, 1, block_size);

        _ = try ring.fsync(2, fd, 0);
        try std.testing.expectEqual(@as(u32, 1), try ring.submit());
        try completeExactly(&ring, 2, 0);
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
        try completeExactly(&ring, 3, block_size);
    }

    try std.testing.expectEqualSlices(u8, &written, &read);
}

test "format writes valid empty checkpoint roots" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, 8 * block_size);
    }

    try format(temporary_directory.dir.handle, "backing", block_size);

    const backing = try temporary_directory.dir.openFile(std.testing.io, "backing", .{});
    defer backing.close(std.testing.io);
    var descriptors: [2 * block_size]u8 = undefined;
    try std.testing.expectEqual(descriptors.len, try backing.readPositionalAll(std.testing.io, &descriptors, 0));

    const volume_id = std.mem.readInt(u64, descriptors[8..16], .little);
    for (0..2) |index| {
        var descriptor = descriptors[index * block_size ..][0..block_size];
        try std.testing.expectEqualSlices(u8, "VBLC", descriptor[0..4]);
        try std.testing.expectEqual(@as(u8, 1), descriptor[4]);
        try std.testing.expectEqual(@as(u8, @intCast(index)), descriptor[5]);
        try std.testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, descriptor[6..8], .little));
        try std.testing.expectEqual(volume_id, std.mem.readInt(u64, descriptor[8..16], .little));
        try std.testing.expectEqual(@as(u32, block_size), std.mem.readInt(u32, descriptor[16..20], .little));
        try std.testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, descriptor[20..24], .little));
        try std.testing.expectEqual(@as(u64, 8), std.mem.readInt(u64, descriptor[24..32], .little));
        try std.testing.expectEqual(@as(u64, @intCast(index + 1)), std.mem.readInt(u64, descriptor[32..40], .little));
        try std.testing.expectEqual(@as(u32, 5), std.mem.readInt(u32, descriptor[48..52], .little));
        try std.testing.expectEqual(@as(u32, @intCast(2 + index * 2)), std.mem.readInt(u32, descriptor[52..56], .little));
        try std.testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, descriptor[56..60], .little));
        try std.testing.expectEqual(@as(u32, @intCast(3 + index * 2)), std.mem.readInt(u32, descriptor[60..64], .little));
        try std.testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, descriptor[64..68], .little));
        try std.testing.expectEqual(@as(u8, 1), descriptor[68]);

        const checksum = std.mem.readInt(u64, descriptor[descriptor_checksum_offset..], .little);
        @memset(descriptor[descriptor_checksum_offset..], 0);
        try std.testing.expectEqual(checksum, std.hash.XxHash3.hash(0, descriptor));
    }
}
