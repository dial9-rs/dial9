"""Seeded, coarse event simulator; this is separate from the executable Rust tests.

10s ticks conservatively discretize the 1s bootstrap census cadence. Complete
pagination, epoch-specific membership, heartbeat buckets, quiet polls, admission,
and trimming follow the implementation. It does not emulate individual S3 pages.
"""
from pathlib import Path
import json
import math
import numpy as np

HERE = Path(__file__).resolve().parent
TARGET, BUDGET, POLL, WIDTH, STEP = 15, 480, 60, 300, 10
CAP = TARGET + math.ceil(3 * math.sqrt(TARGET))
CASES = [
    "cold_start", "rolling_start", "one_start_per_minute", "one_minute_lifetimes",
    "lots_of_containers", "growth_and_shrink", "loss_of_all_reporters",
    "delayed_handoff_reads", "failed_writes_and_cleanup", "partition", "burst_growth",
    "loss_during_handoff",
]


def schedule(case):
    births = [(0, 10_000)]
    lifetime = 10**9
    if case == "rolling_start":
        births = [(m * 60, 100) for m in range(100)]
    elif case == "one_start_per_minute":
        births = [(m * 60, 1) for m in range(180)]
    elif case in ("one_minute_lifetimes", "failed_writes_and_cleanup"):
        births = [(m * 60, 1_000) for m in range(180)]
        lifetime = 60
    elif case == "lots_of_containers":
        births = [(0, 30_000)] + [(m * 60, 300) for m in range(1, 61)]
        lifetime = 7_200
    elif case in ("growth_and_shrink", "partition"):
        births += [(m * 60, 100) for m in range(1, 91)]
    elif case == "burst_growth":
        births += [(30 * 60, 90_000)]
    elif case == "loss_during_handoff":
        births += [(20 * 60, 90_000)]
    return births, lifetime


