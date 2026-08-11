const std = @import("std");

const linux = std.os.linux;
const block_size = 4096;
const checkpoint_body_checksum_offset = 72;
const descriptor_checksum_offset = block_size - @sizeOf(u64);
const footer_checksum_offset = block_size - @sizeOf(u64);
const footer_lba_offset = 32;
const footer_payload_checksum_offset = 1384;
const empty_mapping_flag = 1;
const checksum_algorithm_xxh3_64 = 1;
const write_record_kind = 1;

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

const Checkpoint = struct {
    slot: CheckpointSlot,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    generation: u64,
    checkpoint_lsn: u64,
    last_footer_block: u32,
    layout: Layout,
};

pub const Volume = struct {
    allocator: std.mem.Allocator,
    mapping_storage: []align(block_size) u8,
    backing_fd: linux.fd_t,
    io_uring: linux.IoUring,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    log_start_block: u32,
    physical_blocks: []u32,
    checksums: []u64,
    last_lsn: u64,
    durable_lsn: u64,
    last_footer_block: u32,
    checkpoint_generation: u64,
    next_checkpoint_slot: CheckpointSlot,
    log_bytes_since_checkpoint: u64,

    pub fn write_block(self: *Volume, lba: u32, data: *const [block_size]u8) !void {
        if (lba >= self.volume_blocks) return error.InvalidLogicalBlock;
        if (self.last_lsn == std.math.maxInt(u64)) return error.SequenceExhausted;

        const payload_block_u64 = @as(u64, self.last_footer_block) + 1;
        const footer_block_u64 = payload_block_u64 + 1;
        if (footer_block_u64 >= self.backing_blocks) return error.LogFull;
        const payload_block: u32 = @intCast(payload_block_u64);
        const footer_block: u32 = @intCast(footer_block_u64);
        const lsn = self.last_lsn + 1;
        const checksum = payloadChecksum(self.volume_id, lba, payload_block, data);

        var record: [2 * block_size]u8 align(block_size) = undefined;
        @memcpy(record[0..block_size], data);
        encodeWriteFooter(record[block_size..], self.volume_id, lsn, self.last_footer_block, footer_block, lba, checksum);
        const iovecs = [_]std.posix.iovec_const{
            .{ .base = record[0..block_size].ptr, .len = block_size },
            .{ .base = record[block_size..].ptr, .len = block_size },
        };

        _ = try self.io_uring.writev(lsn, self.backing_fd, &iovecs, payload_block_u64 * block_size);
        if (try self.io_uring.submit() != 1) return error.UnexpectedSubmissionCount;
        try completeExactly(&self.io_uring, lsn, record.len);

        self.physical_blocks[lba] = payload_block;
        self.checksums[lba] = checksum;
        self.last_lsn = lsn;
        self.last_footer_block = footer_block;
        self.log_bytes_since_checkpoint += record.len;
    }

    pub fn flush(self: *Volume) !void {
        _ = try self.io_uring.fsync(self.last_lsn, self.backing_fd, 0);
        if (try self.io_uring.submit() != 1) return error.UnexpectedSubmissionCount;
        try completeExactly(&self.io_uring, self.last_lsn, 0);
        self.durable_lsn = self.last_lsn;
    }

    fn persistCheckpointBody(self: *Volume) !u64 {
        const layout = layoutFor(self.volume_blocks);
        const body_start = switch (self.next_checkpoint_slot) {
            .green => layout.green_physical_map_start,
            .blue => layout.blue_physical_map_start,
        };
        const max_io_bytes = @as(usize, std.math.maxInt(i32)) / block_size * block_size;

        var offset: usize = 0;
        while (offset < self.mapping_storage.len) {
            const length = @min(self.mapping_storage.len - offset, max_io_bytes);
            _ = try self.io_uring.write(
                self.last_lsn,
                self.backing_fd,
                self.mapping_storage[offset..][0..length],
                @as(u64, body_start) * block_size + offset,
            );
            if (try self.io_uring.submit() != 1) return error.UnexpectedSubmissionCount;
            try completeExactly(&self.io_uring, self.last_lsn, @intCast(length));
            offset += length;
        }

        _ = try self.io_uring.fsync(self.last_lsn, self.backing_fd, 0);
        if (try self.io_uring.submit() != 1) return error.UnexpectedSubmissionCount;
        try completeExactly(&self.io_uring, self.last_lsn, 0);
        return std.hash.XxHash3.hash(0, self.mapping_storage);
    }

    fn checkpoint(self: *Volume) !void {
        if (self.checkpoint_generation == std.math.maxInt(u64)) return error.GenerationExhausted;

        try self.flush();
        const body_checksum = try self.persistCheckpointBody();
        const generation = self.checkpoint_generation + 1;
        const slot = self.next_checkpoint_slot;
        var descriptor: [block_size]u8 align(block_size) = undefined;
        encodeCheckpointDescriptor(
            &descriptor,
            slot,
            0,
            generation,
            self.last_lsn,
            self.last_footer_block,
            self.volume_id,
            self.volume_blocks,
            self.backing_blocks,
            layoutFor(self.volume_blocks),
            body_checksum,
        );

        _ = try self.io_uring.write(
            self.last_lsn,
            self.backing_fd,
            &descriptor,
            @as(u64, @intFromEnum(slot)) * block_size,
        );
        if (try self.io_uring.submit() != 1) return error.UnexpectedSubmissionCount;
        try completeExactly(&self.io_uring, self.last_lsn, descriptor.len);
        try self.flush();

        self.checkpoint_generation = generation;
        self.next_checkpoint_slot = if (slot == .green) .blue else .green;
        self.log_bytes_since_checkpoint = 0;
    }

    pub fn deinit(self: *Volume) void {
        self.io_uring.deinit();
        close(self.backing_fd);
        self.allocator.free(self.mapping_storage);
        self.* = undefined;
    }
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

fn backingBlocksFor(layout: Layout, backing_bytes: u64) !u64 {
    if (backing_bytes % block_size != 0) return error.InvalidBackingSize;

    const backing_blocks = backing_bytes / block_size;
    if (backing_blocks > @as(u64, std.math.maxInt(u32)) + 1) return error.BackingTooLarge;
    if (backing_blocks < @as(u64, layout.log_start) + 2) return error.BackingTooSmall;
    return backing_blocks;
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

fn payloadChecksum(volume_id: u64, lba: u32, physical_block: u32, payload: *const [block_size]u8) u64 {
    var addresses: [2 * @sizeOf(u32)]u8 = undefined;
    std.mem.writeInt(u32, addresses[0..4], lba, .little);
    std.mem.writeInt(u32, addresses[4..8], physical_block, .little);

    var hasher = std.hash.XxHash3.init(volume_id);
    hasher.update(&addresses);
    hasher.update(payload);
    return hasher.final();
}

fn encodeWriteFooter(
    footer: *[block_size]u8,
    volume_id: u64,
    lsn: u64,
    previous_footer_block: u32,
    footer_block: u32,
    lba: u32,
    payload_checksum: u64,
) void {
    @memset(footer, 0);
    @memcpy(footer[0..4], "VBLF");
    footer[4] = 1;
    footer[5] = write_record_kind;
    std.mem.writeInt(u64, footer[8..16], volume_id, .little);
    std.mem.writeInt(u64, footer[16..24], lsn, .little);
    std.mem.writeInt(u32, footer[24..28], previous_footer_block, .little);
    std.mem.writeInt(u32, footer[28..32], footer_block, .little);
    std.mem.writeInt(u32, footer[footer_lba_offset..][0..4], lba, .little);
    std.mem.writeInt(u64, footer[footer_payload_checksum_offset..][0..8], payload_checksum, .little);
    std.mem.writeInt(u64, footer[footer_checksum_offset..], std.hash.XxHash3.hash(0, footer), .little);
}

fn encodeCheckpointDescriptor(
    descriptor: *[block_size]u8,
    slot: CheckpointSlot,
    flags: u16,
    generation: u64,
    checkpoint_lsn: u64,
    last_footer_block: u32,
    volume_id: u64,
    volume_blocks: u32,
    backing_blocks: u64,
    layout: Layout,
    body_checksum: u64,
) void {
    @memset(descriptor, 0);
    @memcpy(descriptor[0..4], "VBLC");
    descriptor[4] = 1;
    descriptor[5] = @intFromEnum(slot);
    std.mem.writeInt(u16, descriptor[6..8], flags, .little);
    std.mem.writeInt(u64, descriptor[8..16], volume_id, .little);
    std.mem.writeInt(u32, descriptor[16..20], block_size, .little);
    std.mem.writeInt(u32, descriptor[20..24], volume_blocks, .little);
    std.mem.writeInt(u64, descriptor[24..32], backing_blocks, .little);
    std.mem.writeInt(u64, descriptor[32..40], generation, .little);
    std.mem.writeInt(u64, descriptor[40..48], checkpoint_lsn, .little);
    std.mem.writeInt(u32, descriptor[48..52], last_footer_block, .little);

    const physical_map_start = if (slot == .green) layout.green_physical_map_start else layout.blue_physical_map_start;
    const checksum_map_start = if (slot == .green) layout.green_checksum_map_start else layout.blue_checksum_map_start;
    std.mem.writeInt(u32, descriptor[52..56], physical_map_start, .little);
    std.mem.writeInt(u32, descriptor[56..60], layout.physical_map_blocks, .little);
    std.mem.writeInt(u32, descriptor[60..64], checksum_map_start, .little);
    std.mem.writeInt(u32, descriptor[64..68], layout.checksum_map_blocks, .little);
    descriptor[68] = checksum_algorithm_xxh3_64;
    std.mem.writeInt(u64, descriptor[checkpoint_body_checksum_offset..][0..8], body_checksum, .little);
    std.mem.writeInt(u64, descriptor[descriptor_checksum_offset..], std.hash.XxHash3.hash(0, descriptor), .little);
}

fn decodeEmptyCheckpoint(
    descriptor: *[block_size]u8,
    expected_slot: CheckpointSlot,
    backing_bytes: u64,
) ?Checkpoint {
    const stored_checksum = std.mem.readInt(u64, descriptor[descriptor_checksum_offset..], .little);
    std.mem.writeInt(u64, descriptor[descriptor_checksum_offset..], 0, .little);
    const checksum_valid = stored_checksum == std.hash.XxHash3.hash(0, descriptor);
    std.mem.writeInt(u64, descriptor[descriptor_checksum_offset..], stored_checksum, .little);

    if (!checksum_valid or
        !std.mem.eql(u8, descriptor[0..4], "VBLC") or
        descriptor[4] != 1 or
        descriptor[5] != @intFromEnum(expected_slot) or
        std.mem.readInt(u16, descriptor[6..8], .little) != empty_mapping_flag or
        std.mem.readInt(u32, descriptor[16..20], .little) != block_size or
        descriptor[68] != checksum_algorithm_xxh3_64 or
        !std.mem.allEqual(u8, descriptor[69..checkpoint_body_checksum_offset], 0) or
        std.mem.readInt(u64, descriptor[checkpoint_body_checksum_offset..][0..8], .little) != 0 or
        !std.mem.allEqual(u8, descriptor[checkpoint_body_checksum_offset + @sizeOf(u64) .. descriptor_checksum_offset], 0)) return null;

    const volume_blocks = std.mem.readInt(u32, descriptor[20..24], .little);
    if (volume_blocks == 0 or volume_blocks > @as(u32, 1) << 31) return null;
    const layout = layoutFor(volume_blocks);
    const backing_blocks = backingBlocksFor(layout, backing_bytes) catch return null;

    const physical_map_start = if (expected_slot == .green) layout.green_physical_map_start else layout.blue_physical_map_start;
    const checksum_map_start = if (expected_slot == .green) layout.green_checksum_map_start else layout.blue_checksum_map_start;
    const generation = std.mem.readInt(u64, descriptor[32..40], .little);
    const checkpoint_lsn = std.mem.readInt(u64, descriptor[40..48], .little);
    const last_footer_block = std.mem.readInt(u32, descriptor[48..52], .little);

    if (std.mem.readInt(u64, descriptor[24..32], .little) != backing_blocks or
        generation == 0 or
        checkpoint_lsn != 0 or
        last_footer_block != layout.log_start - 1 or
        std.mem.readInt(u32, descriptor[52..56], .little) != physical_map_start or
        std.mem.readInt(u32, descriptor[56..60], .little) != layout.physical_map_blocks or
        std.mem.readInt(u32, descriptor[60..64], .little) != checksum_map_start or
        std.mem.readInt(u32, descriptor[64..68], .little) != layout.checksum_map_blocks) return null;

    return .{
        .slot = expected_slot,
        .volume_id = std.mem.readInt(u64, descriptor[8..16], .little),
        .volume_blocks = volume_blocks,
        .backing_blocks = backing_blocks,
        .generation = generation,
        .checkpoint_lsn = checkpoint_lsn,
        .last_footer_block = last_footer_block,
        .layout = layout,
    };
}

pub fn open(allocator: std.mem.Allocator, dir_fd: linux.fd_t, backing_path: []const u8) !Volume {
    const fd = try std.posix.openat(dir_fd, backing_path, .{
        .ACCMODE = .RDWR,
        .DIRECT = true,
        .CLOEXEC = true,
    }, 0);
    errdefer close(fd);

    const backing_size_result = linux.lseek(fd, 0, linux.SEEK.END);
    if (linux.errno(backing_size_result) != .SUCCESS) return error.BackingSizeUnavailable;

    var ring = try linux.IoUring.init(2, 0);
    errdefer ring.deinit();

    var descriptors: [2 * block_size]u8 align(block_size) = undefined;
    _ = try ring.read(1, fd, .{ .buffer = &descriptors }, 0);
    if (try ring.submit() != 1) return error.UnexpectedSubmissionCount;
    try completeExactly(&ring, 1, descriptors.len);

    const green = decodeEmptyCheckpoint(descriptors[0..block_size], .green, backing_size_result);
    const blue = decodeEmptyCheckpoint(descriptors[block_size..], .blue, backing_size_result);
    const checkpoint = if (green) |green_checkpoint|
        if (blue) |blue_checkpoint|
            if (green_checkpoint.generation > blue_checkpoint.generation) green_checkpoint else blue_checkpoint
        else
            green_checkpoint
    else if (blue) |blue_checkpoint|
        blue_checkpoint
    else
        return error.NoValidCheckpoint;

    const physical_map_bytes = @as(usize, checkpoint.layout.physical_map_blocks) * block_size;
    const mapping_bytes = physical_map_bytes + @as(usize, checkpoint.layout.checksum_map_blocks) * block_size;
    const mapping_storage = try allocator.allocWithOptions(u8, mapping_bytes, .fromByteUnits(block_size), null);
    errdefer allocator.free(mapping_storage);
    @memset(mapping_storage, 0);

    return .{
        .allocator = allocator,
        .mapping_storage = mapping_storage,
        .backing_fd = fd,
        .io_uring = ring,
        .volume_id = checkpoint.volume_id,
        .volume_blocks = checkpoint.volume_blocks,
        .backing_blocks = checkpoint.backing_blocks,
        .log_start_block = checkpoint.layout.log_start,
        .physical_blocks = @as([*]u32, @ptrCast(mapping_storage.ptr))[0..checkpoint.volume_blocks],
        .checksums = @as([*]u64, @ptrCast(@alignCast(mapping_storage.ptr + physical_map_bytes)))[0..checkpoint.volume_blocks],
        .last_lsn = checkpoint.checkpoint_lsn,
        .durable_lsn = checkpoint.checkpoint_lsn,
        .last_footer_block = checkpoint.last_footer_block,
        .checkpoint_generation = checkpoint.generation,
        .next_checkpoint_slot = if (checkpoint.slot == .green) .blue else .green,
        .log_bytes_since_checkpoint = 0,
    };
}

pub fn format(dir_fd: linux.fd_t, backing_path: []const u8, volume_bytes: u64) !void {
    if (volume_bytes == 0 or volume_bytes % block_size != 0) return error.InvalidVolumeSize;

    const volume_blocks_u64 = volume_bytes / block_size;
    if (volume_blocks_u64 > @as(u64, 1) << 31) return error.InvalidVolumeSize;
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
    const backing_blocks = try backingBlocksFor(layout, backing_size_result);

    const volume_id = try randomVolumeId();
    var descriptors: [2 * block_size]u8 align(block_size) = undefined;
    encodeCheckpointDescriptor(
        descriptors[0..block_size],
        .green,
        empty_mapping_flag,
        1,
        0,
        layout.log_start - 1,
        volume_id,
        volume_blocks,
        backing_blocks,
        layout,
        0,
    );
    encodeCheckpointDescriptor(
        descriptors[block_size..],
        .blue,
        empty_mapping_flag,
        2,
        0,
        layout.log_start - 1,
        volume_id,
        volume_blocks,
        backing_blocks,
        layout,
        0,
    );

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

test "format validates volume size boundaries" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    try std.testing.expectError(error.InvalidVolumeSize, format(temporary_directory.dir.handle, "missing", 0));
    try std.testing.expectError(error.InvalidVolumeSize, format(temporary_directory.dir.handle, "missing", block_size - 1));

    const maximum_volume_blocks = @as(u64, 1) << 31;
    try std.testing.expectError(
        error.InvalidVolumeSize,
        format(temporary_directory.dir.handle, "missing", (maximum_volume_blocks + 1) * block_size),
    );

    const layout = layoutFor(@intCast(maximum_volume_blocks));
    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, (@as(u64, layout.log_start) + 2) * block_size);
    }

    try format(temporary_directory.dir.handle, "backing", maximum_volume_blocks * block_size);
}

