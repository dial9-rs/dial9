# Expected Outcomes — Long Poll Diagnosis

## Skill activation

- [ ] Agent loads `dial9-diagnose-long-poll` (and likely `dial9-zoom-window`)
- [ ] Agent runs `diagnose_long_poll.js` against `./trace.bin`, not only the
      red-flag scan
- [ ] Agent reads trace data before making claims

## Calibration

- [ ] Agent reports this runtime's p50/p99 poll duration
- [ ] Agent frames "long" as a multiple of p99, not an absolute number

## On-CPU vs off-CPU reasoning

- [ ] Agent classifies the target poll as on-CPU or off-CPU and cites the
      sample counts the script reported
- [ ] For an on-CPU poll, agent points to the hottest stack(s) and ties them
      to the spawn location
- [ ] For an off-CPU poll, agent uses the per-tid CPU census from the script
      (or `zoom.js`) to identify any on-CPU holder
- [ ] Agent reasons **per tid**, not from aggregate sample counts

## Sampling-resolution honesty

- [ ] When the per-tid census is sparse (few expected samples), agent reports
      UNKNOWN rather than claiming "blocked off-box" or "PINNED holder"
- [ ] If the agent uses `--hz`, it documents what it set it to and why

## Recommendations

- [ ] Recommendations cite the actual spawn location and stack from the trace
- [ ] Recommendations are specific to what the diagnosis showed (e.g.
      `spawn_blocking` for a hot synchronous loop, not a generic "use async I/O")
- [ ] Agent explains the causal chain: poll → cause → user-visible impact

## Anti-patterns to flag

- [ ] Does NOT make claims without running analysis first
- [ ] Does NOT hallucinate stacks or sample counts
- [ ] Does NOT call a thread "PINNED" or "the blocker" when the script
      reported the window as below the sampling noise floor
- [ ] Does NOT recommend changes unrelated to the diagnosed cause

## Failure recovery

- [ ] Agent recovers from tool errors within 1-2 retries
- [ ] If the trace has no qualifying long poll, agent says so clearly rather
      than fabricating one

## Tool usage analysis (for JSON output modes)

```bash
# All commands run by the agent
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Bash") | .input.command' "$LOG.raw"

# Skills loaded
jq -r 'select(.type == "assistant") | .message.content[]? | select(.type == "tool_use" and .name == "Skill") | .input.skill' "$LOG.raw"
```
