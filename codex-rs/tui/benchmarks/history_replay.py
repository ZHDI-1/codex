#!/usr/bin/env python3
"""Compare offline history rendering using an immutable copy of a saved rollout.

Build the Rust example with --release --features history-render-bench first. This runner only
passes a saved file to that renderer; it never invokes Codex resume or executes recorded commands.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rollout", type=Path)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=True)
    runs = []
    reference = None
    with tempfile.TemporaryDirectory(prefix="codex-history-benchmark-") as directory:
        snapshot = Path(directory) / "rollout.jsonl"
        shutil.copyfile(args.rollout, snapshot)
        with snapshot.open("rb") as contents:
            digest = hashlib.file_digest(contents, "sha256").hexdigest()
        worker_budget = max(1, len(os.sched_getaffinity(0)) - 1)
        cases = [("baseline", 1), ("parallel8", 8), ("parallel_nproc_minus_1", worker_budget)]
        for repeat in range(args.repeats):
            # Rotate order so the same variant does not always get the first run.
            modes = cases[repeat % len(cases):] + cases[:repeat % len(cases)]
            for mode, workers in modes:
                completed = subprocess.run(
                    [str(args.binary.resolve()), "baseline" if mode == "baseline" else "parallel", str(snapshot), str(workers)],
                    check=True, text=True, capture_output=True, timeout=600,
                )
                result = json.loads(completed.stdout)
                result["repeat"] = repeat
                result["mode"] = mode
                signature = [
                    (case["case"], case["width"], case["rows"], case["output_hash"])
                    for case in result["measurements"]
                ]
                if reference is None:
                    reference = (result["commands"], result["command_bytes"], signature)
                elif reference != (result["commands"], result["command_bytes"], signature):
                    raise RuntimeError("baseline and parallel styled output differ")
                runs.append(result)
                (args.output / f"{mode}-{repeat}.json").write_text(json.dumps(result, indent=2) + "\n")
                print(mode, repeat, {case["case"]: round(case["elapsed_ms"], 2)
                                     for case in result["measurements"]}, flush=True)

    measurements = []
    for case_name in ("open", "resize", "restore"):
        case = {"case": case_name}
        for mode in ("baseline", "parallel8", "parallel_nproc_minus_1"):
            samples = [measurement for run in runs if run["mode"] == mode
                       for measurement in run["measurements"] if measurement["case"] == case_name]
            case[mode + "_median_ms"] = statistics.median(s["elapsed_ms"] for s in samples)
            case[mode + "_max_heartbeat_gap_ms"] = max(s["max_heartbeat_gap_ms"] for s in samples)
            case[mode + "_median_cpu_ms"] = statistics.median(s["cpu_ms"] for s in samples)
            case[mode + "_peak_rss_mib"] = max(s["peak_rss_mib"] for s in samples)
        case["nproc_vs_8_speedup"] = case["parallel8_median_ms"] / case["parallel_nproc_minus_1_median_ms"]
        case["nproc_vs_baseline_speedup"] = case["baseline_median_ms"] / case["parallel_nproc_minus_1_median_ms"]
        measurements.append(case)
    summary = {
        "source": str(args.rollout.resolve()), "source_sha256": digest,
        "commands": reference[0], "command_bytes": reference[1],
        "patches": runs[0]["patches"], "messages": runs[0]["messages"], "cells": runs[0]["cells"],
        "repeats": args.repeats, "styled_output_matches": True,
        "worker_counts": {run["mode"]: run["workers"] for run in runs},
        "platform": platform.platform(), "cpu_affinity": len(os.sched_getaffinity(0)),
        "measurements": measurements,
        "scope": "offline production command, patch and message rendering; excludes terminal writes and history grouping; "
                 "heartbeat is a foreground proxy, not a measured interactive TUI frame",
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