test "format validates backing capacity boundaries" {
    const layout = layoutFor(1);
    const physical_address_space_blocks = @as(u64, std.math.maxInt(u32)) + 1;
    try std.testing.expectEqual(
        physical_address_space_blocks,
        try backingBlocksFor(layout, physical_address_space_blocks * block_size),
    );
    try std.testing.expectError(
        error.BackingTooLarge,
        backingBlocksFor(layout, (physical_address_space_blocks + 1) * block_size),
    );

    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "misaligned", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, (@as(u64, layout.log_start) + 2) * block_size - 1);
    }
    try std.testing.expectError(
        error.InvalidBackingSize,
        format(temporary_directory.dir.handle, "misaligned", block_size),
    );

    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "too-small", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, (@as(u64, layout.log_start) + 1) * block_size);
    }
    try std.testing.expectError(
        error.BackingTooSmall,
        format(temporary_directory.dir.handle, "too-small", block_size),
    );
}

test "format writes valid empty checkpoint roots and log geometry" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    const volume_blocks: u32 = 1025;
    const backing_blocks: u64 = 14;
    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, backing_blocks * block_size);
    }

    try format(temporary_directory.dir.handle, "backing", @as(u64, volume_blocks) * block_size);

    const backing = try temporary_directory.dir.openFile(std.testing.io, "backing", .{});
    defer backing.close(std.testing.io);
    var descriptors: [2 * block_size]u8 = undefined;
    try std.testing.expectEqual(descriptors.len, try backing.readPositionalAll(std.testing.io, &descriptors, 0));

    const volume_id = std.mem.readInt(u64, descriptors[8..16], .little);
    const physical_map_starts = [_]u32{ 2, 7 };
    const checksum_map_starts = [_]u32{ 4, 9 };
    for (0..2) |index| {
        var descriptor = descriptors[index * block_size ..][0..block_size];
        try std.testing.expectEqualSlices(u8, "VBLC", descriptor[0..4]);
        try std.testing.expectEqual(@as(u8, 1), descriptor[4]);
        try std.testing.expectEqual(@as(u8, @intCast(index)), descriptor[5]);
        try std.testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, descriptor[6..8], .little));
        try std.testing.expectEqual(volume_id, std.mem.readInt(u64, descriptor[8..16], .little));
        try std.testing.expectEqual(@as(u32, block_size), std.mem.readInt(u32, descriptor[16..20], .little));
        try std.testing.expectEqual(volume_blocks, std.mem.readInt(u32, descriptor[20..24], .little));
        try std.testing.expectEqual(backing_blocks, std.mem.readInt(u64, descriptor[24..32], .little));
        try std.testing.expectEqual(@as(u64, @intCast(index + 1)), std.mem.readInt(u64, descriptor[32..40], .little));
        try std.testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, descriptor[40..48], .little));
        try std.testing.expectEqual(@as(u32, 11), std.mem.readInt(u32, descriptor[48..52], .little));
        try std.testing.expectEqual(physical_map_starts[index], std.mem.readInt(u32, descriptor[52..56], .little));
        try std.testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, descriptor[56..60], .little));
        try std.testing.expectEqual(checksum_map_starts[index], std.mem.readInt(u32, descriptor[60..64], .little));
        try std.testing.expectEqual(@as(u32, 3), std.mem.readInt(u32, descriptor[64..68], .little));
        try std.testing.expectEqual(@as(u8, 1), descriptor[68]);

        const checksum = std.mem.readInt(u64, descriptor[descriptor_checksum_offset..], .little);
        @memset(descriptor[descriptor_checksum_offset..], 0);
        try std.testing.expectEqual(checksum, std.hash.XxHash3.hash(0, descriptor));
    }

    var empty_log: [2 * block_size]u8 = undefined;
    try std.testing.expectEqual(
        empty_log.len,
        try backing.readPositionalAll(std.testing.io, &empty_log, 12 * block_size),
    );
    var zeroes: [2 * block_size]u8 = undefined;
    @memset(&zeroes, 0);
    try std.testing.expectEqualSlices(u8, &zeroes, &empty_log);
}

