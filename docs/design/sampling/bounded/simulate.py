#!/usr/bin/env python3
"""Independent cohort simulator; no Rust/network/refinement claim.

Each cohort represents equal cached control, fixed poll phase, lifetime,
recording state, expansion weight, and workload. Binomial/multinomial draws
avoid one Python object per process. The 20-point poll-phase grid is an explicit
15-second approximation of a fixed uniform phase, not independently jittered
five-minute buckets. Census/profiler instants and ranks are fresh independent
draws in each minute. All times below are seconds.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import dataclass, replace
import json
import math
from pathlib import Path

import numpy as np

TARGET, K, P = 15, 480, 64
WINDOW, POLL, CLOSE, VALID, TRIM_LEAD = 60, 300, 22, 600, 81
LIMIT, PHASES = TARGET + math.ceil(3 * math.sqrt(TARGET)), 20
SEED = 1093


@dataclass(frozen=True)
class Cohort:
    cache: int
    phase: int
    born: int
    dies: int
    recording: bool
    weight: float
    work: int
    supported: bool


@dataclass
class Policy:
    current_c: float
    next_c: float
    effective: int
    population: float | None
    source_end: int
    published: float
    trim_generation: int = 0

    def cutoff(self, minute: int) -> float:
        return self.next_c if minute >= self.effective else self.current_c

    def admission(self, now: float, fallback: str) -> float:
        if self.population is not None and now <= self.source_end + VALID:
            return min(1.0, TARGET / self.population)
        return 1.0 if fallback == "enabled" else 0.0


@dataclass
class Observation:
    estimate: float | None
    retained: int
    delivered: int
    usable: bool


def estimate(keys: list[tuple[np.ndarray, float]], budget: int) -> Observation:
    """Model valid globally sorted complete keys, with one bounded page."""
    if not keys:
        return Observation(None, 0, 0, True)
    ranks = np.concatenate([u for u, _ in keys])
    cutoffs = np.concatenate([np.full(len(u), c) for u, c in keys])
    delivered = len(ranks)
    # Backend returns <= budget+1 keys; simulation may know all eligible keys.
    if delivered > budget:
        boundary = np.partition(ranks, budget)[budget]
        keep = ranks < boundary
    else:
        boundary, keep = 1.0, np.ones(delivered, dtype=bool)
    return Observation(float(np.sum(1.0 / np.minimum(cutoffs[keep], boundary))),
                       int(keep.sum()), delivered, True)


def gate(value: float) -> float:
    # Recorded actual gate is also used in report weights; quantization keeps
    # the cohort representation finite. Rust uses an integer cutoff instead.
    return max(2.0 ** -64, float(f"{min(1.0, value):.4g}"))


def summarize(rows: list[list[dict]]) -> list[dict]:
    minutes = sorted({0, 1, 2, 4, 6, 9, 14, 19, 29, 59, len(rows[0]) - 1})
    fields = ["actual_population", "actual_profilers", "weighted_population",
              "weighted_work", "actual_work", "candidate_gets", "periodic_gets",
              "profiler_gets", "startup_gets", "report_put_attempts", "list_calls",
              "published_population", "next_cutoff", "total_get_attempts",
              "total_put_attempts", "source_actual_population", "unsupported_population"]
    output = []
    for t in minutes:
        if t >= len(rows[0]):
            continue
        sample = [run[t] for run in rows]
        entry = {"minute": t + 1}
        for field in fields:
            values = [r[field] for r in sample if r[field] is not None]
            entry[field + "_mean"] = float(np.mean(values)) if values else None
        ratios = [r["published_population"] / r["actual_population"]
                  for r in sample if r["published_population"] is not None
                  and r["actual_population"]]
        entry["population_ratio_p05_p95"] = (
            [float(v) for v in np.quantile(ratios, [.05, .95])] if ratios else None)
        weights = [r["weighted_population"] / r["actual_population"]
                   for r in sample if r["actual_population"]]
        entry["weighted_population_ratio_p05_p95"] = [
            float(v) for v in np.quantile(weights, [.05, .95])]
        entry["unknown_population_fraction"] = sum(
            r["published_population"] is None for r in sample) / len(sample)
        output.append(entry)
    return output


def run(case: str, ticks: int, rng: np.random.Generator, *, active_control: bool = True,
        trim_lead: int = TRIM_LEAD, trim_cooldown: int = 1) -> tuple[list[dict], dict]:
    cold = case.startswith("cold")
    shrinking = case.startswith("shrink")
    fallback = "enabled" if case == "cold_enabled" else "disabled"
    start_n = (1 if case == "shrink_one" else 1000) if shrinking else (
        10_000 if case == "minute" else 500_000 if case == "containers" else
        1000 if case in {"rolling", "growth"} else 100_000)
    prior_n = 1_000_000 if shrinking else start_n
    initial = Policy(1.0 if cold else gate(K / prior_n),
                     1.0 if cold else gate(K / prior_n), 0,
                     None if cold else float(prior_n), -60, -38)
    # Cache version0 is the local no-roster bootstrap, not a globally readable
    # warm policy. A failed startup GET uses c1 and disabled default admission.
    policies = [Policy(1.0, 1.0, 0, None, -60, -38), initial]
    groups: dict[Cohort, int] = {}
    trim = None
    leader = not (cold or shrinking)
    discovery = 0.0 if leader else None
    convergence = None
    previous = None
    rows = []
    reserved_sources = set()

    def add(n: int, born: int, dies: int, startup: bool, *, request_ok: bool = True,
            admission_population: float | None = None) -> int:
        policy = policies[-1]
        p = (min(1.0, TARGET / admission_population) if admission_population is not None
             else policy.admission(born, fallback)) if request_ok else (
                 1.0 if fallback == "enabled" else 0.0)
        # Two exogenous workloads exercise weighted aggregation, not just counts.
        for work, count in [(4, int(rng.binomial(n, .25))), (1, 0)]:
            if work == 4:
                busy = count
            else:
                count = n - busy
            counts = rng.multinomial(count, np.full(PHASES, 1 / PHASES))
            for phase, number in enumerate(counts):
                enabled = int(rng.binomial(number, p)) if p else 0
                for recording, size in [(True, enabled), (False, number - enabled)]:
                    if size:
                        cohort = Cohort(len(policies) - 1 if request_ok else 0,
                                        phase, born, dies, recording,
                                        1 / p if recording else 0.0, work, p > 0)
                        groups[cohort] = groups.get(cohort, 0) + size
        return n if startup else 0

    # Warm initialization is not charged as current startup traffic.
    if case != "minute":
        initial_startup = add(start_n, 0, ticks * WINDOW + 1, cold)
    else:
        initial_startup = 0
        add(start_n, -30, 30, False)

    for t in range(ticks):
        start, end = t * WINDOW, (t + 1) * WINDOW
        leader_before = leader
        groups = {g: n for g, n in groups.items() if g.dies > start}
        for g, n in groups.items():
            assert isinstance(n, (int, np.integer)) and n > 0
            assert 0 <= g.phase < PHASES and g.born < g.dies
            assert not g.recording or (g.supported and math.isfinite(g.weight) and g.weight > 0)
        io_ok = not (case in {"outage", "outage_churn"} and 10 <= t < 22)
        list_ok = io_ok and not (case == "list_outage" and 10 <= t < 22)
        loss = .3 if case == "delivery_loss" else 0.0
        startup_gets = initial_startup if t == 0 else 0
        if case == "minute":
            # The birth is after the22s publication point. Its startup GET sees
            # the usable prior-window observation, not an artificially stale
            # policy. The current lottery still uses the pre-effective cutoff.
            late_population = (previous[0].estimate if previous is not None
                               and previous[0].usable and list_ok and leader_before else None)
            startup_gets += add(10_000, start + 30, end + 30, True,
                               request_ok=io_ok, admission_population=late_population)
        elif case == "rolling" and t:
            startup_gets += add(1000, start, ticks * WINDOW + 1, True)
        elif case == "growth" and t == 5:
            startup_gets += add(99_000, start, ticks * WINDOW + 1, True)
        elif case == "outage_churn" and t:
            startup_gets += add(1000, start, ticks * WINDOW + 1, True,
                               request_ok=io_ok)

        # A trim executes once at the predeclared future boundary. Missed tokens
        # are not applied later. Later source windows may cause a new generation.
        if trim and trim["effective"] == t:
            changed = defaultdict(int)
            for g, n in groups.items():
                if (g.recording and g.born // WINDOW <= trim["source_window"]
                        and g.cache >= trim["version"]):
                    survived = int(rng.binomial(n, trim["q"]))
                    changed[replace(g, weight=g.weight / trim["q"])] += survived
                    changed[replace(g, recording=False, weight=0.0)] += n - survived
                else:
                    changed[g] += n
            groups = {g: n for g, n in changed.items() if n}

        # Make current-window draws before publishing anything from the previous
        # window. They remain frozen even if those GETs acquire new control.
        facts = []
        census_keys, profiler_keys = [], []
        candidate_gets = periodic_gets = profiler_gets = puts = 0
        actual = recorded = weighted = actual_work = weighted_work = unsupported = 0.0
        first_read = math.inf
        for g, n in groups.items():
            lo, hi = max(start, g.born), min(end, g.dies)
            if hi <= lo:
                continue
            span = (hi - lo) / WINDOW
            c = policies[g.cache].cutoff(t)
            assert 0 < c <= 1 and math.isfinite(c)
            candidates = int(rng.binomial(n, span * c))
            candidate_times = rng.uniform(lo, hi, candidates)
            assert candidates <= n
            assert not candidates or (float(candidate_times.min()) >= g.born
                                      and float(candidate_times.max()) < g.dies)
            profile_reports = int(rng.binomial(n, span)) if g.recording else 0
            profile_times = rng.uniform(lo, hi, profile_reports)
            assert not profile_reports or (g.recording and float(profile_times.min()) >= g.born
                                          and float(profile_times.max()) < g.dies)
            phase_time = (g.phase + .5) * POLL / PHASES
            # One fixed 300s phase; active60s slots contain all quiet300s slots.
            # This also preserves the max300s gap on a monotonic disable.
            active = active_control and g.recording
            period = WINDOW if active else POLL
            due = start + ((phase_time - start) % period)
            poll = lo <= due < hi
            if poll:
                if active:
                    profiler_gets += n
                else:
                    periodic_gets += n
                first_read = min(first_read, due)
            if candidates:
                first_read = min(first_read, float(candidate_times.min()))
            if profile_reports and not active_control:
                first_read = min(first_read, float(profile_times.min()))
            candidate_gets += candidates
            if not active_control:
                profiler_gets += profile_reports
            # Eligible report attempts continue when GET fails. Full outages use
            # the two-attempt cap; random unobserved loss biases completed counts.
            attempts = 1 if io_ok else 2
            puts += attempts * (candidates + profile_reports)
            delivered = int(rng.binomial(candidates, 1 - loss)) if io_ok else 0
            if delivered:
                census_keys.append((rng.uniform(0, c, delivered), c))
            if io_ok and profile_reports:
                profiler_keys.append((rng.uniform(0, 1, profile_reports), 1.0))
            actual += n * span
            actual_work += n * span * g.work
            if not g.supported:
                unsupported += n * span
            if g.recording:
                recorded += n * span
                weighted += n * span * g.weight
                weighted_work += n * span * g.weight * g.work
            facts.append((g, n, poll, due, candidate_times,
                          np.empty(0) if active_control else profile_times))

        # Any reader, including a quiet non-profiler, can acquire an expired lease.
        if leader_before and not io_ok:
            leader = False
        if not leader and io_ok and first_read < math.inf:
            leader = True
            if discovery is None:
                discovery = first_read  # Election latency abstracted; see scope.
        publish_time = (start + CLOSE if leader_before else max(start + CLOSE, first_read)) if leader else math.inf
        can_publish = leader and io_ok and previous is not None and publish_time < end
        new_version = len(policies) - 1
        list_calls = 0
        if can_publish:
            assert t - 1 not in reserved_sources
            reserved_sources.add(t - 1)
            list_calls = 2
            old = policies[-1]
            c_old = old.cutoff(t)
            census, profiles = previous
            if not list_ok:
                census = replace(census, estimate=None, usable=False)
            current_population, source_end = old.population, old.source_end
            next_c = c_old
            if census.usable:
                if census.estimate is not None:
                    current_population, source_end = census.estimate, start
                    nominal = gate(K / census.estimate)
                else:
                    nominal = 0.0
                next_c = gate(max(2 * c_old, nominal)) if census.retained < K / 2 else nominal
            # Population freshness belongs to its source, never to this CAS.
            if current_population is not None and publish_time > source_end + VALID:
                current_population = None
            new = Policy(c_old, next_c, t + 1, current_population, source_end,
                         publish_time, old.trim_generation)
            # Current default:75s propagation+6s pre-CAS reservation, then next
            # UTC boundary; source>=effective+1. Comparison:313/+6.
            cooldown = trim is None or t - 1 >= trim["effective"] + trim_cooldown
            if (profiles.usable and profiles.estimate is not None
                    and profiles.estimate > LIMIT and cooldown):
                activation = math.floor((publish_time + trim_lead) / WINDOW) + 1
                new.trim_generation += 1
                trim = {"effective": activation, "q": TARGET / profiles.estimate,
                        "published": publish_time, "source_window": t - 1,
                        "version": len(policies)}
            policies.append(new)
            new_version = len(policies) - 1

        # Refresh cache at actual read instants without changing the frozen draw.
        member_count_before_refresh = sum(groups.values())
        changed = defaultdict(int)
        for g, n, poll, due, ct, pt in facts:
            if not io_ok:
                changed[g] += n
                continue
            if can_publish and g.born >= publish_time and g.born < end:
                # A startup GET after publication learns the future schedule.
                changed[replace(g, cache=new_version)] += n
                continue
            if poll and due >= publish_time and can_publish:
                changed[replace(g, cache=new_version)] += n
                continue
            # Candidate/profiler draws are independent per process. Conditional
            # selected identities within equal cohorts are uniform, so a
            # hypergeometric union represents late control refresh.
            late_c = int((ct >= publish_time).sum()) if can_publish else 0
            late_p = int((pt >= publish_time).sum()) if can_publish else 0
            categories = np.array([n - len(ct), len(ct) - late_c, late_c])
            def allocate(size: int, available: np.ndarray) -> np.ndarray:
                first = int(rng.hypergeometric(int(available[0]),
                            int(available[1] + available[2]), size)) if size else 0
                second = int(rng.hypergeometric(int(available[1]),
                             int(available[2]), size - first)) if size - first else 0
                return np.array([first, second, size - first - second])
            p_late = allocate(late_p, categories)
            p_early = allocate(len(pt) - late_p, categories - p_late)
            late = late_c + late_p - int(p_late[2])
            none = int(categories[0] - p_late[0] - p_early[0])
            # Exact random cross-table union, conditional on equal-cohort counts.
            if poll:
                early = n - late
            else:
                early = n - none - late
            previous_version = new_version - 1 if can_publish else new_version
            if late:
                changed[replace(g, cache=max(g.cache, new_version))] += late
            if early:
                changed[replace(g, cache=max(g.cache, previous_version))] += early
            if n - late - early:
                changed[g] += n - late - early
        groups = {g: n for g, n in changed.items() if n}
        assert sum(groups.values()) == member_count_before_refresh
        previous = (estimate(census_keys, K), estimate(profiler_keys, P))
        policy = policies[-1]
        population = (policy.population if end <= policy.source_end + VALID else None)
        assert population is None or (population > 0 and end <= policy.source_end + VALID)
        source_actual = (rows[policy.source_end // WINDOW - 1]["actual_population"]
                         if population is not None and policy.source_end > 0
                         else float(prior_n) if population is not None else None)
        if (convergence is None and population is not None and actual
                and .9 <= population / actual <= 1.1 and shrinking):
            convergence = t + 1
        rows.append(dict(
            minute=t + 1, actual_population=actual, actual_profilers=recorded,
            weighted_population=weighted, actual_work=actual_work,
            weighted_work=weighted_work, candidate_gets=candidate_gets,
            periodic_gets=periodic_gets, profiler_gets=profiler_gets,
            startup_gets=startup_gets, report_put_attempts=puts,
            list_calls=list_calls, published_population=population,
            total_get_attempts=candidate_gets + periodic_gets + profiler_gets
                + startup_gets + int(can_publish),
            total_put_attempts=puts + 2 * int(can_publish),
            source_actual_population=source_actual, unsupported_population=unsupported,
            next_cutoff=policy.next_c, census_retained=previous[0].retained,
        ))
    return rows, {"discovery_seconds": discovery,
                  "first_within_10pct_minute": convergence,
                  "profiler_minutes": sum(r["actual_profilers"] for r in rows),
                  "excess_above_limit_profiler_minutes": sum(
                      max(0, r["actual_profilers"] - LIMIT) for r in rows)}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--trials", type=int, default=40)
    parser.add_argument("--comparison", action="store_true",
                        help="Run old313/+6 and current81/+1 cadence on comparable seed streams")
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("simulation-results.json"))
    args = parser.parse_args()
    rng = np.random.default_rng(SEED)
    result = {"seed": SEED, "trials_per_dynamic_case": args.trials,
              "parameters": {"target": TARGET, "K": K, "P": P, "window_s": WINDOW,
                             "poll_s": POLL, "close_margin_s": CLOSE,
                             "population_max_age_s": VALID, "trim_lead_s": TRIM_LEAD,
                             "trim_propagation_lead_s": 75,
                             "trim_publication_guard_s": 6,
                             "trim_source_cooldown_windows": 1,
                             "active_poll_s": 60,
                             "leader_cas_puts_per_collected_window": 2},
              "scope": "Independent cohort simulation, 20 fixed phase grid points, complete eligible report delivery except explicit outages/loss; ideal reads/CAS and no request latency, election contention, or clock/network/Rust refinement proof. Cost counts uncoalesced logical operations plus one leader GET/two leader CAS PUTs per collected window; report failures use max two identical PUT attempts.",
              "cases": {}}
    cases = [("cold_disabled", 30), ("cold_enabled", 30), ("warm", 60),
             ("rolling", 60), ("minute", 60), ("containers", 30),
             ("growth", 60), ("shrink_thousand", 60), ("shrink_one", 60),
             ("outage", 40), ("outage_churn", 40), ("list_outage", 40),
             ("delivery_loss", 60)]
    for name, ticks in cases:
        runs, events = [], []
        for _ in range(args.trials):
            rows, event = run(name, ticks, rng)
            assert max(row["list_calls"] for row in rows) <= 2
            runs.append(rows)
            events.append(event)
        discovery = [e["discovery_seconds"] for e in events if e["discovery_seconds"] is not None]
        converged = [e["first_within_10pct_minute"] for e in events
                     if e["first_within_10pct_minute"] is not None]
        result["cases"][name] = {
            "snapshots": summarize(runs),
            "discovery_seconds_mean": float(np.mean(discovery)) if discovery else None,
            "discovery_seconds_max": float(np.max(discovery)) if discovery else None,
            "first_within_10pct_minute_p50_p95": [float(v) for v in np.quantile(converged, [.5, .95])] if converged else None,
            "ever_within_10pct_fraction": len(converged) / args.trials,
            "final_weighted_population_ratio_mean": float(np.mean([
                run[-1]["weighted_population"] / run[-1]["actual_population"] for run in runs])),
            "final_weighted_work_ratio_mean": float(np.mean([
                run[-1]["weighted_work"] / run[-1]["actual_work"] for run in runs])),
            "profiler_minutes_mean": float(np.mean([e["profiler_minutes"] for e in events])),
            "excess_above_limit_profiler_minutes_mean": float(np.mean([
                e["excess_above_limit_profiler_minutes"] for e in events])),
        }
        print(name, result["cases"][name]["first_within_10pct_minute_p50_p95"], flush=True)
    if args.comparison:
        result["trim_comparison"] = {}
        for name, ticks in [("cold_enabled", 30), ("growth", 60), ("rolling", 60)]:
            comparison = {}
            for label, active, lead, cooldown in [
                    ("conservative_313s_cooldown6", False, 313, 6),
                    ("active_75s_plus6s_guard_cooldown1", True, 81, 1)]:
                trial_rng = np.random.default_rng(SEED + 1000)
                runs, events = [], []
                for _ in range(args.trials):
                    rows, event = run(name, ticks, trial_rng, active_control=active,
                                      trim_lead=lead, trim_cooldown=cooldown)
                    runs.append(rows)
                    events.append(event)
                comparison[label] = {
                    "snapshots": summarize(runs),
                    "profiler_minutes_mean": float(np.mean([e["profiler_minutes"] for e in events])),
                    "excess_above_limit_profiler_minutes_mean": float(np.mean([
                        e["excess_above_limit_profiler_minutes"] for e in events])),
                    "final_weighted_population_ratio_mean": float(np.mean([
                        run[-1]["weighted_population"] / run[-1]["actual_population"] for run in runs])),
                }
                print("comparison", name, label,
                      comparison[label]["profiler_minutes_mean"], flush=True)
            result["trim_comparison"][name] = comparison
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(args.output)


if __name__ == "__main__":
    main()
