#!/usr/bin/env python3
"""Finite exact estimator checks and seeded weighting/timing experiments."""
from fractions import Fraction
from itertools import product
import json
import math
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
SPACE, K = 8, 2


def discrete_estimate(ranks, cutoffs, budget):
    reports = [(u, c) for u, c in zip(ranks, cutoffs) if u < c]
    ordered = sorted(u for u, _ in reports)
    theta = ordered[budget] if len(ordered) > budget else SPACE
    return sum((Fraction(SPACE, min(c, theta)) for u, c in reports if u < theta),
               Fraction())


def exact_mean(cutoffs, budget):
    return sum((discrete_estimate(u, cutoffs, budget)
                for u in product(range(SPACE), repeat=len(cutoffs))), Fraction()) / SPACE ** len(cutoffs)


heterogeneous = exact_mean([1, 4, 8], K)
# Strict discrete boundaries have a zero-floor exception: leave-one-out
# threshold zero gives no included contribution, rather than expectation one.
assert heterogeneous == 3 * Fraction(63, 64)
common = exact_mean([4, 4, 4], 1)
assert common == 3 * Fraction(7, 8) ** 2
wrong_extra_fraction = common / Fraction(4, SPACE)
assert wrong_extra_fraction == 2 * common
coupled = sum((discrete_estimate(u, [SPACE, max(1, u[0])], 1)
               for u in product(range(SPACE), repeat=2)), Fraction()) / SPACE ** 2
assert coupled != exact_mean([SPACE, SPACE], 1)

rng = np.random.default_rng(1093)
trials, population, target = 10_000, 100_000, 15
census = []
for _ in range(trials):
    c = 480 / population
    n = int(rng.binomial(population, c))
    ranks = rng.uniform(0, c, n)
    theta = np.partition(ranks, 480)[480] if n > 480 else 1.0
    census.append(float(np.sum(1 / np.minimum(c, theta) * (ranks < theta))))
census = np.asarray(census) / population
assert abs(float(census.mean()) - 1) < .005

# Admission probability actually used remains the expansion factor even when
# the census estimate is 30% low; independent thinning multiplies that factor.
p, q, busy = target / (.7 * population), .5, population // 4
busy_sampled = rng.binomial(busy, p * q, trials)
ordinary_sampled = rng.binomial(population - busy, p * q, trials)
weighted_population = (busy_sampled + ordinary_sampled) / (p * q)
weighted_work = (4 * busy_sampled + ordinary_sampled) / (p * q)
actual_work = 4 * busy + population - busy
expected_sd = math.sqrt(population * (1 - p * q) / (p * q))
assert abs(float(weighted_population.mean()) - population) < 5 * expected_sd / math.sqrt(trials)
assert abs(float(weighted_work.mean()) / actual_work - 1) < .025
wrong_missing_trim = (busy_sampled + ordinary_sampled) / p
assert abs(float(wrong_missing_trim.mean()) / population - .5) < .015

# With a fixed independent phase, healthy due opportunities are <=300s apart.
# Early unrelated reads leave those deadlines unchanged. The bound explicitly
# excludes read failures, clock steps, suspension, and election completion.
largest_gap = 0
largest_transition_gap = 0
for phase in range(300):
    first = phase if phase else 300
    slots = [first + 300 * i for i in range(3)]
    largest_gap = max(largest_gap, max(b - a for a, b in zip([0, *slots], slots)))
    opportunities = sorted({0, *slots, *(slot - 10 for slot in slots if slot >= 10)})
    largest_gap = max(largest_gap, max(b - a for a, b in zip(opportunities, opportunities[1:])))
    for disable in range(60):
        active_phase = phase % 60
        last_active = active_phase + 60 * math.floor((disable - active_phase) / 60)
        next_quiet = phase + 300 * (math.floor((disable - phase) / 300) + 1)
        largest_transition_gap = max(largest_transition_gap, next_quiet - last_active)
assert largest_gap <= 300
assert largest_transition_gap <= 300
# Independent active/quiet phases fail the same transition-gap requirement.
independent_phase_gap = max(
    quiet + 300 * (math.floor((disable - quiet) / 300) + 1)
    - (active + 60 * math.floor((disable - active) / 60))
    for quiet, active, disable in product(range(300), range(60), range(60)))
assert independent_phase_gap > 300
# New independent phases each bucket can leave nearly 600s between reads.
rerandomized_phase_gap = (300 + 299) - 0
assert rerandomized_phase_gap > 300

# A constant local clock offset shifts the real interval sampled by its window
# label. Visibility margins cannot eliminate this mean-time alignment effect.
width = 60
clock_rows = []
for offset in [-5, 0, 5]:
    local_window_in_real_time = (-offset, width - offset)
    overlap = max(0, min(width, local_window_in_real_time[1]) -
                  max(0, local_window_in_real_time[0]))
    clock_rows.append({"clock_offset_s": offset, "true_utc_mean": 1.0,
                       "expected_labelled_window_contribution": overlap / width})

result = {
    "seed": 1093, "trials": trials,
    "exact_discrete_ranks": {
        "rank_space": SPACE, "heterogeneous_gate_mean": str(heterogeneous),
        "common_gate_mean": str(common),
        "wrong_extra_fraction_mean": str(wrong_extra_fraction),
        "current_rank_dependent_gate_mean": str(coupled),
        "true_current_rank_example_population": 2,
        "independent_gate_baseline_same_discrete_space": str(exact_mean([SPACE, SPACE], 1)),
        "note": "Continuous proof exact; discrete zero thresholds create a rare guard event. Rust rejects zero boundary as unusable. Small rank-space results deliberately expose that qualification.",
    },
    "settled_census": {"mean_ratio": float(census.mean()),
                       "relative_sd": float(census.std()),
                       "empirical_p025_p975": [float(v) for v in np.quantile(census, [.025, .975])]},
    "actual_probability_weighting": {
        "census_ratio_used_for_admission": .7, "startup_p": p, "thinning_q": q,
        "weighted_population_ratio_mean": float(weighted_population.mean()) / population,
        "weighted_work_ratio_mean": float(weighted_work.mean()) / actual_work,
        "wrong_missing_trim_factor_ratio_mean": float(wrong_missing_trim.mean()) / population,
        "expected_weighted_population_sd": expected_sd,
        "unobserved_30pct_trace_loss_expected_ratio": .7,
        "note": "Trace loss is separate from census report loss; unobserved trace loss has no encoded correction.",
    },
    "scheduled_reads": {"phases_checked": 300, "largest_healthy_opportunity_gap_s": largest_gap,
                        "nested_active_to_quiet_transitions_checked": 300 * 60,
                        "largest_transition_opportunity_gap_s": largest_transition_gap,
                        "negative_independent_active_quiet_phase_gap_s": independent_phase_gap,
                        "negative_independently_redrawn_phase_gap_s": rerandomized_phase_gap,
                        "scope": "Fixed-deadline opportunity check, not network/election liveness."},
    "skew_fixture": {"lifetime_real_utc_s": [0, 60], "window_label_s": [0, 60],
                     "cases": clock_rows,
                     "note": "Exact UTC-window mean proof assumes aligned clocks; bounded offsets shift per-process observation intervals."},
}
(HERE / "statistical-results.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result, indent=2))
