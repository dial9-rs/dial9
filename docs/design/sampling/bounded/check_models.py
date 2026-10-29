#!/usr/bin/env python3
"""Require safe finite TLC model and eight intended broken controls to fail."""
from pathlib import Path
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
JAR = Path(sys.argv[1]).resolve()
CONTROLS = {
    "RerollCurrent": "FrozenLottery",
    "UncappedList": "BoundedLists",
    "ShortPageExhaustion": "NoFalseExhaustion",
    "DoubleTrim": "OnceOnly",
    "UnfencedResult": "CASFenced",
    "UnfencedElection": "CASFenced",
    "ReservationFreshens": "ReservationIsNotPopulation",
    "LateEnable": "NoReenable",
}
INVARIANTS = ["FrozenLottery", "BoundedLists", "NoFalseExhaustion", "CASFenced",
              "ReservationIsNotPopulation", "OnceOnly", "NoReenable"]
result = {
    "scope": "Finite safety abstraction: two processes/owners, two source windows, CAS versions 0–4, one half-survival trim. No Rust refinement or network/scheduler liveness proof.",
    "jar_sha256": hashlib.sha256(JAR.read_bytes()).hexdigest(),
    "model_sha256": hashlib.sha256((HERE / "BoundedCensus.tla").read_bytes()).hexdigest(),
    "progress_model_sha256": hashlib.sha256((HERE / "QuietProgress.tla").read_bytes()).hexdigest(),
    "checks": [],
}
for broken in [None, *CONTROLS]:
    name = broken or "safe"
    constants = "\n".join(f"{key} = {'TRUE' if key == broken else 'FALSE'}"
                          for key in CONTROLS)
    with tempfile.TemporaryDirectory(prefix="dial9-bounded-model-") as tmp:
        shutil.copyfile(HERE / "BoundedCensus.tla", Path(tmp) / "BoundedCensus.tla")
        cfg = Path(tmp) / "check.cfg"
        cfg.write_text(f"SPECIFICATION Spec\nCONSTANTS\n{constants}\n"
                       "CHECK_DEADLOCK FALSE\nINVARIANTS\n" +
                       "\n".join(INVARIANTS) + "\n")
        proc = subprocess.run(
            ["java", "-Xmx1200m", "-cp", str(JAR), "tlc2.TLC", "-workers", "1",
             "-metadir", str(Path(tmp) / "states"), "-config", str(cfg),
             "BoundedCensus.tla"], cwd=tmp, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180)
    log = proc.stdout
    (HERE / f"{name}.log").write_text(log)
    expected = CONTROLS.get(broken)
    if expected:
        assert proc.returncode != 0 and f"Invariant {expected} is violated" in log, log
    else:
        assert proc.returncode == 0 and "No error has been found" in log, log
    count = re.search(r"([\d,]+) states generated, ([\d,]+) distinct states found", log)
    assert count, log
    result["checks"].append({
        "control": broken, "expected_violation": expected,
        "exit_code": proc.returncode, "generated": int(count[1].replace(",", "")),
        "distinct": int(count[2].replace(",", "")),
    })
    print(name, "passed", count[2], "distinct states", flush=True)
result["healthy_progress_checks"] = []
for broken in [None, "ReporterOnlyReads", "RedrawEachInterval"]:
    constants = "\n".join(f"{key} = {'TRUE' if key == broken else 'FALSE'}"
                          for key in ["ReporterOnlyReads", "RedrawEachInterval"])
    with tempfile.TemporaryDirectory(prefix="dial9-quiet-progress-") as tmp:
        shutil.copyfile(HERE / "QuietProgress.tla", Path(tmp) / "QuietProgress.tla")
        cfg = Path(tmp) / "check.cfg"
        cfg.write_text(f"SPECIFICATION Spec\nCONSTANTS\n{constants}\n"
                       "CHECK_DEADLOCK FALSE\nPROPERTIES\n"
                       "QuietEventuallyReads\nQuietEventuallyClaims\n")
        proc = subprocess.run(
            ["java", "-Xmx768m", "-cp", str(JAR), "tlc2.TLC", "-workers", "1",
             "-metadir", str(Path(tmp) / "states"), "-config", str(cfg),
             "QuietProgress.tla"], cwd=tmp, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=90)
    log = proc.stdout
    name = "progress-" + (broken or "safe")
    (HERE / f"{name}.log").write_text(log)
    if broken:
        assert (proc.returncode != 0 and "QuietEventuallyReads" in log
                and "QuietEventuallyClaims" in log and "were violated" in log), log
    else:
        assert proc.returncode == 0 and "No error has been found" in log, log
    count = re.search(r"([\d,]+) states generated, ([\d,]+) distinct states found", log)
    assert count, log
    result["healthy_progress_checks"].append({
        "control": broken, "exit_code": proc.returncode,
        "generated": int(count[1].replace(",", "")),
        "distinct": int(count[2].replace(",", "")),
        "scope": "One live quiet process, fair healthy GET/election completion and timely deadline scheduling; no failures/contenders/Rust refinement.",
    })
    print(name, "passed", count[2], "distinct states", flush=True)
(HERE / "model-results.json").write_text(json.dumps(result, indent=2) + "\n")
