#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly DEST="${ROOT}/tools/build/workbench-native"
readonly MIRROR="https://archive.ubuntumirror.dei.uc.pt/ubuntu"
[[ "$(uname -s)-$(uname -m)" == Linux-x86_64 ]] || {
  echo 'native packages require Linux x86_64' >&2; exit 1;
}
mkdir -p "${DEST}/packages"
readonly STAGING="$(mktemp -d "${DEST}/staging.XXXXXX")"
trap 'rm -rf -- "${STAGING}"' EXIT

while read -r expected path; do
  archive="${DEST}/packages/${path##*/}"
  if [[ ! -f "${archive}" ]]; then
    curl --fail --silent --show-error --location --max-time 60 \
      "${MIRROR}/${path}" -o "${STAGING}/download"
    printf '%s  %s\n' "${expected}" "${STAGING}/download" | sha256sum --check --status
    mv "${STAGING}/download" "${archive}"
  fi
  printf '%s  %s\n' "${expected}" "${archive}" | sha256sum --check --status
  dpkg-deb --extract "${archive}" "${STAGING}/root"
done < "${ROOT}/tools/native/packages.sha256"

# Replace only this recipe's generated prefix, after every archive validates.
rm -rf -- "${DEST}/root"
mv "${STAGING}/root" "${DEST}/root"
cp "${ROOT}/tools/native/packages.sha256" "${DEST}/packages.sha256"
printf 'Native packages extracted: %s/root (no host installation)\n' "${DEST}"
