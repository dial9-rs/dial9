#!/usr/bin/env python3
"""Seeded statistical checks and deterministic identifiability counterexamples."""

import itertools
import json
import math
import random
import statistics
from pathlib import Path


SEED = 0xD1A909
TRIALS = 60_000
RNG = random.Random(SEED)


def exact_census_moments(n, f):
    """Exhaust the finite inclusion design, without relying on the simulator law."""
    weighted = []
    for states in itertools.product((False, True), repeat=n):
        r = sum(states)
        probability = f**r * (1 - f) ** (n - r)
        weighted.append((r / f, probability))
    mean = sum(value * probability for value, probability in weighted)
    variance = sum((value - n) ** 2 * probability for value, probability in weighted)
    assert math.isclose(mean, n, abs_tol=1e-10)
    assert math.isclose(variance, n * (1 - f) / f, abs_tol=1e-10)
    return {"n": n, "f": f, "exact_mean": mean, "exact_variance": variance}


def census_calibration(n=100_000, f=1 / 128):
    errors = []
    coverage = 0
    gaussian_available = 0
    for _ in range(TRIALS):
        r = RNG.binomialvariate(n, f)
        estimate = r / f
        errors.append((estimate - n) / n)
        if r >= 30:
            gaussian_available += 1
            estimated_se = math.sqrt(r * (1 - f)) / f
            coverage += abs(estimate - n) <= 1.96 * estimated_se
    return {
        "n": n,
        "f": f,
        "trials": TRIALS,
        "empirical_relative_sd": statistics.pstdev(errors),
        "reference_relative_sd": math.sqrt((1 - f) / (n * f)),
        "normal_plugin_95_coverage": coverage / gaussian_available,
        "gaussian_available_trials": gaussian_available,
        "mean_relative_error": statistics.mean(errors),
    }


def correlated_rank_check(hosts=100, containers=20, f=1 / 8):
    """Reusing a physical host's rank for all containers clusters inclusion."""
    n = hosts * containers
    estimates = []
    normal_coverage = 0
    for _ in range(TRIALS):
        observed_hosts = RNG.binomialvariate(hosts, f)
        r = containers * observed_hosts
        estimate = r / f
        estimates.append(estimate)
        naive_se = math.sqrt(r * (1 - f)) / f
        normal_coverage += abs(estimate - n) <= 1.96 * naive_se
    return {
        "physical_hosts": hosts,
        "containers_per_host": containers,
        "true_process_population": n,
        "tracking_fraction": f,
        "mean_process_estimate": statistics.mean(estimates),
        "empirical_estimate_sd": statistics.pstdev(estimates),
        "independent_rank_predicted_sd": math.sqrt(n * (1 - f) / f),
        "cluster_rank_true_sd": math.sqrt(hosts * containers**2 * (1 - f) / f),
        "naive_normal_95_coverage": normal_coverage / TRIALS,
    }


def shared_rank_check(n=30, f=1 / 4, target=2):
    """Draw actual rank values, then condition on the observed tracked count."""
    bins = {}
    totals = []
    for _ in range(TRIALS):
        ranks = [RNG.random() for _ in range(n)]
        r = sum(rank < f for rank in ranks)
        cutoff = min(f, target * f / r) if r else 0
        active = sum(rank < cutoff for rank in ranks)
        bins.setdefault(r, []).append(active)
        totals.append(active)
    r = max(bins, key=lambda count: len(bins[count]))
    values = bins[r]
    observed_mean = statistics.mean(values)
    observed_variance = statistics.pvariance(values)
    p = min(1, target / r)
    return {
        "population": n,
        "tracking_fraction": f,
        "target": target,
        "conditioning_count": r,
        "conditioned_trials": len(values),
        "observed_conditional_mean": observed_mean,
        "correct_conditional_mean": r * p,
        "wrong_independent_conditional_mean": n * min(f, target * f / r),
        "observed_conditional_variance": observed_variance,
        "correct_conditional_variance": r * p * (1 - p),
        "unconditional_mean": statistics.mean(totals),
    }


def host_deduplication_counterexample(hosts=100, containers=20, f=1 / 128):
    host_inclusion_probability = 1 - (1 - f) ** containers
    naive_expected_host_estimate = hosts * host_inclusion_probability / f
    return {
        "physical_hosts": hosts,
        "containers_per_host": containers,
        "tracking_fraction": f,
        "each_container_has_an_independent_rank": True,
        "host_inclusion_probability": host_inclusion_probability,
        "expected_observed_distinct_physical_hosts": hosts * host_inclusion_probability,
        "naive_distinct_observed_hosts_divided_by_f": naive_expected_host_estimate,
        "correct_host_weight_if_multiplicity_known": 1 / host_inclusion_probability,
    }


def zero_count_reference(f=1 / 128, n=15):
    upper = math.floor(math.log(0.05) / math.log1p(-f))
    assert (1 - f) ** upper >= 0.05
    assert (1 - f) ** (upper + 1) < 0.05
    return {
        "tracking_fraction": f,
        "true_population": n,
        "probability_of_zero_tracked": (1 - f) ** n,
        "point_estimate_at_zero": 0,
        "one_sided_95_nonrejected_integer_upper_after_zero": upper,
        "normal_interval_available": False,
    }


def history_counterexamples():
    # Both fleets write all start memberships, clean them up on graceful exit,
    # and are enumerated at t=0 and t=60. The observations are indistinguishable.
    common_observations = [
        {"enumerated_at": 0, "members": 100},
        {"enumerated_at": 60, "members": 100},
    ]
    history_a = {"persistent": 100, "temporary": 0}
    history_b = {"persistent": 100, "temporary": 1000, "start": 30, "stop": 50}

    # A process lasting 60 seconds writes only its startup bucket. Following
    # abrupt exit, no DELETE succeeds. Current+previous membership buckets can
    # therefore retain ~5-10 times as many units as are currently alive.
    width = 300
    lifetime = 60
    starts_per_second = 10
    phases = []
    for time in (0, 11, 150, 288, 300, 450, 588):
        current_bucket = time // width
        earliest_visible_start = (current_bucket - 1) * width
        visible = starts_per_second * (time - earliest_visible_start)
        active = starts_per_second * lifetime
        phases.append(
            {
                "time": time,
                "true_active_processes": active,
                "visible_memberships": visible,
                "bias_percent": 100 * (visible - active) / active,
            }
        )
    return {
        "between_observations": {
            "same_retained_observations": common_observations,
            "fleet_a": history_a,
            "fleet_b": history_b,
            "fleet_a_active_at_40": 100,
            "fleet_b_active_at_40": 1100,
            "unconditional_identifiability": False,
        },
        "crash_ghosts": {
            "bucket_width_seconds": width,
            "lifetime_seconds": lifetime,
            "starts_per_second": starts_per_second,
            "normal_60s_poll_can_occur_after_exit": True,
            "snapshots": phases,
        },
    }


def main():
    results = {
        "seed": SEED,
        "exact_inclusion_design": exact_census_moments(8, 1 / 4),
        "normal_calibration": census_calibration(),
        "shared_census_admission_rank": shared_rank_check(),
        "correlated_rank_warning": correlated_rank_check(),
        "physical_host_estimand": host_deduplication_counterexample(),
        "zero_count_reference": zero_count_reference(),
        "historical_identifiability": history_counterexamples(),
    }
    output = Path(__file__).with_name("results.json")
    output.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
