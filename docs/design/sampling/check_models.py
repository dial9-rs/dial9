"""Run positive TLC checks and require the expected negative controls to fail."""

import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

CASES = [
    ("leader", "RosterSampling", None),
    ("lifecycle", "RosterSampling", None),
    ("boundary", "BoundaryCensus", None),
    ("terminal-registry", "TerminalRegistry", None),
    ("negative-split-objects", "RosterSampling", "FencedPublication"),
    ("negative-incomplete-list", "RosterSampling", "CompletePassOnly"),
    ("negative-reused-key", "RosterSampling", "IncarnationIsolation"),
    ("negative-late-enable", "RosterSampling", "NoLateEnable"),
    ("negative-terminal-trim", "RosterSampling", "TerminalTrim"),
    ("negative-off-by-one", "OffByOne", "DesiredCount"),
    ("negative-boundary", "BoundaryCensus", "GuardedCensus"),
    ("negative-empty-arm", "BoundaryCensus", "NonemptyArm"),
    ("negative-bucket-first", "TerminalRegistry", "TerminalClassification"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("jar", type=Path, help="Path to tla2tools.jar")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    directory = Path(__file__).parent
    results = []
    for case, module, invariant in CASES:
        with tempfile.TemporaryDirectory(prefix="dial9-roster-tlc-") as temporary:
            working = Path(temporary)
            shutil.copyfile(directory / f"{module}.tla", working / f"{module}.tla")
            shutil.copyfile(directory / f"{case}.cfg", working / f"{case}.cfg")
            completed = subprocess.run(
                ["java", "-Xmx768m", "-XX:+UseParallelGC", "-cp", str(args.jar.resolve()),
                 "tlc2.TLC", "-workers", "1", "-metadir", str(working / "states"),
                 "-config", f"{case}.cfg", f"{module}.tla"],
                cwd=working, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                timeout=120,
            )
        text = completed.stdout
        passed = (
            completed.returncode == 0 and "No error has been found." in text
            if invariant is None else
            completed.returncode != 0 and (
                f"Invariant {invariant} is violated." in text
                or f"Invariant {invariant} is violated by the initial state" in text
            )
        )
        counts = re.search(r"([\d,]+) states generated, ([\d,]+) distinct states found", text)
        result = {
            "case": case,
            "expected_violation": invariant,
            "passed": passed,
            "exit_code": completed.returncode,
            "states_generated": None if counts is None else int(counts[1].replace(",", "")),
            "distinct_states": None if counts is None else int(counts[2].replace(",", "")),
            "output": text,
        }
        results.append(result)
        print(f"{case}: {'PASS' if passed else 'FAIL'}")
        if not passed:
            print(text)
            raise SystemExit(1)
    output = args.output or directory / "model-results.json"
    output.write_text(json.dumps(results, indent=2) + "\n")
    print(f"All {len(results)} model checks matched their expected outcomes.")


if __name__ == "__main__":
    main()
