#!/usr/bin/env bash
# Prepares a Debian 13 or Ubuntu 24.04+ machine to run every test in `make test-all`.
# Run it as the normal developer user; it calls sudo only for the system changes it lists.
# Each step checks its current state first, so running the script again is safe.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || exit 2
readonly ROOT
# The ublksrv revision used in the earlier arm64 ublk tests.
readonly UBLKSRV_URL=https://github.com/ublk-org/ublksrv.git
readonly UBLKSRV_COMMIT=23461b7aa314d2ee9e014356c210a5bfaa9eecec
readonly UDEV_RULE=/etc/udev/rules.d/90-ublk.rules
readonly MODULES_CONF=/etc/modules-load.d/block-storage.conf
readonly HELPER=/usr/local/libexec/block-storage-mount
# Build: Rust (bindgen needs clang), ublksrv (autotools, liburing), SQLite (curl, patch, unzip).
# Test: filesystems, fio, python3 and the XXH3 library for the ADR-05 image checker.
readonly PACKAGES=(
  autoconf automake build-essential ca-certificates clang curl git libclang-dev libtool
  liburing-dev pkg-config patch unzip
  e2fsprogs xfsprogs fio kmod udev util-linux python3 libxxhash0
)

step() { printf '\n== %s\n' "$*"; }
fail() { printf 'setup: %s\n' "$*" >&2; exit 1; }

check_host() {
  [[ "${EUID}" != 0 ]] || fail 'run as the normal developer user, not root; the script calls sudo itself'
  [[ "$(uname -s)" == Linux ]] || fail 'native setup needs Linux; on macOS use: make vm-setup'
  if command -v systemd-detect-virt > /dev/null && systemd-detect-virt --container --quiet; then
    fail 'containers cannot load ublk_drv; use a machine or VM with its own kernel'
  fi
  command -v apt-get > /dev/null || fail "no apt-get; install these packages by hand: ${PACKAGES[*]}"
}

confirm() {
  cat << EOF
This setup makes these system changes through sudo:
  - installs packages: ${PACKAGES[*]} (and rustup if it is missing)
  - installs the ublksrv owner helper and library under /usr/local and the udev rule ${UDEV_RULE}
    (group plugdev may use /dev/ublk-control), and adds ${USER} to group plugdev
  - loads ublk_drv and xfs now and at every boot (${MODULES_CONF})
  - installs the mount helper ${HELPER} with a passwordless sudo rule for ${USER}
    and the runtime directory /run/ublksrvd (see README "Normal-user ext4 development")
It also clones ublksrv into ./ublksrv and builds SQLite under tools/build/ as ${USER}.
EOF
  [[ "${YES:-}" == 1 ]] && return
  local answer
  read -r -p 'Proceed? [y/N] ' answer
  [[ "${answer}" == y || "${answer}" == Y ]] || fail 'cancelled'
}

install_packages() {
  step 'Packages'
  local missing=() package wanted=("${PACKAGES[@]}")
  # Keep an existing rustup (for example from rustup.rs); otherwise use the distribution's.
  command -v rustup > /dev/null || wanted+=(rustup)
  for package in "${wanted[@]}"; do
    dpkg-query -W -f='${Status}' "${package}" 2> /dev/null | grep -q 'install ok installed' ||
      missing+=("${package}")
  done
  if [[ "${#missing[@]}" == 0 ]]; then echo 'already installed'; return; fi
  sudo apt-get update -q
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${missing[@]}"
}

install_rust() {
  step 'Rust toolchain'
  rustup default > /dev/null 2>&1 || rustup default stable
  rustup component add rustfmt clippy
  cargo --version
}

