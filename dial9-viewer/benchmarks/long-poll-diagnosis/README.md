# Long Poll Diagnosis Benchmark

Manual testing harness for the `dial9-diagnose-long-poll` and
`dial9-zoom-window` skills.

Where `trace-diagnosis` runs an open-ended "find what's wrong" prompt, this
benchmark pins the agent on a specific failure mode: it's already been told
there is a long poll, and it has to use the new skills to explain *why* the
poll was long — distinguishing on-CPU work from off-CPU waits, and (when off-
CPU) an in-process holder from an off-box wait.

## Quick start

```bash
# Generate target project, install skills, run agent, capture output
./run.sh

# Or with explicit target directory
./run.sh /path/to/target

# Regenerate from scratch
./run.sh --clean

# Run with Codex instead of Claude
./run.sh --agent codex
```

## What it does

Reuses `../trace-diagnosis/setup.sh` to produce the project, then issues a
diagnosis-focused prompt that requires the long-poll and zoom skills to answer
well. The output log is what to attach to a PR description.

## Evaluating results

After the run completes:

```
Evaluate /tmp/dial9-long-poll-benchmark-*.md against benchmarks/long-poll-diagnosis/EXPECTED.md
```

## What it tests

- Agent loads the `dial9-diagnose-long-poll` skill before answering
- Agent runs `diagnose_long_poll.js` (does not just eyeball `red_flag_scan.js`)
- Agent reasons about the on-CPU / off-CPU split correctly
- For off-CPU polls, agent uses the per-tid CPU census (zoom or diagnose) and
  does **not** confidently call "blocked off-box" when the poll is too short
  to support that claim
- Recommendation cites the actual spawn location and stack from the trace
