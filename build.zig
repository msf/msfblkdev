const std = @import("std");

pub fn build(b: *std.Build) void {
    const storage = b.createModule(.{
        .root_source_file = b.path("src/root.zig"),
        .target = b.standardTargetOptions(.{}),
        .optimize = b.standardOptimizeOption(.{}),
    });
    b.installArtifact(b.addLibrary(.{
        .name = "block-storage",
        .root_module = storage,
    }));

    const tests = b.addTest(.{
        .root_module = storage,
    });

    const test_step = b.step("test", "Run tests");
    test_step.dependOn(&b.addRunArtifact(tests).step);
}