test "open reconstructs state and selects a valid checkpoint root" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    const volume_blocks: u32 = 1025;
    const backing_blocks: u64 = 14;
    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, backing_blocks * block_size);
    }
    try format(temporary_directory.dir.handle, "backing", @as(u64, volume_blocks) * block_size);

    const backing = try temporary_directory.dir.openFile(std.testing.io, "backing", .{ .mode = .read_write });
    defer backing.close(std.testing.io);
    var descriptors: [2 * block_size]u8 = undefined;
    try std.testing.expectEqual(descriptors.len, try backing.readPositionalAll(std.testing.io, &descriptors, 0));
    const volume_id = std.mem.readInt(u64, descriptors[8..16], .little);

    {
        var volume = try open(std.testing.allocator, temporary_directory.dir.handle, "backing");
        defer volume.deinit();

        try std.testing.expectEqual(volume_id, volume.volume_id);
        try std.testing.expectEqual(volume_blocks, volume.volume_blocks);
        try std.testing.expectEqual(backing_blocks, volume.backing_blocks);
        try std.testing.expectEqual(@as(u32, 12), volume.log_start_block);
        try std.testing.expectEqual(@as(usize, volume_blocks), volume.physical_blocks.len);
        try std.testing.expectEqual(@as(usize, volume_blocks), volume.checksums.len);
        try std.testing.expect(std.mem.allEqual(u32, volume.physical_blocks, 0));
        try std.testing.expect(std.mem.allEqual(u64, volume.checksums, 0));
        try std.testing.expectEqual(@as(u64, 0), volume.last_lsn);
        try std.testing.expectEqual(@as(u64, 0), volume.durable_lsn);
        try std.testing.expectEqual(@as(u32, 11), volume.last_footer_block);
        try std.testing.expectEqual(@as(u64, 2), volume.checkpoint_generation);
        try std.testing.expectEqual(CheckpointSlot.green, volume.next_checkpoint_slot);
        try std.testing.expectEqual(@as(u64, 0), volume.log_bytes_since_checkpoint);
    }

    var green = descriptors[0..block_size];
    std.mem.writeInt(u64, green[32..40], 3, .little);
    std.mem.writeInt(u64, green[descriptor_checksum_offset..], 0, .little);
    std.mem.writeInt(u64, green[descriptor_checksum_offset..], std.hash.XxHash3.hash(0, green), .little);
    try backing.writePositionalAll(std.testing.io, green, 0);
    {
        var volume = try open(std.testing.allocator, temporary_directory.dir.handle, "backing");
        defer volume.deinit();
        try std.testing.expectEqual(@as(u64, 3), volume.checkpoint_generation);
        try std.testing.expectEqual(CheckpointSlot.blue, volume.next_checkpoint_slot);
    }

    green[0] ^= 0xff;
    try backing.writePositionalAll(std.testing.io, green, 0);
    {
        var volume = try open(std.testing.allocator, temporary_directory.dir.handle, "backing");
        defer volume.deinit();
        try std.testing.expectEqual(@as(u64, 2), volume.checkpoint_generation);
        try std.testing.expectEqual(CheckpointSlot.green, volume.next_checkpoint_slot);
    }

    descriptors[block_size] ^= 0xff;
    try backing.writePositionalAll(std.testing.io, descriptors[block_size..], block_size);
    try std.testing.expectError(
        error.NoValidCheckpoint,
        open(std.testing.allocator, temporary_directory.dir.handle, "backing"),
    );
}

