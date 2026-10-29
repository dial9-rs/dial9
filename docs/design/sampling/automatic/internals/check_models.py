"""Check the finite reporting model and require broken controls to fail."""
from pathlib import Path
import json
import re
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
JAR = Path(sys.argv[1]).resolve()
INVARIANTS = ["EpochCorrect", "NoPrematurePromotion", "QuietRecovery",
              "CASFenced", "OnceOnly", "NoLateEnable"]
CONTROLS = [
    (None, None),
    ("MixFractions", "EpochCorrect"),
    ("EarlyPromotion", "NoPrematurePromotion"),
    ("ReporterOnlyRecovery", "QuietRecovery"),
    ("UnfencedCAS", "CASFenced"),
    ("ReapplyTrim", "OnceOnly"),
    ("LateEnable", "NoLateEnable"),
    ("StalledRecovery", "QuietRecovery"),
]
results = []
for broken, expected in CONTROLS:
    name = broken or "safe"
    constants = "\n".join(f"{key} = {'TRUE' if key == broken else 'FALSE'}"
                           for key in [control for control, _ in CONTROLS[1:]])
    with tempfile.TemporaryDirectory(prefix="dial9-reporting-model-") as tmp:
        shutil.copyfile(HERE / "ReportingEpochs.tla", Path(tmp) / "ReportingEpochs.tla")
        cfg = Path(tmp) / "check.cfg"
        cfg.write_text(f"SPECIFICATION Spec\nCONSTANTS\n{constants}\n"
                       "CHECK_DEADLOCK FALSE\nINVARIANTS\n"
                       + "\n".join(INVARIANTS) + "\n")
        proc = subprocess.run(
            ["java", "-Xmx768m", "-cp", str(JAR), "tlc2.TLC", "-workers", "1",
             "-metadir", str(Path(tmp) / "states"), "-config", str(cfg),
             "ReportingEpochs.tla"],
            cwd=tmp, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            timeout=120,
        )
    log = proc.stdout
    (HERE / f"{name}.log").write_text(log)
    if expected:
        assert proc.returncode != 0 and f"Invariant {expected} is violated" in log, log
    else:
        assert proc.returncode == 0 and "No error has been found" in log, log
    counts = re.search(r"([\d,]+) states generated, ([\d,]+) distinct states found", log)
    results.append(dict(
        control=broken, expected_violation=expected, exit_code=proc.returncode,
        generated=int(counts[1].replace(",", "")),
        distinct=int(counts[2].replace(",", "")),
    ))
    print(name, "passed")
(HERE / "model-results.json").write_text(json.dumps(results, indent=2) + "\n")
