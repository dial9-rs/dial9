---
name: dial9-diagnose-long-poll
description: Root-cause WHY a poll was long, not just where it was. Covers what on-CPU vs off-CPU truly means, how to find the cause of an off-CPU stall even with no scheduling events captured (the per-OS-thread CPU census + correlation method), and how to distinguish an in-process holder from an off-box wait. Use when a long poll has been identified (by red_flag_scan.js or analyze.js) and you need to explain it and recommend a fix. Use when the user says "why is this poll long", "diagnose this long poll", "what is it blocked on", "is something holding a lock", or "off-CPU but no sched events".
---

# Diagnosing the root cause of a long poll

Finding long polls is easy (`red_flag_scan.js`, `analyze.js`). Explaining *why* a
poll was long — and recommending the right fix — is the hard part, and it is where
most analyses jump to a wrong conclusion. This skill is the method.

Read `dial9-runtime` first for the execution model (what a poll is, why a long one
hurts). Use `dial9-zoom-window` for the windowing this method relies on.

## "Long" is relative, not an absolute number

There is no universal millisecond threshold for "long." A poll is long when it sits
in the **far tail of this runtime's own poll-duration distribution**. In a service
whose p99 poll is 500µs, a 1ms poll is a 2×+ tail outlier worth chasing; in a batch
job whose p99 is 40ms, a 5ms poll is noise. A fixed threshold (e.g. "1ms = bad")
cries wolf on the batch job and stays silent on the latency-critical one until
polls are already catastrophic.

So **always calibrate against the distribution first**: read
`pollDurationByLoc` (or the `analyzeTraces` poll histograms) to learn this app's
p50/p99 per spawn location, and judge any candidate poll as a *multiple of p99*,
not an absolute value. The script does this automatically — its default threshold
is `3× p99` (with a 1ms floor to skip sub-ms noise), and it labels each poll with
how many × the runtime's p99 it is.

## Run it

```bash
node scripts/diagnose_long_poll.js <trace.bin|dir> [--task <id>] [--pctl-mult 3] [--min-ms <abs>] [--floor-ms 1] [--top 1]
```

Defaults to the single longest poll, with a threshold of `3× p99`. Flags:
- `--task <id>` — diagnose a specific poll regardless of threshold.
- `--top N` — diagnose the N longest.
- `--pctl-mult M` — set the threshold to `M× p99` (default 3).
- `--min-ms <abs>` — pin an **absolute** threshold in ms; overrides the percentile
  logic. Use only when you have a real latency budget (e.g. "anything over my 5ms
  SLA").
- `--floor-ms` — ignore polls below this even if they clear the percentile gate.

The script prints the runtime's poll distribution, the threshold it chose, then
automates the entire method below and prints a verdict. The sections that follow
explain what it is doing and how to interpret edge cases by hand.

> Run it per host directory (one trace file or one host's directory). 

## Step 1 — Is the poll a problem at all?

Two independent questions, often conflated:

- **Is it long?** (this skill: why did one `.poll()` take N ms)
- **Did it hurt anything?** A long poll only causes latency if work was waiting
  behind it. Check global queue depth and other workers' busy time in the window
  (`dial9-zoom-window`). `queue max = 0` and idle peers → the long poll delayed
  nothing; it's a curiosity, not an incident. `queue > 0` → it caused head-of-line
  blocking and is worth fixing. Note that just because in _this_ instance the poll
  did not cause harm does not mean it couldn't cause harm in other situations.


## Step 2 — On-CPU vs off-CPU: what it TRULY means

dial9 attaches CPU samples to each poll. `attachCpuSamples()` splits them:
`poll.cpuSamples` (source=0, **on-CPU**) and `poll.schedSamples` (source=1,
**off-CPU**, only present if schedule profiling was enabled).

The single most important fact about perf sampling:

> **A thread is sampled only while it is ON a CPU.** A thread that is parked,
> blocked, sleeping, or waiting on a syscall produces **NO samples** for the
> entire time it is off-CPU.

So the on-CPU sample count of the long poll itself tells you which world you're in:

| Observation | Meaning | Where the fix lives |
|---|---|---|
| **Has on-CPU samples** | The future ran synchronous work for that long — serialization, parsing, crypto, big collection ops, allocation. The stacks point straight at the hot code. | The spawn location's code: `spawn_blocking`, `yield_now`, or optimize the hot path. |
| **No on-CPU samples** (off-CPU) | The worker was descheduled by the kernel **inside the poll** — blocked on a syscall, a `std::sync::Mutex`, file/DNS I/O, or a network round-trip. The future did almost no CPU work; it *waited*. | Depends on *what* it waited on — Step 3. |

Crucial corollary: **the off-CPU poll's own (absent) samples can never tell you
why it was long.** There is nothing on-CPU to sample. The answer is always in
*what the rest of the machine was doing* during that window.

A subtlety on async vs sync waits: awaiting a `tokio::sync::Mutex`, a channel, or a
socket the normal way returns `Pending` and **ends the poll** — it does not produce
a long off-CPU poll. A long *off-CPU* poll means the worker thread was blocked
**synchronously inside the poll**: a blocking syscall, or a `std::sync::Mutex`/
parking-lot lock that parked the OS thread. That distinction narrows the cause
before you read a single stack.

## Step 3 — Why off-CPU, even with NO scheduling events

If schedule profiling was on, `poll.schedSamples` contains the off-CPU stacks —
read them directly; that's the blocking syscall/lock. **Usually it was not on**
(it needs `perf_event_paranoid <= 1` and explicit config), so you have an off-CPU
poll with zero stacks. You can still find the cause. The technique:

### Per-OS-thread CPU census of the poll's window

For a poll to be blocked *by something on this machine*, that something has to be
**running** — and running means on-CPU, which means perf sampled it. So census the
window by OS thread (tid):

1. Take all on-CPU samples whose timestamp falls within `[pollStart, pollEnd]`.
2. Group by `tid` (exclude the blocked poll's own worker tid).
3. Estimate each tid's on-CPU time: `samples × (1000 / samplingHz)` ms
   (≈ 10ms/sample at the 99 Hz default).

Then read the result:

- **One tid pinned on-CPU for ~the whole window** (coverage ≈ 100%, samples evenly
  spaced ~10ms apart) → that thread is the **likely blocker**. Read its stack: it
  is holding the lock / doing the work the blocked poll is waiting to proceed past.
  The blocker is frequently **not a tokio task** — it can be a `spawn_blocking`
  pool thread, a metrics/telemetry flusher, a background compaction thread, or the
  allocator. (That is exactly why a task/await scan misses it and the *thread*
  census finds it.)
- **No samples from anyone** in the window → the whole box was idle while this poll
  waited. There is **no in-process holder**. The poll is blocked **off-box**: a
  network or disk syscall, or a futex whose owner is also parked. The cause is the
  external dependency the spawn location awaits (an RPC, a DB/journal round-trip,
  the next inbound bytes). No amount of local-CPU hunting will find a holder
  because there isn't one.

> ### The sampling-math trap — read this twice
> The easiest mistake in the entire workflow: seeing "only 5 samples" on a thread
> and dismissing it as "barely running / idle." At 99 Hz the on-CPU sampling
> ceiling is ~1 sample / 10ms **per thread**. So 5 samples across a 50ms window is
> not "barely running" — it is **on-CPU the entire 50ms**. Evenly spaced
> timestamps (gaps ≈ 10ms) confirm continuous execution. Never judge "busy vs
> idle" from the *aggregate* inter-sample gap (it is large on an idle box simply
> because few threads are on-CPU); always reason **per tid**.
