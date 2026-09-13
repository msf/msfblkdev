ZIG_VERSION := 0.16.0
ZIG_DIR := .tools/zig-x86_64-linux-$(ZIG_VERSION)
ZIG := $(ZIG_DIR)/zig
ZIG_URL := https://ziglang.org/download/$(ZIG_VERSION)/zig-x86_64-linux-$(ZIG_VERSION).tar.xz
ZIG_SHA256 := 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00

.PHONY: setup setup-sqlite build build-zig build-rust build-ext4 lint lint-zig lint-rust test test-acceptance test-ublk-fio test-ext4 check-ext4 test-xfs check-xfs check-sqlite check-sqlite-ext4 test-sqlite test-sqlite-ext4 test-sqlite-ext4-one test-zig test-rust run

build: build-rust

build-zig: setup
	@$(ZIG) build

build-rust: build-ext4

setup:
	@set -eu; \
	if [ "$$(uname -s)-$$(uname -m)" != Linux-x86_64 ]; then \
		echo "setup supports Linux x86_64 only"; exit 1; \
	fi; \
	if [ -x "$(ZIG)" ] && [ "$$($(ZIG) version 2>/dev/null)" = "$(ZIG_VERSION)" ]; then exit 0; fi; \
	mkdir -p .tools; \
	tmp="$$(mktemp -d .tools/zig-setup.XXXXXX)"; \
	trap 'rm -rf "$$tmp"' EXIT HUP INT TERM; \
	echo "Downloading Zig $(ZIG_VERSION)"; \
	curl --fail --silent --show-error --location "$(ZIG_URL)" -o "$$tmp/zig.tar.xz"; \
	printf '%s  %s\n' "$(ZIG_SHA256)" "$$tmp/zig.tar.xz" | sha256sum --check --status; \
	tar -xJf "$$tmp/zig.tar.xz" -C "$$tmp"; \
	rm -rf "$(ZIG_DIR)"; \
	mv "$$tmp/zig-x86_64-linux-$(ZIG_VERSION)" "$(ZIG_DIR)"

lint: lint-rust

lint-zig: setup
	@$(ZIG) fmt --check build.zig src

lint-rust:
	@cd rust && cargo fmt --check && cargo clippy --all-targets --features lab -- -D warnings

test:
	@cd rust && cargo run --quiet --features lab --bin block-storage-lab -- test

test-acceptance:
	@cd rust && cargo run --quiet --features lab --bin block-storage-lab -- engine-crash

# Privileged and opt-in: preflight refuses to create resources without ublk access.
test-ublk-fio:
	@cd rust && cargo build --quiet --features test-failpoints,lab --bin block-storage-ublk --bin block-storage-lab
	@cd rust && cargo run --quiet --features test-failpoints,lab --bin block-storage-lab -- ublk-fio

# Build as the normal user. The operator installs only the separate mount helper.
build-ext4:
	@cd rust && cargo build --quiet --bin block-storage-ublk --bin block-storage-mount
	@cd rust && cargo build --quiet --features lab --bin block-storage-lab

# Checks prerequisites without creating a backing image, device, or mount.
check-ext4: build-ext4
	@rust/target/debug/block-storage-lab ext4-preflight

# Explicit operator opt-in; the lab itself stays unprivileged.
test-ext4: build-ext4
	@rust/target/debug/block-storage-lab ext4

# Optional SQLite tooling is deliberately outside the normal setup/test path.
setup-sqlite:
	@timeout --kill-after=5s 300s bash scripts/setup-sqlite.sh

check-sqlite: build-ext4
	@timeout --kill-after=5s 30s uv run --quiet python tools/sqlite/check-build.py
	@.tools/sqlite-3.51.3/build/sqlite3 --version

# Explicit operator opt-in; runs one fresh ext4 WAL/FULL probe, not acceptance.
test-sqlite: check-sqlite
	@rust/target/debug/block-storage-lab sqlite-probe

# Checks tools and ext4 prerequisites without creating a device or mount.
check-sqlite-ext4: check-sqlite check-ext4

# Explicit operator opt-in; fail-fast 29-profile/case x 3-repetition ext4 matrix (87 fresh images).
test-sqlite-ext4: check-sqlite-ext4
	@rust/target/debug/block-storage-lab sqlite-ext4

# Focused operator run. The lab accepts only a whitelisted matrix member and repetition 1..3.
test-sqlite-ext4-one: check-sqlite-ext4
	@test -n "$(SQLITE_CASE)" -a -n "$(SQLITE_PROFILE)" -a -n "$(SQLITE_REPETITION)" || \
		{ echo "set SQLITE_CASE, SQLITE_PROFILE, and SQLITE_REPETITION"; exit 2; }
	@rust/target/debug/block-storage-lab sqlite-ext4 \
		"$(SQLITE_CASE)" "$(SQLITE_PROFILE)" "$(SQLITE_REPETITION)"

# Checks XFS prerequisites and capacity without creating a backing image, device, or mount.
check-xfs: build-ext4
	@rust/target/debug/block-storage-lab xfs-preflight

# Explicit operator opt-in; the lab itself stays unprivileged.
test-xfs: build-ext4
	@rust/target/debug/block-storage-lab xfs

test-zig: setup
	@$(ZIG) build test

test-rust:
	@cd rust && cargo run --quiet --features lab --bin block-storage-lab -- test

run: setup
	@$(ZIG) test src/root.zig --test-filter "V0.4 black-box acceptance"
