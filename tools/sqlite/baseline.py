#!/usr/bin/env python3
"""Select and compare complete SQLite/ext4 measurement matrices."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import sys

RUNS = 87
GROUPS = 29
MAX_JSON = 4 * 1024 * 1024
MAX_RESULT = 2 * 1024 * 1024
PROFILES = ("delete-extra", "wal-full", "wal-normal")
CASES = ("speedtest-main", "kv-sequential-read-small", "kv-random-read-small",
         "kv-sequential-read-large", "kv-random-read-large",
         "kv-sequential-update-small-single-transaction",
         "kv-sequential-update-small-per-op-commit",
         "kv-random-update-large-single-transaction",
         "kv-random-update-large-per-op-commit")
LONG = "kv-long-wal-checkpoint-stress"


def catalog():
    groups = [(case, profile) for profile in PROFILES for case in CASES]
    groups += [(LONG, profile) for profile in PROFILES[1:]]
    return [f"{case}--{profile}--repetition-{repetition}"
            for case, profile in groups for repetition in (1, 2, 3)]


class Invalid(ValueError):
    pass


def read_json(path, limit=MAX_JSON, jsonl=False):
    try:
        size = path.stat().st_size
        if not path.is_file() or size <= 0 or size > limit:
            raise Invalid(f"invalid file size: {path}")
        raw = path.read_bytes()
        if jsonl and (raw.count(b"\n") != 1 or not raw.endswith(b"\n")):
            raise Invalid(f"result must contain exactly one JSON line: {path}")
        return json.loads(raw, parse_constant=lambda value: (_ for _ in ()).throw(
            Invalid(f"non-finite JSON number: {value}")))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError) as error:
        raise Invalid(f"cannot read JSON {path}: {error}") from error


def digest(path):
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_JSON:
            raise Invalid(f"hash artifact is not a bounded regular file: {path}")
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError as error:
        raise Invalid(f"cannot hash artifact {path}: {error}") from error


def required_number(value, name, integer=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise Invalid(f"{name} is not numeric")
    if not math.isfinite(value) or value <= 0 or (integer and not isinstance(value, int)):
        raise Invalid(f"{name} is invalid")
    return value


def valid_unavailable(value):
    return isinstance(value, dict) and value.get("available") is False


def metric(record, name):
    workload = record["phases"]["workload_plus_drain"]
    if name in ("workload_ns", "workload_plus_drain_ns"):
        return required_number(workload["timings"].get(name), name, True)
    value = workload.get(name)
    if name == "operations_per_second" and valid_unavailable(value):
        value = workload.get("benchmark_subtest_rate", value)
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return required_number(value, name)
    if valid_unavailable(value):
        return None
    raise Invalid(f"{name} is neither available nor explicitly unavailable")


def validate_result(record, identity):
    labels = record.get("labels", {})
    expected = f"{labels.get('case')}--{labels.get('profile')}--repetition-{labels.get('repetition')}"
    if (record.get("schema"), record.get("schema_version"), record.get("scope"),
            record.get("status"), labels.get("fs"), labels.get("expected")) != (
            "block-storage-sqlite", 2, "ext4_matrix", "pass", "Ext4", 3):
        raise Invalid(f"invalid or non-passing result: {identity}")
    if expected != identity or record.get("errors") != []:
        raise Invalid(f"result identity or errors invalid: {identity}")
    ledger = record.get("metadata", {}).get("final_ledger", {})
    if (ledger.get("valid") is not True or ledger.get("pending_operations") != 0
            or ledger.get("errors") != 0 or ledger.get("completed_totals_incomplete") is not False
            or ledger.get("budget_exhausted") is not False):
        raise Invalid(f"invalid final ledger: {identity}")
    limit = required_number(ledger.get("write_limit_bytes"), "write limit", True)
    reserved = required_number(ledger.get("write_reserved_bytes"), "reserved writes", True)
    completed = required_number(ledger.get("write_completed_bytes"), "completed writes", True)
    if limit != 512 * 1024 * 1024 or not completed <= reserved <= limit:
        raise Invalid(f"final write budget is invalid: {identity}")
    validations = record.get("validations")
    phases = {"validation", "reopen_revalidation"}
    if (not isinstance(validations, list) or len(validations) != 2
            or {item.get("phase") for item in validations} != phases):
        raise Invalid(f"two logical validations are required: {identity}")
    for item in validations:
        if (item.get("integrity_check") != "ok" or item.get("foreign_key_rows") != 0
                or item.get("reference_match") is not True
                or not isinstance(item.get("sha3sum_schema"), str) or not item["sha3sum_schema"]):
            raise Invalid(f"logical validation failed: {identity}")
    metric(record, "workload_ns")
    metric(record, "workload_plus_drain_ns")
    metric(record, "operations_per_second")
    gaps = record.get("measurement_gaps", {}).get("latency_tail_percentiles")
    tail = record["phases"]["workload_plus_drain"].get("tail_latency", gaps)
    available_tail = (isinstance(tail, dict) and tail.get("available") is True
                      and len(tail) > 1 and all(required_number(value, name) for name, value
                                               in tail.items() if name != "available"))
    if not valid_unavailable(tail) and not available_tail:
        raise Invalid(f"tail latency schema is unsupported: {identity}")


def signature(record):
    labels = record["labels"]
    metadata = record["metadata"]
    source = metadata.get("tested_source", {})
    tools = metadata.get("tools", {})
    hardware = source.get("hardware")
    manifest = tools.get("manifest")
    required = (source.get("commit"), source.get("dirty"), source.get("kernel"),
                source.get("geometry"), source.get("backing_medium"), hardware, manifest,
                record.get("parameters"), record.get("cache_condition"),
                record.get("mount_options"))
    pinned = (("sqlite_version",), ("sqlite_source_id",), ("adaptation", "patch_sha256"),
              ("compile", "cc"), ("compile", "common_flags"), ("features",),
              ("artifacts_sha256",))
    if any(value is None for value in required) or not isinstance(manifest, dict):
        raise Invalid("comparability metadata is incomplete (hardware is required)")
    for path in pinned:
        value = manifest
        for field in path:
            value = value.get(field) if isinstance(value, dict) else None
        if value in (None, "", {}):
            raise Invalid(f"tool manifest lacks pinned {'/'.join(path)}")
    hashes = [manifest["adaptation"]["patch_sha256"],
              *manifest["artifacts_sha256"].values()]
    if any(not isinstance(value, str) or len(value) != 64
           or any(character not in "0123456789abcdef" for character in value) for value in hashes):
        raise Invalid("tool manifest artifact or patch SHA-256 is invalid")
    commit = source["commit"]
    if (not isinstance(source["dirty"], bool) or not isinstance(commit, str)
            or len(commit) != 40
            or any(character not in "0123456789abcdef" for character in commit)):
        raise Invalid("block-device commit metadata is invalid")
    settings = {"profile": labels["profile"],
                "journal_mode": "delete" if labels["profile"] == "delete-extra" else "wal",
                "synchronous": {"delete-extra": 3, "wal-full": 2, "wal-normal": 1}
                               [labels["profile"]]}
    return {
        "parameters": record["parameters"],
        "fs": labels["fs"], "case": labels["case"], "profile": labels["profile"],
        "cache_condition": record["cache_condition"],
        "effective_settings": settings,
        "mount_options": record["mount_options"],
        "tested_source": {key: source[key] for key in
                          ("kernel", "geometry", "backing_medium", "hardware")},
        "tool_manifest": manifest,
    }


def summary_statistics(values):
    ordered = sorted(values)
    return {"samples": ordered, "median": ordered[1],
            "range": {"minimum": ordered[0], "maximum": ordered[2]}}


def expected_summary(key, items):
    case, profile = key.split("--", 1)
    records = [item[1] for item in items]
    value = {"case": case, "profile": profile, "sample_count": 3,
             "expected_repetitions_complete": True,
             "workload_ns": summary_statistics([metric(item, "workload_ns") for item in records]),
             "workload_plus_drain_ns": summary_statistics(
                 [metric(item, "workload_plus_drain_ns") for item in records])}
    rates = [metric(item, "operations_per_second") for item in records]
    if any(rate is None for rate in rates) and not all(rate is None for rate in rates):
        raise Invalid(f"operation-rate availability differs within {key}")
    value["operations_per_second"] = ({"available": False} if rates[0] is None
                                      else summary_statistics(rates))
    tails = [item["phases"]["workload_plus_drain"].get("tail_latency") for item in records]
    if all(tail is None or valid_unavailable(tail) for tail in tails):
        value["tail_latency"] = {"available": False}
    else:
        names = set(tails[0]) - {"available"} if isinstance(tails[0], dict) else set()
        if any(tail is None or valid_unavailable(tail)
               or set(tail) - {"available"} != names for tail in tails):
            raise Invalid(f"tail-latency availability differs within {key}")
        value["tail_latency"] = {"available": True, **{
            name: summary_statistics([tail[name] for tail in tails]) for name in sorted(names)}}
    return value


def validate_run(root):
    root = root.resolve()
    manifest_path, summary_path = root / "manifest.json", root / "summary.json"
    manifest, summary = read_json(manifest_path), read_json(summary_path)
    actual = manifest.get("actual", {})
    planned = manifest.get("planned_selections")
    completed = manifest.get("completed_selections")
    if ((manifest.get("schema"), manifest.get("schema_version"), manifest.get("planned_full"),
         manifest.get("selected"), actual.get("attempted"), actual.get("completed"),
         actual.get("failed")) != ("block-storage-sqlite-suite-index", 1, RUNS, RUNS,
                                   RUNS, RUNS, 0)
            or planned != catalog() or completed != planned
            or manifest.get("failed_selections") != []):
        raise Invalid("manifest is not one complete, successful 87-run matrix")
    if ((summary.get("schema"), summary.get("schema_version"), summary.get("scope"),
         summary.get("status"), summary.get("selected"), summary.get("planned_full"),
         summary.get("expected_selections"), summary.get("manifest")) !=
            ("block-storage-sqlite-summary", 2, "ext4_matrix", "pass", RUNS, RUNS,
             planned, "manifest.json")):
        raise Invalid("summary does not describe the exact full matrix")
    samples = summary.get("samples")
    if (not isinstance(samples, list) or len(samples) != GROUPS
            or any(item.get("sample_count") != 3
                   or item.get("expected_repetitions_complete") is not True for item in samples)):
        raise Invalid("summary does not contain 29 complete three-repetition groups")
    records, artifacts = {}, []
    for identity in planned:
        if not isinstance(identity, str) or "/" in identity or identity in records:
            raise Invalid("unsafe or duplicate result identity")
        path = root / identity / "result.jsonl"
        record = read_json(path, MAX_RESULT, True)
        validate_result(record, identity)
        records[identity] = record
        artifacts.append((path, digest(path)))
    groups = {}
    for identity, record in records.items():
        key = f"{record['labels']['case']}--{record['labels']['profile']}"
        groups.setdefault(key, []).append((identity, record))
    summary_groups = {(item.get("case"), item.get("profile")) for item in samples}
    result_groups = {tuple(key.split("--", 1)) for key in groups}
    if len(groups) != GROUPS or summary_groups != result_groups:
        raise Invalid("summary groups do not match the 29 result groups")
    summary_by_group = {f"{item.get('case')}--{item.get('profile')}": item for item in samples}
    matrix_commits = set()
    for key, items in groups.items():
        repetitions = {item[1]["labels"]["repetition"] for item in items}
        signatures = [signature(item[1]) for item in items]
        sources = [item[1]["metadata"]["tested_source"] for item in items]
        commits = [(source.get("commit"), source.get("dirty")) for source in sources]
        matrix_commits.update(commits)
        if repetitions != {1, 2, 3} or any(value != signatures[0] for value in signatures[1:]) \
                or any(value != commits[0] for value in commits[1:]):
            raise Invalid(f"inconsistent repetitions in group {key}")
        if summary_by_group[key] != expected_summary(key, items):
            raise Invalid(f"summary statistics differ from results for {key}")
    if len(matrix_commits) != 1:
        raise Invalid("block-device commit or dirty state differs within the matrix")
    return root, manifest_path, summary_path, records, artifacts, groups


def select(run_directory, baseline_json):
    root, manifest, summary, records, artifacts, groups = validate_run(Path(run_directory))
    target = Path(baseline_json).resolve()
    anchor = target.parent
    relative = lambda path: os.path.relpath(path, anchor)
    value = {
        "schema": "block-storage-sqlite-baseline", "schema_version": 1,
        "scope": "ext4_matrix", "selected_explicitly": True,
        "run_directory": relative(root),
        "artifacts": {
            "manifest": {"path": relative(manifest), "sha256": digest(manifest)},
            "summary": {"path": relative(summary), "sha256": digest(summary)},
            "results": [{"identity": identity, "path": relative(path), "sha256": sha}
                        for identity, (path, sha) in zip(records, artifacts)],
        },
        "comparability_signatures": {key: signature(items[0][1]) for key, items in groups.items()},
    }
    if not anchor.is_dir():
        raise Invalid("baseline pointer parent directory does not exist")
    with target.open("x", encoding="utf-8") as output:
        json.dump(value, output, indent=2, sort_keys=True, allow_nan=False)
        output.write("\n")
    return {"status": "selected", "baseline": str(target), "baseline_pointer": value}


def load_baseline(path):
    path = path.resolve()
    pointer = read_json(path)
    if ((pointer.get("schema"), pointer.get("schema_version"), pointer.get("scope"),
         pointer.get("selected_explicitly")) !=
            ("block-storage-sqlite-baseline", 1, "ext4_matrix", True)):
        raise Invalid("invalid baseline pointer schema")
    expected = pointer.get("artifacts", {})
    listed = [expected.get("manifest"), expected.get("summary")] + expected.get("results", [])
    if len(listed) != RUNS + 2:
        raise Invalid("baseline artifact list is incomplete")
    for item in listed:
        artifact = (path.parent / item["path"]).resolve()
        if digest(artifact) != item.get("sha256"):
            raise Invalid(f"selected baseline artifact changed: {artifact}")
    root = (path.parent / pointer["run_directory"]).resolve()
    validated = validate_run(root)
    identities = [item["identity"] for item in expected["results"]]
    canonical = [validated[1], validated[2], *[item[0] for item in validated[4]]]
    pointer_paths = [(path.parent / item["path"]).resolve() for item in listed]
    if (pointer_paths != canonical or identities != list(validated[3])
            or pointer.get("comparability_signatures") != {
                key: signature(items[0][1]) for key, items in validated[5].items()}):
        raise Invalid("baseline pointer artifact, identity, or signature changed")
    return pointer, validated


def stats(values):
    ordered = sorted(values)
    return {"samples": ordered, "median": statistics.median(ordered),
            "minimum": ordered[0], "maximum": ordered[-1]}


def comparison_metric(old_records, new_records, name):
    old = [metric(record, name) for record in old_records]
    new = [metric(record, name) for record in new_records]
    if all(value is None for value in old + new):
        return {"available": False, "reason": "unavailable in both runs"}
    if any(value is None for value in old + new):
        raise Invalid(f"availability differs for {name}")
    baseline, candidate = stats(old), stats(new)
    change = (candidate["median"] - baseline["median"]) * 100.0 / baseline["median"]
    overlap = max(baseline["minimum"], candidate["minimum"]) <= min(
        baseline["maximum"], candidate["maximum"])
    return {"available": True, "baseline": baseline, "candidate": candidate,
            "median_percentage_change": change,
            "observed_ranges_overlap": overlap,
            "interpretation": "inconclusive" if overlap else "observed_ranges_do_not_overlap"}


def tail_comparison(old_records, new_records):
    def tails(records):
        values = []
        for record in records:
            workload = record["phases"]["workload_plus_drain"]
            values.append(workload["tail_latency"] if "tail_latency" in workload else
                          record["measurement_gaps"]["latency_tail_percentiles"])
        return values
    old, new = tails(old_records), tails(new_records)
    if all(valid_unavailable(value) for value in old + new):
        return {"available": False, "reason": "unavailable in both runs"}
    if any(valid_unavailable(value) for value in old + new):
        raise Invalid("tail latency availability differs")
    names = set(old[0]) - {"available"}
    if any(set(value) - {"available"} != names for value in old + new):
        raise Invalid("tail latency percentile names differ")
    return {"available": True, "percentiles": {
        name: comparison_metric([{ "phases": {"workload_plus_drain": {name: value[name]}}}
                                 for value in old],
                                [{"phases": {"workload_plus_drain": {name: value[name]}}}
                                 for value in new], name) for name in sorted(names)}}


def compare(baseline_json, candidate_directory, allow_commit_change=False):
    pointer, baseline = load_baseline(Path(baseline_json))
    candidate = validate_run(Path(candidate_directory))
    old_groups, new_groups = baseline[5], candidate[5]
    if set(old_groups) != set(new_groups):
        raise Invalid("candidate groups differ from baseline")
    changes, output = [], []
    for key in sorted(old_groups):
        old_items, new_items = old_groups[key], new_groups[key]
        old_signature, new_signature = signature(old_items[0][1]), signature(new_items[0][1])
        if old_signature != new_signature:
            raise Invalid(f"incompatible tool/build/hardware/cache/settings for {key}")
        old_source = old_items[0][1]["metadata"]["tested_source"]
        new_source = new_items[0][1]["metadata"]["tested_source"]
        old_commit = {name: old_source.get(name) for name in ("commit", "dirty")}
        new_commit = {name: new_source.get(name) for name in ("commit", "dirty")}
        if old_commit != new_commit:
            if not allow_commit_change:
                raise Invalid(f"block-device commit or dirty state differs for {key}")
            changes.append({"group": key, "baseline": old_commit, "candidate": new_commit})
        old_records = [item[1] for item in old_items]
        new_records = [item[1] for item in new_items]
        output.append({"group": key, "metrics": {
            name: comparison_metric(old_records, new_records, name) for name in
            ("workload_ns", "workload_plus_drain_ns", "operations_per_second")
        }, "tail_latency": tail_comparison(old_records, new_records)})
    same = baseline[0] == candidate[0]
    return {"schema": "block-storage-sqlite-comparison", "schema_version": 1,
            "status": "compared", "baseline": str(Path(baseline_json).resolve()),
            "candidate_run_directory": str(candidate[0]),
            "self_comparison": same,
            "candidate_independent": not same,
            "note": "initial 0% self-comparison; not an independent candidate" if same else None,
            "intentional_commit_changes": changes, "thresholds_applied": False,
            "improvement_claim": False, "groups": output}


def main(argv=None):
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    choose = commands.add_parser("select")
    choose.add_argument("run_directory")
    choose.add_argument("baseline_json")
    check = commands.add_parser("compare")
    check.add_argument("baseline_json")
    check.add_argument("candidate_run_directory")
    check.add_argument("--allow-commit-change", action="store_true")
    try:
        args = parser.parse_args(argv)
        result = (select(args.run_directory, args.baseline_json) if args.command == "select" else
                  compare(args.baseline_json, args.candidate_run_directory,
                          args.allow_commit_change))
        print(json.dumps(result, sort_keys=True, allow_nan=False))
        return 0
    except (Invalid, OSError, KeyError, TypeError, AttributeError, IndexError) as error:
        print(json.dumps({"status": "error", "error": str(error)}))
        return 2


if __name__ == "__main__":
    sys.exit(main())
