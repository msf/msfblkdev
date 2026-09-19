"""Count backing I/O with the public ledger API; timing is diagnostic only."""
import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

from run import checked, prepare


def build_binary(repo, build, version):
    prepare(repo, build, version)
    example = build / version / "rust/examples/mapping_io_counts"
    example.mkdir(exist_ok=True)
    source = repo / "tools/mapping-bench"
    shutil.copyfile(source / "io-counts.rs", example / "main.rs")
    adapter = (source / f"{version.replace('.', '')}.rs").read_text()
    (example / "options.rs").write_text(adapter.split("\nfn stats(", 1)[0])
    env = dict(os.environ, CARGO_TARGET_DIR=str(build / f"target-{version}"))
    with (build / f"io-counts-{version}.log").open("w") as log:
        for action in ("build", "clippy"):
            command = ["cargo", action, "--offline", "--locked", "--release", "--example", "mapping_io_counts"]
            if action == "clippy":
                command += ["--", "-D", "warnings"]
            checked(command, cwd=example.parents[1], env=env, stdout=log, stderr=log, timeout=120)
    return build / f"target-{version}/release/examples/mapping_io_counts"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    build = args.build_dir.resolve()
    build.mkdir(exist_ok=True)
    binaries = {v: build_binary(repo, build, v) for v in ("v1.1", "v1.2")}
    with args.output.open("x") as output:
        for version, budget in (("v1.1", "default"), ("v1.2", "default"), ("v1.2", str(16 << 20))):
            for pages, flush in ((3277, 1), (3277, 32), (3277, 256), (3277, 1024), (32, 32)):
                fixture = Path(tempfile.mkdtemp(prefix=".mapping-io-counts-", dir=repo))
                checked(["chattr", "+C", str(fixture)])
                result = checked([str(binaries[version]), str(fixture), budget, str(flush), str(pages)],
                                 capture_output=True, timeout=120)
                row = dict(version=version, budget=budget)
                row.update((key, int(value)) for key, value in
                           (field.split("=", 1) for field in result.stdout.strip().split()))
                output.write(json.dumps(row) + "\n")
                output.flush()
                fixture.rmdir()
                print(f"PASS {version} {budget} pages={pages} flush={flush}", flush=True)


if __name__ == "__main__":
    main()