test "write_block appends and publishes one-block record" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    const volume_blocks: u32 = 2;
    const layout = layoutFor(volume_blocks);
    const backing_blocks = @as(u64, layout.log_start) + 2;
    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, backing_blocks * block_size);
    }
    try format(temporary_directory.dir.handle, "backing", @as(u64, volume_blocks) * block_size);

    var volume = try open(std.testing.allocator, temporary_directory.dir.handle, "backing");
    defer volume.deinit();

    var payload: [block_size]u8 = undefined;
    for (&payload, 0..) |*byte, index| byte.* = @truncate(index);

    const lba: u32 = 1;
    const previous_footer_block = volume.last_footer_block;
    const payload_block = previous_footer_block + 1;
    const footer_block = payload_block + 1;
    try std.testing.expectError(error.InvalidLogicalBlock, volume.write_block(volume_blocks, &payload));
    try volume.write_block(lba, &payload);
    try std.testing.expectError(error.LogFull, volume.write_block(0, &payload));

    const backing = try temporary_directory.dir.openFile(std.testing.io, "backing", .{});
    defer backing.close(std.testing.io);
    var record: [2 * block_size]u8 = undefined;
    try std.testing.expectEqual(
        record.len,
        try backing.readPositionalAll(std.testing.io, &record, @as(u64, payload_block) * block_size),
    );
    try std.testing.expectEqualSlices(u8, &payload, record[0..block_size]);

    var addresses: [2 * @sizeOf(u32)]u8 = undefined;
    std.mem.writeInt(u32, addresses[0..4], lba, .little);
    std.mem.writeInt(u32, addresses[4..8], payload_block, .little);
    var payload_hasher = std.hash.XxHash3.init(volume.volume_id);
    payload_hasher.update(&addresses);
    payload_hasher.update(record[0..block_size]);
    const expected_payload_checksum = payload_hasher.final();

    var footer = record[block_size..];
    try std.testing.expectEqualSlices(u8, "VBLF", footer[0..4]);
    try std.testing.expectEqual(@as(u8, 1), footer[4]);
    try std.testing.expectEqual(@as(u8, write_record_kind), footer[5]);
    try std.testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, footer[6..8], .little));
    try std.testing.expectEqual(volume.volume_id, std.mem.readInt(u64, footer[8..16], .little));
    try std.testing.expectEqual(@as(u64, 1), std.mem.readInt(u64, footer[16..24], .little));

    const decoded_previous_footer_block = std.mem.readInt(u32, footer[24..28], .little);
    const decoded_footer_block = std.mem.readInt(u32, footer[28..32], .little);
    try std.testing.expectEqual(previous_footer_block, decoded_previous_footer_block);
    try std.testing.expectEqual(footer_block, decoded_footer_block);
    const payload_count = decoded_footer_block - decoded_previous_footer_block - 1;
    try std.testing.expectEqual(@as(u32, 1), payload_count);
    try std.testing.expectEqual(payload_block, decoded_footer_block - payload_count);

    try std.testing.expectEqual(lba, std.mem.readInt(u32, footer[footer_lba_offset..][0..4], .little));
    try std.testing.expectEqual(
        expected_payload_checksum,
        std.mem.readInt(u64, footer[footer_payload_checksum_offset..][0..8], .little),
    );
    try std.testing.expect(std.mem.allEqual(u8, footer[footer_lba_offset + @sizeOf(u32) .. footer_payload_checksum_offset], 0));
    try std.testing.expect(std.mem.allEqual(u8, footer[footer_payload_checksum_offset + @sizeOf(u64) .. footer_checksum_offset], 0));

    const stored_footer_checksum = std.mem.readInt(u64, footer[footer_checksum_offset..], .little);
    std.mem.writeInt(u64, footer[footer_checksum_offset..], 0, .little);
    try std.testing.expectEqual(stored_footer_checksum, std.hash.XxHash3.hash(0, footer));

    try std.testing.expectEqual(@as(u32, 0), volume.physical_blocks[0]);
    try std.testing.expectEqual(@as(u64, 0), volume.checksums[0]);
    try std.testing.expectEqual(payload_block, volume.physical_blocks[lba]);
    try std.testing.expectEqual(expected_payload_checksum, volume.checksums[lba]);
    try std.testing.expectEqual(@as(u64, 1), volume.last_lsn);
    try std.testing.expectEqual(@as(u64, 0), volume.durable_lsn);
    try std.testing.expectEqual(footer_block, volume.last_footer_block);
    try std.testing.expectEqual(@as(u64, 2 * block_size), volume.log_bytes_since_checkpoint);
}

