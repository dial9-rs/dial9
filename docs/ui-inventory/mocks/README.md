# dial9-viewer UX mocks

Clickable design proposals from the UX phase (see `../04-ux-findings.md` for the
findings each mock addresses). Screenshots under `assets/` are the real UI on
the demo trace; purple `NEW` tags mark added chrome; fabricated rows are
labeled illustrative.

## Run

```bash
cd docs/ui-inventory/mocks && python3 -m http.server 8090
# open http://localhost:8090/
```

Any static file server works; there is no build step.

## Contents

- `index.html` - gallery with finding references.
- `concept-1.html` - track B: unified timeline column (drag the cyan minimap
  box to pan the tracks; hold `C` to flash the current UI).
- `concept-2.html` - track B: triage-first (issues rail + inspector).
- `concept-3.html` - track B: conservative evolution (baseline comparator).
- `keyboard.html` - track A, interactive: press `/`, `n`/`p`, `g`, `z`, `f`,
  `?` to feel the proposed unified keyboard model.

Mock mechanics are simulations (screenshot crops pan instead of a real
viewport); the durable spec lives in the proposals document, not here.
