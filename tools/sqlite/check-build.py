#!/usr/bin/env python3
"""Check the local SQLite build against its recorded sources and artifacts."""
import hashlib
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TOOLS = ROOT / "tools/build/sqlite-3.51.3"


def check_digest(path, expected):
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"missing regular file: {path}; run make setup-sqlite")
    with path.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"build identity mismatch: {path}; run make setup-sqlite")


def main():
    manifest = json.loads((TOOLS / "build-manifest.json").read_text())
    if manifest["sqlite_version"] != "3.51.3":
        raise ValueError("unexpected SQLite version")
    for name in ("sqlite3", "kvtest", "speedtest1", "sqlite3.o"):
        check_digest(TOOLS / "build" / name, manifest["artifacts_sha256"][name])
    for name, field in (
        ("sqlite_lab.c", "adapter_source_sha256"),
        ("wal-observer.c", "observer_source_sha256"),
        ("sqlite-autocheckpoint-observer.patch", "patch_sha256"),
    ):
        check_digest(ROOT / "tools/sqlite" / name, manifest["adaptation"][field])
    print("SQLite 3.51.3 sources and build artifacts: PASS")


if __name__ == "__main__":
    main()