def fleet(case, seed):
    rng = np.random.default_rng(seed)
    births, lifetime = schedule(case)
    birth = np.repeat([t for t, _ in births], [n for _, n in births])
    death = birth + lifetime
    size = len(birth)
    live = np.zeros(size, bool)
    profile = np.zeros(size, bool)
    stopped = np.zeros(size, bool)
    terminal_until = np.full(size, -1, int)
    weight = np.zeros(size)
    next_poll = birth.astype(float).copy()
    # Quantize scheduled heartbeat wakeups to the simulation grid. A continuous
    # offset in the last 10s would otherwise be skipped at every bucket boundary.
    offset = rng.integers(0, WIDTH // STEP, size) * STEP
    slow = rng.random(size) < 0.4
    active, pending, epochs = 0, None, {}
    cached_active = np.zeros(size, int)
    cached_pending = np.full(size, -1, int)
    last_trim = np.zeros(size, int)
    last_write = {}
    records = {}
    states = {}
    histories = {}
    population, population_at = None, -10**9
    revision, trim_id, trim_until = 0, 0, 0
    trim_q = 1.0
    outside = 0
    counts = dict(GET=0, PUT=0, LIST=0, CAS=0, DELETE=0)
    samples = []
    changes = []
    leader_pass_due = 0
    killed = False

    def epoch(f):
        e = len(epochs)
        epochs[e] = dict(f=f, ranks=rng.random(size))
        records[e] = np.full(size, -10**9, int)
        states[e] = np.zeros(size, int)
        last_write[e] = np.full(size, -1, int)
        histories[e] = []
        return e

    epoch(1.0)

    def settled(e, n):
        histories[e].append(n)
        histories[e] = histories[e][-4:]
        return len(histories[e]) == 4 and all(
            b * 100 <= a * 102 for a, b in zip(histories[e], histories[e][1:])
        )

    def census(e, now):
        counts["LIST"] += 2
        visible = records[e] >= now // WIDTH - 1
        reporter = visible & (epochs[e]["ranks"] < epochs[e]["f"])
        return int(reporter.sum()), int((visible & (states[e] == 1)).sum())

    for now in range(0, 181 * 60, STEP):
        healthy = not (case == "partition" and 40 * 60 <= now < 70 * 60)
        old_live = live.copy()
        live = (birth <= now) & (death > now)
        lose_during_handoff = (
            case == "loss_during_handoff" and active != 0 and pending is not None
            and epochs[active]["f"] < 1 and epochs[pending[0]]["f"] < 1
        )
        if not killed and (
            (case in ("growth_and_shrink", "loss_of_all_reporters") and now >= 90 * 60)
            or lose_during_handoff
        ):
            candidates = np.flatnonzero(live)
            if case in ("loss_of_all_reporters", "loss_during_handoff"):
                quiet = ~(epochs[active]["ranks"] < epochs[active]["f"]) & ~profile
                if lose_during_handoff:
                    quiet &= ~(epochs[pending[0]]["ranks"] < epochs[pending[0]]["f"])
                candidates = np.flatnonzero(live & quiet)
            keep = rng.choice(candidates, min(100, len(candidates)), replace=False)
            kill = live.copy()
            kill[keep] = False
            death[kill] = now
            live[kill] = False
            killed = True

        departed = old_live & ~live
        for e in epochs:
            to_delete = departed & (records[e] >= now // WIDTH - 1)
            counts["DELETE"] += int(to_delete.sum())
            if healthy:
                deleted = to_delete.copy()
                if case == "failed_writes_and_cleanup":
                    deleted &= rng.random(size) >= 0.5
                records[e][deleted] = -10**9
        profile &= live
        new = live & ~old_live
        if new.any():
            # One bounded startup GET; a disabled fallback has zero support.
            counts["GET"] += int(new.sum())
            usable = healthy and population is not None and now - population_at <= 960
            p = min(1.0, TARGET / population) if usable else 0.0
            if not healthy and case == "partition":
                p = TARGET / 100.0  # Deliberately aggressive, explicit outage fallback.
            choices = rng.random(size) < p
            profile[new] = choices[new]
            if p:
                weight[new & profile] = 1 / p
            if healthy:
                cached_active[new] = active
                cached_pending[new] = -1 if pending is None else pending[0]
            next_poll[new] = now + rng.uniform(54, 66, int(new.sum()))

        polled = live & (next_poll <= now)
        counts["GET"] += int(polled.sum())
        if healthy:
            current = polled.copy()
            if case == "delayed_handoff_reads" and pending is not None:
                current &= ~slow | (now >= pending[1] + 600)
            cached_active[current] = active
            cached_pending[current] = -1 if pending is None else pending[0]
            candidates = current & profile & (last_trim < trim_id) & (now <= trim_until)
            if candidates.any():
                survival = rng.random(size) < trim_q
                weight[candidates & survival] /= trim_q
                excluded = candidates & ~survival
                profile[excluded] = False
                stopped[excluded] = True
                terminal_until[excluded] = now // WIDTH + 1
                last_trim[candidates] = trim_id
        next_poll[polled] = now + rng.uniform(54, 66, int(polled.sum()))
        last_trim[new] = trim_id  # New startups skip rounds sealed before they started.

        for e in list(epochs):
            cached = (cached_active == e) | (cached_pending == e)
            required = live & cached & (
                (epochs[e]["ranks"] < epochs[e]["f"]) | profile |
                (stopped & (now // WIDTH <= terminal_until))
            )
            fresh = last_write[e] < 0
            due = (last_write[e] != now // WIDTH) & (now % WIDTH >= offset)
            terminal = stopped & (states[e] == 1)
            write = required & (fresh | due | terminal)
            counts["PUT"] += int(write.sum())
            if healthy:
                success = write.copy()
                if case == "failed_writes_and_cleanup":
                    success &= rng.random(size) >= 0.2
                records[e][success] = now // WIDTH
                last_write[e][success] = now // WIDTH
                states[e][success] = np.where(stopped[success], 2, profile[success].astype(int))

        if healthy and now >= leader_pass_due and live.any():
            plateau = False
            complete = case != "failed_writes_and_cleanup" or rng.random() >= 0.1
            if complete:
                r, a = census(active, now)
                plateau = settled(active, r)
                if plateau and r:
                    population, population_at = r / epochs[active]["f"], now
                outside = min(3, outside + 1) if r < BUDGET / 2 or r > BUDGET * 2 else 0
                if r == 0 and pending is not None and epochs[pending[0]]["f"] < 1:
                    pending = None
                if pending is None:
                    f = 1.0 if r == 0 else min(1.0, BUDGET * epochs[active]["f"] / r)
                    if (r == 0 or (plateau and outside == 3)) and f != epochs[active]["f"]:
                        proposed = epoch(f)
                        pending = (proposed, now, now + 5 * POLL)
                        changes.append(dict(at=now, proposed=proposed, fraction=f))
                elif now >= pending[2]:
                    r2, _ = census(pending[0], now)
                    if settled(pending[0], r2) and r2:
                        active = pending[0]
                        pending = None
                        population, population_at = r2 / epochs[active]["f"], now
                        outside = 0
                        a = 0  # Promotion itself must not thin from mixed epochs.
                        changes.append(dict(at=now, promoted=active, fraction=epochs[active]["f"]))
                if a <= CAP:
                    trim_until = 0
                elif now >= trim_until:
                    trim_id += 1
                    trim_q = TARGET / a
                    trim_until = now + 3 * POLL
                counts["CAS"] += 1
                revision += 1
            else:
                counts["LIST"] += 1
            warming = pending is not None and now < pending[2]
            ready_pending = pending is not None and now >= pending[2]
            leader_pass_due = now + (POLL if warming or (plateau and not ready_pending) else STEP)

        if now % 60 == 0:
            samples.append(dict(
                second=now, actual_processes=int(live.sum()),
                projected_processes=population,
                observation_age_seconds=None if population is None else now - population_at,
                usable_for_startup=population is not None and now - population_at <= 960,
                actual_profilers=int(profile.sum()),
                fraction=epochs[active]["f"],
                pending_fraction=None if pending is None else epochs[pending[0]]["f"],
                weighted_profile_units=float(weight[profile].sum()),
            ))
    final = samples[-1]
    return dict(case=case, seed=seed, final=final, operations=counts,
                peak_actual_profilers=max(s["actual_profilers"] for s in samples),
                reporting_changes=changes, time_series=samples)


def adaptation():
    rng = np.random.default_rng(16028)
    runs, n, budget, old = 20_000, 1000, 480, 0.048
    ranks = rng.random((runs, n))
    previous = (ranks < old).sum(1)
    fraction = np.minimum(1, budget * old / np.maximum(previous, 1))
    same = (ranks < fraction[:, None]).sum(1) / fraction
    fresh = (rng.random((runs, n)) < fraction[:, None]).sum(1) / fraction
    def summarize(estimates):
        error = estimates / n - 1
        return dict(relative_bias=float(error.mean()), relative_sd=float(error.std()),
                    error_percentiles=dict(zip(["p025", "p50", "p975"],
                                               np.quantile(error, [.025, .5, .975]).tolist())))
    # Heterogeneous workload is deliberately tied to process position. Census
    # and startup hashes remain independent of these workload classes.
    work = np.where(np.arange(n) < 300, 10.0, 1.0)
    p = np.minimum(1.0, TARGET / fresh)
    admitted = rng.random((runs, n)) < p[:, None]
    weighted = (admitted * work).sum(1) / p
    survives = rng.random((runs, n)) < 0.5
    thinned = admitted & survives
    weighted_after_trim = (thinned * work).sum(1) / p / 0.5
    workload = float(work.sum())
    return dict(runs=runs, true_processes=n, old_fraction=old,
                same_rank_control=summarize(same),
                fresh_epoch=summarize(fresh),
                exact_unclipped_same_rank_relative_bias=1 / budget,
                fixed_fraction_gaussian_reference_sd=math.sqrt((1 - .48) / 480),
                heterogeneous_workload=dict(
                    actual_total_workload=workload,
                    weighted_mean=float(weighted.mean()),
                    weighted_mean_after_half_survival=float(weighted_after_trim.mean()),
                    broken_missing_trim_factor_mean=float((weighted_after_trim / 2).mean()),
                    weighted_relative_standard_error_of_mean=float(weighted.std() / workload / math.sqrt(runs)),
                    actual_profiler_mean=float(admitted.sum(1).mean()),
                    actual_profiler_count_percentiles=np.quantile(admitted.sum(1), [.025, .5, .975]).tolist(),
                ))


if __name__ == "__main__":
    result = dict(
        semantics="10s ticks; independent reporting-epoch ranks; default disabled cold fallback "
                  "except explicit assume_population(100) during the partition scenario; "
                  "membership visibility is two 5-minute buckets; not an instantaneous liveness proof",
        adaptive_error=adaptation(),
        scenarios=[fleet(case, 20261028 + i) for i, case in enumerate(CASES)],
    )
    (HERE / "simulation-results.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["adaptive_error"], indent=2))
    for scenario in result["scenarios"]:
        print(scenario["case"], scenario["final"],
              "peak profilers", scenario["peak_actual_profilers"])
