.PHONY: setup-blockcheck-host setup-sqlite build build-rust build-ext4 lint lint-rust test test-acceptance test-ublk-fio test-ext4 check-ext4 test-xfs check-xfs check-sqlite check-sqlite-ext4 test-sqlite test-sqlite-ext4 test-sqlite-ext4-one test-rust

build: build-rust

build-rust: build-ext4


lint: lint-rust

lint-rust:
	@cd rust && cargo fmt --check && cargo clippy --all-targets --features lab -- -D warnings

test:
	@cd rust && cargo run --quiet --features lab --bin block-storage-lab -- test

# Linux, opt-in: one disposable regular file. The timeout covers I/O, not compilation.
.PHONY: test-compaction-small
test-compaction-small:
	@cd rust && cargo test --locked --release --test compaction_small --no-run
	@set -eu; dir=$$(mktemp -d); trap 'rm -rf -- "$$dir"' EXIT; \
		cd rust && BLOCK_STORAGE_SMALL_FIXTURE="$$dir" timeout --kill-after=5s 120s \
		cargo test --locked --release --test compaction_small -- --ignored --nocapture

# Focused format-4 recovery test. No QEMU, ublk or second disk required.
.PHONY: test-dst test-fault
test-dst:
	@cd rust && cargo test --locked --lib descriptor_survival --no-run
	@cd rust && timeout -k 5s 60s cargo test --locked --lib descriptor_survival -- --nocapture

# Guest only: never choose a scratch filesystem on behalf of the operator.
test-fault:
	@: "$${SCRATCH:?set SCRATCH to the writable disposable guest directory}" "$${DEVICE:?set DEVICE to its ext4 device}"
	@cd tools/adr05-fault-lab && $(CC) -std=c11 -Wall -Wextra -Werror -O2 -o probe-io probe-io.c -luring
	@timeout -k 5s 90s python3 -B -u tools/adr05-fault-lab/guest-probe.py "$$SCRATCH" "$$DEVICE"
	@cargo build --locked --manifest-path rust/Cargo.toml --target-dir rust/target --example adr05-engine
	@timeout -k 5s 90s python3 -B -u tools/adr05-fault-lab/engine-checkpoint.py "$$SCRATCH" "$$DEVICE" rust/target/debug/examples/adr05-engine

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

# Explicit operator opt-in after each boot: loads ublk_drv and verifies the one-time setup.
setup-blockcheck-host:
	@sudo bash scripts/setup-blockcheck-host.sh

# Optional SQLite tooling is deliberately outside the normal setup/test path.
setup-sqlite:
	@timeout --kill-after=5s 300s bash tools/sqlite/setup.sh

.PHONY: setup-native setup-rocksdb setup-go-ycsb
setup-native:
	@timeout --kill-after=5s 600s bash tools/native/setup.sh

setup-rocksdb:
	@timeout --kill-after=10s 1000s bash tools/rocksdb/setup.sh

setup-go-ycsb: setup-native
	@timeout --kill-after=5s 300s bash tools/go-ycsb/setup.sh

check-sqlite: build-ext4
	@timeout --kill-after=5s 30s uv run --quiet python tools/sqlite/check-build.py
	@tools/build/sqlite-3.51.3/build/sqlite3 --version

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

.PHONY: check-sqlite-xfs test-sqlite-xfs test-sqlite-xfs-one
check-sqlite-xfs: check-sqlite check-xfs

test-sqlite-xfs: check-sqlite-xfs
	@rust/target/debug/block-storage-lab sqlite-xfs

test-sqlite-xfs-one: check-sqlite-xfs
	@test -n "$(SQLITE_CASE)" -a -n "$(SQLITE_PROFILE)" -a -n "$(SQLITE_REPETITION)" || \
		{ echo "set SQLITE_CASE, SQLITE_PROFILE, and SQLITE_REPETITION"; exit 2; }
	@rust/target/debug/block-storage-lab sqlite-xfs \
		"$(SQLITE_CASE)" "$(SQLITE_PROFILE)" "$(SQLITE_REPETITION)"

# Checks XFS prerequisites and capacity without creating a backing image, device, or mount.
check-xfs: build-ext4
	@rust/target/debug/block-storage-lab xfs-preflight

# Explicit operator opt-in; the lab itself stays unprivileged.
test-xfs: build-ext4
	@rust/target/debug/block-storage-lab xfs

test-rust:
	@cd rust && cargo run --quiet --features lab --bin block-storage-lab -- test
