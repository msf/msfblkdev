# ADR-05 fault-lab dependencies

Authored by LLM. This inventory is for the [portable fault lab](../../docs/ADR05-FAULT-LAB.md). A new machine does not need `vmlab`, OrbStack, a private golden image, or any files from the developer's home directory. No launcher or qualified image is bundled yet.

## Host: choose one public VM provider

| Host | Native guest | Provider and prerequisites |
|---|---|---|
| Linux arm64 | arm64 | QEMU `qemu-system-aarch64` with KVM access and AArch64 UEFI firmware. On Debian: `qemu-system-arm qemu-utils qemu-efi-aarch64`. |
| Linux amd64 | amd64 | QEMU `qemu-system-x86_64` with KVM access and x86 UEFI firmware. On Debian: `qemu-system-x86 qemu-utils ovmf`. |
| macOS arm64 | arm64 | [Lima](https://lima-vm.io/) with the VZ driver (`brew install lima` if using Homebrew). Use a VM with no host directory mounts. |
| macOS amd64 (Intel) | amd64 | [QEMU](https://www.qemu.org/download/) with HVF and x86 firmware. Check `qemu-system-x86_64 -accel help` for `hvf`. |

Use a matching [Debian 13 generic cloud image](https://cloud.debian.org/images/cloud/trixie/) and a second, empty virtual disk, both file-backed. Use SSH/SCP or the provider's equivalent for guest commands and file transfer. With a generic cloud image, provision a normal user and its SSH public key via cloud-init; seed generation is a host provisioning detail, not a guest test dependency. Record the dated image URL, verified digest, provider version and guest kernel for each architecture. Don't treat `latest` as a pin. The [first injector run](../../evidence/adr05-probe-arm64-20261119/run.txt) records these pins for macOS arm64 with Lima/VZ. The other provider rows haven't run this probe.

## Builder and test guest: Debian 13

[Build packages](debian13-build.packages) provide the native compiler, bindgen dependencies and autotools. [Guest packages](debian13-guest.packages) provide the storage daemon (`qemu-utils`), FUSE tools, `fio` for control I/O, and tools for device discovery and ext4 scratch storage. Install only the relevant list on each machine. For a native build **inside the disposable test guest**, run from the repository root:

```sh
sudo apt-get update
xargs -a tools/adr05-fault-lab/debian13-build.packages sudo apt-get install -y --no-install-recommends
xargs -a tools/adr05-fault-lab/debian13-guest.packages sudo apt-get install -y --no-install-recommends
```

The injector probe needs Python 3, a Linux C compiler and `liburing-dev`. It does not need a Rust build. For later engine tests, two build dependencies do not come from those lists:

- **Rust and Cargo:** install a native Linux toolchain through [rustup](https://rustup.rs/). The recorded arm64 lab used `1.97.1`. Build with the repository's `rust/Cargo.lock`, for example `cd rust && cargo +1.97.1 build --locked --features test-failpoints,lab --bins`. Record `rustc -vV` and the executable hashes; if building elsewhere, check the resulting binaries' shared libraries in the guest.
- **ublk ownership helper:** the local `ublksrv/` checkout is **not tracked**. Fetch the public source explicitly on the builder before following the [normal-user ublk setup](../../README.md#normal-user-ublk-setup):

  ```sh
  git clone https://github.com/ublk-org/ublksrv.git ublksrv
  git -C ublksrv checkout --detach 23461b7aa314d2ee9e014356c210a5bfaa9eecec
  (cd ublksrv && autoreconf -fi && ./configure)
  ```

  Install the matching helper, shared library, restricted udev rules and private runtime directory **in the test guest**. Don't install them on the host. The pinned revision is the one used in earlier arm64 ublk testing, not evidence of a backing-fault test.

## What packages cannot verify

The guest kernel must support `io_uring`, `ublk_drv` and FUSE. The test user needs access to `/dev/ublk-control`, its own ublk nodes and `/dev/fuse`. The guest scratch filesystem must support direct I/O. `qemu-storage-daemon` must support `raw`, `file`, `blkdebug` and a FUSE export. Package installation alone does not prove those capabilities: require the no-fault and one-shot write/flush probes in the topology plan before accepting results. Save resolved package versions (`dpkg-query -W`) with the evidence. Nested KVM, Docker, LVM, ZFS, and the ADR-10 benchmark VM are not dependencies for this first gate.

## Which command goes in the dev loop?

Run `make test-fault-model` on Linux or macOS to check the bounded medium and logical history without starting an engine. Run `make test-dst` on Linux to run those controls and exercise both descriptor-survival outcomes against format-4 recovery. It uses temporary regular files and needs `io_uring` and direct I/O, but no QEMU or ublk device. To test the real backing-error path in a provisioned guest, set `SCRATCH` and `DEVICE` as described below, then run `make test-fault`. The target builds the probe and example and stops at the first failure. Redirect its output to a new log on the guest OS disk for evidence. Neither target qualifies the full [ADR-05 campaign](../../docs/ADR05-CAMPAIGN.md).

## Run the injector probe

Inside the disposable VM, identify the ext4 filesystem on the **second** virtual disk with `lsblk -f`. Use a writable directory on that filesystem for `SCRATCH`, and its device or `/dev/disk/by-uuid/…` path for `DEVICE`. The runner checks the device identity but never opens the device for I/O. Keep the source and output log on the guest OS disk.

Copy this tool directory to the guest. From that directory, build and run as the normal user (set `SCRATCH` and `DEVICE` first):

```sh
cc -std=c11 -Wall -Wextra -Werror -O2 -o probe-io probe-io.c -luring
timeout -k 5s 90s python3 -u guest-probe.py "${SCRATCH:?}" "${DEVICE:?}" >probe.log 2>&1
```

Require exit status zero and `injector=PASS`. Each case creates a private 4 KiB image and starts a new storage daemon. The caller opens the exported raw node with `O_DIRECT`. The control must write, sync and read the same bytes. The write case must see `EIO` from `pwrite` and then write successfully. The flush case must write successfully, see `EIO` from `fsync`, dirty the block again and sync successfully. All three compare the exported bytes and, after daemon shutdown, the underlying file. The caller submits at most 20 KiB of writes across the three cases; this excludes filesystem metadata.

Copy the log off the VM before teardown. A failed case exits nonzero and leaves its fixture at the printed path. Preserve that image and inspect the trace before deleting the VM. This test doesn't start ublk or the engine.

## Fail the first engine checkpoint on close

After the injector probe passes, copy `rust/` to the guest and build the example with the repository lockfile. From the repository root inside the guest:

```sh
(cd rust && cargo +1.97.1 build --locked --example adr05-engine)
timeout -k 5s 90s python3 -u tools/adr05-fault-lab/engine-checkpoint.py \
  "${SCRATCH:?}" "${DEVICE:?}" rust/target/debug/examples/adr05-engine >engine.log 2>&1
```

Require exit status zero and `engine_checkpoint=PASS`. Each case formats a new 1,581,056-byte image before exposing it through FUSE. The client writes logical blocks 1 and 5, overwrites block 1 and calls `Volume::flush`. `Volume::close` then tries to publish the blue descriptor (physical byte offset 4096). Only the fault case has a one-shot write rule for sector 8. The control must update the descriptor; the fault case must return `EIO` and leave both roots unchanged.

The runner stops QEMU, copies each image and opens only the copy for recovery. It compares all eight logical blocks with the expected bytes, including the six unwritten blocks. Both original images and both recovery copies stay under the printed fixture directory. Copy the log and faulted image off the VM before teardown. The [native arm64 trace](../../evidence/adr05-engine-arm64-20261119/run.txt) covers `Volume` through `io_uring`. It doesn't exercise a live ublk client, a checkpoint during reclamation, a failed final fsync or power loss.
