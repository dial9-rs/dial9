#!/usr/bin/env python3
"""Run the safety model and controls; pass an existing tla2tools.jar path."""
from pathlib import Path
import json
import re
import subprocess
import sys

jar = Path(sys.argv[1]).resolve()
assert jar.is_file(), jar
here = Path(__file__).resolve().parent
jobs = [
    ('safety', None),
    ('negative-duplicate-epoch', 'OnceOnly'),
    ('negative-stale-cas', 'CASFencing'),
    ('missed-round-witness', 'NoMissedRound'),
]
results = []
for name, violation in jobs:
    command = ['java', '-XX:+UseParallelGC', '-Xmx2g', '-cp', str(jar),
               'tlc2.TLC', '-workers', '2', '-config', name + '.cfg',
               'BernoulliControl.tla']
    completed = subprocess.run(command, cwd=here, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    log = completed.stdout
    (here / (name + '.log')).write_text(log)
    if violation:
        assert completed.returncode == 12, (name, completed.returncode, log)
        assert f'Invariant {violation} is violated.' in log, (name, log)
    else:
        assert completed.returncode == 0, (name, completed.returncode, log)
        assert 'Model checking completed. No error has been found.' in log
    counts = re.search(r'(\d+) states generated, (\d+) distinct states found', log)
    assert counts, (name, log)
    depth = re.search(r'depth of the complete state graph search is (\d+)', log)
    results.append(dict(config=name + '.cfg', exit_code=completed.returncode,
                        outcome='safe' if not violation else 'expected counterexample',
                        invariant=violation, states_generated=int(counts[1]),
                        distinct_states=int(counts[2]), depth=int(depth[1]),
                        command=command))
(here / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
print(json.dumps(results, indent=2))