build_ublksrv() {
  step "ublksrv owner helper (${UBLKSRV_COMMIT:0:7})"
  local tree="${ROOT}/ublksrv"
  if [[ ! -d "${tree}" ]]; then
    git clone --quiet "${UBLKSRV_URL}" "${tree}"
    git -C "${tree}" checkout --quiet --detach "${UBLKSRV_COMMIT}"
  fi
  local head
  head="$(git -C "${tree}" rev-parse HEAD)"
  [[ "${head}" == "${UBLKSRV_COMMIT}" ]] ||
    printf 'note: ./ublksrv is at %s, not the tested %s; using it unchanged\n' "${head:0:7}" "${UBLKSRV_COMMIT:0:7}"
  if [[ ! -x "${tree}/ublk_user_id" ]]; then
    (cd "${tree}" && autoreconf -fi && ./configure --quiet)
    make -C "${tree}/lib" --quiet libublksrv.la
    make -C "${tree}" --quiet ublk_user_id
  fi
  sudo "${tree}/libtool" --quiet --mode=install install -m 0755 "${tree}/lib/libublksrv.la" /usr/local/lib
  sudo "${tree}/libtool" --quiet --mode=install install -m 0755 "${tree}/ublk_user_id" /usr/local/sbin
  sudo install -m 0755 "${tree}/utils/ublk_chown.sh" /usr/local/sbin/ublk_chown.sh
  sudo ldconfig
}

# The vendored rule grants mode 0666 on /dev/ublk-control; restrict it to group plugdev.
install_udev_rule() {
  step "udev rule ${UDEV_RULE}"
  local rule
  rule="$(mktemp)"
  sed -e 's/KERNEL=="ublk-control", MODE="0666"/KERNEL=="ublk-control", GROUP="plugdev", MODE="0660"/' \
    -e 's/",KERNEL/", KERNEL/g' -e 's/",RUN/", RUN/g' "${ROOT}/ublksrv/utils/ublk_dev.rules" > "${rule}"
  grep -q 'GROUP="plugdev", MODE="0660"' "${rule}" || fail 'unexpected ublk_dev.rules format; follow the README by hand'
  if ! cmp -s "${rule}" "${UDEV_RULE}"; then
    sudo install -m 0644 "${rule}" "${UDEV_RULE}"
    sudo udevadm control --reload-rules
  fi
  rm -f -- "${rule}"
  id -nG "${USER}" | tr ' ' '\n' | grep -qx plugdev || sudo usermod -aG plugdev "${USER}"
}

load_modules() {
  step 'Kernel modules ublk_drv and xfs'
  modinfo ublk_drv > /dev/null 2>&1 || fail "kernel $(uname -r) has no ublk_drv module; use a VM (make vm-setup)"
  printf 'ublk_drv\nxfs\n' | sudo tee "${MODULES_CONF}" > /dev/null
  sudo modprobe -a ublk_drv xfs
  sudo udevadm trigger --settle --action=add --name-match=ublk-control
  ls -l /dev/ublk-control
}

install_mount_helper() {
  step "Mount helper ${HELPER}"
  make -C "${ROOT}" --no-print-directory build-ext4
  if cmp -s "${ROOT}/rust/target/debug/block-storage-mount" "${HELPER}"; then
    echo 'installed helper matches the current build'
    return
  fi
  sudo bash "${ROOT}/scripts/install-ext4-helper.sh"
}

build_sqlite() {
  step 'SQLite 3.51.3 test tools'
  if [[ -x "${ROOT}/tools/build/sqlite-3.51.3/build/sqlite3" ]]; then echo 'already built'; return; fi
  make -C "${ROOT}" --no-print-directory setup-sqlite
}

# The plugdev membership applies to new logins only; check through sg until then.
verify() {
  step 'Preflight checks (create no device or mount)'
  local checks='make --no-print-directory check-ext4 check-xfs check-sqlite'
  if id -nG | tr ' ' '\n' | grep -qx plugdev; then
    (cd "${ROOT}" && bash -c "${checks}")
  else
    (cd "${ROOT}" && sg plugdev -c "${checks}")
    printf '\nGroup plugdev applies at your next login; make test-all uses sg plugdev until then.\n'
  fi
  printf '\nSetup complete. Run make test-all, then read evidence/test-all-latest/summary.md.\n'
}

check_host
confirm
sudo -v
install_packages
install_rust
build_ublksrv
install_udev_rule
load_modules
install_mount_helper
build_sqlite
verify