test "flush advances durability through the current write" {
    var temporary_directory = std.testing.tmpDir(.{});
    defer temporary_directory.cleanup();

    const volume_blocks: u32 = 1;
    const layout = layoutFor(volume_blocks);
    {
        const backing = try temporary_directory.dir.createFile(std.testing.io, "backing", .{
            .read = true,
            .exclusive = true,
        });
        defer backing.close(std.testing.io);
        try backing.setLength(std.testing.io, (@as(u64, layout.log_start) + 2) * block_size);
    }
    try format(temporary_directory.dir.handle, "backing", @as(u64, volume_blocks) * block_size);

    var volume = try open(std.testing.allocator, temporary_directory.dir.handle, "backing");
    defer volume.deinit();

    try volume.flush();
    try std.testing.expectEqual(@as(u64, 0), volume.last_lsn);
    try std.testing.expectEqual(@as(u64, 0), volume.durable_lsn);

    var payload: [block_size]u8 = undefined;
    @memset(&payload, 0xa5);
    try volume.write_block(0, &payload);
    try std.testing.expectEqual(@as(u64, 1), volume.last_lsn);
    try std.testing.expectEqual(@as(u64, 0), volume.durable_lsn);

    try volume.flush();
    try std.testing.expectEqual(volume.last_lsn, volume.durable_lsn);
}
