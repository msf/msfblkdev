#!/bin/bash
set -euo pipefail

# Operator-only installation: never build code as root.
if [[ "$EUID" != 0 || -z "${SUDO_USER:-}" || "$SUDO_USER" == root ]]; then
    printf 'Run this installer through sudo as the intended non-root developer.\n' >&2
    exit 1
fi
if [[ ! "$SUDO_USER" =~ ^[a-z_][a-z0-9_-]*$ ]]; then
    printf 'Unsupported sudo user name.\n' >&2
    exit 1
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
source_binary="$repo/rust/target/debug/block-storage-mount"
if [[ ! -f "$source_binary" || -L "$source_binary" || ! -x "$source_binary" ]]; then
    printf 'Build first as the normal user: make build-ext4\n' >&2
    exit 1
fi
uid=$(id -u "$SUDO_USER")
gid=$(id -g "$SUDO_USER")
runtime=/run/ublksrvd
runtime_rule=/etc/tmpfiles.d/block-storage-ublk.conf
runtime_spec="d $runtime 0700 $uid $gid -"
if [[ -L "$runtime" ]] || { [[ -e "$runtime" ]] && { [[ ! -d "$runtime" ]] || [[ $(stat -c %u "$runtime") != "$uid" ]]; }; }; then
    printf 'Refusing to change an existing runtime directory not owned by %s: %s\n' "$SUDO_USER" "$runtime" >&2
    exit 1
fi
if [[ -L "$runtime_rule" ]] || { [[ -e "$runtime_rule" ]] && [[ $(< "$runtime_rule") != "$runtime_spec" ]]; }; then
    printf 'Refusing to overwrite a different runtime policy: %s\n' "$runtime_rule" >&2
    exit 1
fi
rule=$(mktemp /etc/sudoers.d/.block-storage-ext4.XXXXXX)
runtime_temp=$(mktemp /etc/tmpfiles.d/.block-storage-ublk.XXXXXX)
trap 'rm -f -- "$rule" "$runtime_temp"' EXIT
printf '%s\n' "$runtime_spec" > "$runtime_temp"
printf '%s ALL=(root) NOPASSWD: /usr/local/libexec/block-storage-mount\n' "$SUDO_USER" > "$rule"
chmod 0440 "$rule"
visudo -cf "$rule"
install -d -o root -g root -m 0755 /usr/local/libexec
install -o root -g root -m 0755 "$source_binary" /usr/local/libexec/block-storage-mount
install -o root -g root -m 0440 "$rule" "/etc/sudoers.d/block-storage-ext4-$SUDO_USER"
install -o root -g root -m 0644 "$runtime_temp" "$runtime_rule"
systemd-tmpfiles --create "$runtime_rule"
printf 'Installed the mount helper, private runtime directory and sudo rule for %s. No devices were created or mounted.\n' "$SUDO_USER"
printf 'The separate normal-user ublk setup in README.md is still required.\n'
