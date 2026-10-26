#!/usr/bin/env python3
"""Run saved allocator benchmark binaries serially in randomized paired rounds."""

import argparse
import itertools
import json
import os
from pathlib import Path
import random
import statistics
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=["micro", "workload"], required=True)
    parser.add_argument("--binary", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--config", action="append", choices=[
        "none", "sampling_only", "sampling_with_liveset"
    ])
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--cpu", default="2")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--criterion-dir", type=Path, default=Path("target/criterion"))
    parser.add_argument("--filter", default="")
    parser.add_argument("--warmup", type=float, default=0.5)
    parser.add_argument("--measurement", type=float, default=1)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--iterations", type=int, default=3)
    parser.add_argument("--events", type=int, default=300_000)
    parser.add_argument("--trace", type=Path)
    args = parser.parse_args()
    if args.rounds <= 0 or args.iterations <= 0 or args.events <= 0:
        parser.error("rounds, iterations, and events must be positive")
    binaries = {}
    for spec in args.binary:
        label, path = spec.split("=", 1)
        if label in binaries or not label.replace("-", "").replace("_", "").isalnum():
            parser.error("binary labels must be unique and alphanumeric")
        binaries[label] = str(Path(path).resolve(strict=True))
    configs = args.config or ["none"]
    args.output.mkdir(parents=True, exist_ok=True)
    rng = random.Random(args.seed)
    runs = []
    result_path = args.output / "measurements.json"
    for round_number in range(1, args.rounds + 1):
        cases = list(itertools.product(
            binaries, configs,
            ["micro"] if args.kind == "micro" else ["events", "trace"],
        ))
        rng.shuffle(cases)
        for label, config, workload in cases:
            baseline = f"{args.output.name}-{label}-{config}-{round_number}"
            stem = f"{baseline}-{workload}"
            cmd = ["taskset", "-c", args.cpu, binaries[label]]
            if args.kind == "micro":
                cmd += [
                    "--bench", "--noplot", "--sample-size", str(args.samples),
                    "--warm-up-time", str(args.warmup),
                    "--measurement-time", str(args.measurement),
                    "--nresamples", "10000", "--save-baseline", baseline,
                ]
                if args.filter:
                    cmd.append(args.filter)
            else:
                cmd += [
                    "--workload", workload, "--iterations", str(args.iterations),
                    "--events", str(args.events),
                ]
                if args.trace:
                    cmd += ["--trace", str(args.trace.resolve(strict=True))]
            env = dict(os.environ, BENCH_CONFIG=config)
            print(f"round {round_number}/{args.rounds}: {label} {config} {workload}",
                  flush=True)
            with (args.output / f"{stem}.log").open("w") as log:
                subprocess.run(cmd, env=env, stdout=log, stderr=subprocess.STDOUT,
                               check=True)
            run = dict(round=round_number, label=label, config=config, workload=workload)
            if args.kind == "micro":
                estimates = {}
                for path in args.criterion_dir.glob(f"**/{baseline}/estimates.json"):
                    key = str(path.parent.parent.relative_to(args.criterion_dir))
                    estimates[key] = json.loads(path.read_text())["slope"]
                if not estimates:
                    raise RuntimeError(f"No Criterion estimates for {baseline}")
                run["estimates"] = estimates
            else:
                # Profiler/decoder diagnostics, if any, are retained in the log.
                text = (args.output / f"{stem}.log").read_text().splitlines()
                values = [json.loads(line) for line in text if line.startswith("{")]
                if len(values) != 1:
                    raise RuntimeError(f"Expected one workload result in {stem}")
                run["result"] = values[0]
            runs.append(run)
            result_path.write_text(json.dumps(dict(
                settings=vars(args) | {"output": str(args.output),
                                      "criterion_dir": str(args.criterion_dir),
                                      "trace": str(args.trace) if args.trace else None},
                binaries=binaries, runs=runs,
            ), indent=2) + "\n")
    grouped = {}
    for run in runs:
        if args.kind == "micro":
            points = {key: value["point_estimate"]
                      for key, value in run["estimates"].items()}
        else:
            points = {run["workload"]: statistics.median(run["result"]["times_ns"])}
        for workload, point in points.items():
            grouped.setdefault((run["config"], workload, run["label"]), []).append(point)
    for (config, workload, label), points in sorted(grouped.items()):
        print(f"{config} {workload} {label}: median {statistics.median(points):.3f} ns")


if __name__ == "__main__":
    main()
