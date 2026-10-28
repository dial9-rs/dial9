#!/usr/bin/env python3
"""Independent checks of sequential expansion weights and asynchronous thinning."""

import json
import math
from pathlib import Path

import numpy as np


SEED = 0x50A11CE
TRIALS = 60_000
UNITS = 240
CHUNK = 1_000
FRACTION = 0.5
TARGET = 8
CAP = TARGET + math.ceil(3 * math.sqrt(TARGET))
RNG = np.random.default_rng(SEED)


def summarize(values, truth):
    values = np.concatenate(values)
    finite = values[np.isfinite(values)]
    sd = finite.std()
    return {
        "truth": float(truth),
        "trials": len(values),
        "available_trials": len(finite),
        "mean": float(finite.mean()),
        "sd": float(sd),
        "monte_carlo_standard_error_of_mean": float(sd / math.sqrt(len(finite))),
        "mean_error_in_monte_carlo_standard_errors": (
            float((finite.mean() - truth) / (sd / math.sqrt(len(finite))))
            if sd else 0
        ),
        "central_95_estimator_interval": [
            float(np.quantile(finite, 0.025)), float(np.quantile(finite, 0.975))
        ],
    }


def check_case(adaptive=False):
    # Three workload cohorts have deliberately unequal admission rates.
    workload = np.repeat([1.0, 5.0, 20.0], UNITS // 3)
    probabilities = np.repeat([0.4, 0.05, 0.02], UNITS // 3)
    expected_work = workload.sum()
    weighted = {name: [] for name in ("startup", "round1_lazy", "round1_retry",
                                      "round2_skipped", "alive_slice")}
    expanded_units, ratio_work, repeated_unchanged = [], [], True

    for offset in range(0, TRIALS, CHUNK):
        size = min(CHUNK, TRIALS - offset)
        census_coin = RNG.random((size, UNITS))
        tracked = census_coin < FRACTION
        observed = tracked.sum(axis=1)
        if adaptive:
            # R includes the unit's census inclusion. Independent admission
            # conditional on C lets this endogenous probability cancel exactly.
            safe_observed = np.maximum(observed, 1)
            cohort_factors = np.repeat([2.0, 0.5, 0.2], UNITS // 3)
            conditional_admission = np.minimum(
                1.0, TARGET * cohort_factors[None, :] / safe_observed[:, None]
            )
        else:
            conditional_admission = np.broadcast_to(
                probabilities[None, :] / FRACTION, (size, UNITS)
            )
        admission_coin = RNG.random((size, UNITS))
        selected = tracked & (admission_coin < conditional_admission)
        weight = np.where(
            selected, 1.0 / (FRACTION * conditional_admission), 0.0
        )
        weighted["startup"].append((weight * workload).sum(axis=1))

        def apply_round(q, delivery, seen, sealed_coin):
            nonlocal selected, weight
            # Delivery time does not depend on that round's as-yet-unrevealed coin.
            apply = selected & delivery & ~seen
            survive = sealed_coin < q[:, None]
            weight = np.where(apply, weight * survive / q[:, None], weight)
            selected = np.where(apply, survive, selected)
            return seen | apply

        count1 = selected.sum(axis=1)
        q1 = np.where(count1 > CAP, TARGET / np.maximum(count1, 1), 1.0)
        coin1 = RNG.random((size, UNITS))
        timing1 = RNG.random((size, UNITS))
        delivery_rate1 = np.repeat([0.95, 0.5, 0.1], UNITS // 3)
        seen1 = apply_round(q1, timing1 < delivery_rate1, np.zeros_like(selected), coin1)
        weighted["round1_lazy"].append((weight * workload).sum(axis=1))

        # The same revision can be read repeatedly. Once seen, it must not
        # multiply its weight or apply its coin again.
        old_weights = weight.copy()
        seen1 = apply_round(q1, timing1 < delivery_rate1, seen1, coin1)
        repeated_unchanged &= np.array_equal(old_weights, weight)
        weighted["round1_retry"].append((weight * workload).sum(axis=1))

        count2 = selected.sum(axis=1)
        # Ghosts alter q but cannot alter its conditional expansion identity.
        observed_count2 = count2 + 12
        q2 = np.where(observed_count2 > CAP,
                      TARGET / np.maximum(observed_count2, 1), 1.0)
        coin2 = RNG.random((size, UNITS))
        delivery_rate2 = np.repeat([0.05, 0.3, 0.9], UNITS // 3)
        delivery2 = RNG.random((size, UNITS)) < delivery_rate2
        apply_round(q2, delivery2, np.zeros_like(selected), coin2)
        weighted["round2_skipped"].append((weight * workload).sum(axis=1))
        expanded_units.append(weight.sum(axis=1))

        # Retained population / sampled count is a post-stratification ratio,
        # not an inverse-probability weight under unequal cohort inclusion.
        profile_count = selected.sum(axis=1)
        sample_work = (selected * workload).sum(axis=1)
        ratio = np.divide(
            observed / FRACTION * sample_work,
            profile_count,
            out=np.full(size, np.nan),
            where=profile_count > 0,
        )
        ratio_work.append(ratio)

        # Lifetimes are exogenous. Exactly half the old cohort has ended at
        # this slice, without regard to any sampling or thinning coin.
        alive = np.ones(UNITS, dtype=bool)
        alive[: UNITS // 6] = False
        weighted["alive_slice"].append((weight * workload * alive).sum(axis=1))

    assert repeated_unchanged
    results = {
        "adaptive_admission": adaptive,
        "units": UNITS,
        "f": FRACTION,
        "target": TARGET,
        "overshoot_reconciliation_trigger": CAP,
        "repeated_round_is_idempotent": repeated_unchanged,
        "weighted_work": {
            name: summarize(values, expected_work if name != "alive_slice"
                            else expected_work - UNITS // 6)
            for name, values in weighted.items()
        },
        "expanded_unit_count_after_async_rounds": summarize(expanded_units, UNITS),
        "naive_population_over_profiler_ratio_work": summarize(ratio_work, expected_work),
    }
    for outcome in results["weighted_work"].values():
        assert abs(outcome["mean_error_in_monte_carlo_standard_errors"]) < 5.0
    units = results["expanded_unit_count_after_async_rounds"]
    assert abs(units["mean_error_in_monte_carlo_standard_errors"]) < 5.0
    return results


def negative_controls():
    # Reusing admission entropy for a trim that has q=1/2 makes every admitted
    # process with admission p=1/4 survive; dividing by q doubles the expectation.
    reused = RNG.random((TRIALS, 100))
    startup = reused < 0.25
    weighted_reused = ((startup & (reused < 0.5)) * 8).sum(axis=1)
    # Reapplying a previously passed round coin also doubles expectation.
    fresh = RNG.random((TRIALS, 100)) < 0.5
    weighted_duplicate = ((startup & fresh) * 16).sum(axis=1)
    return {
        "reused_admission_coin": summarize([weighted_reused], 100),
        "duplicate_round_without_dedup": summarize([weighted_duplicate], 100),
        "disabled_cohort": {
            "unobserved_work_units": 100,
            "selection_probability": 0,
            "inverse_weight": None,
            "finite_unbiased_reconstruction_possible": False,
        },
    }


def main():
    results = {
        "seed": SEED,
        "trials_per_scenario": TRIALS,
        "heterogeneous_admission": check_case(False),
        "census_endogenous_admission": check_case(True),
        "negative_controls": negative_controls(),
    }
    output = Path(__file__).with_name("weighting-results.json")
    output.write_text(json.dumps(results, indent=2) + "\n")
    for name in ("heterogeneous_admission", "census_endogenous_admission"):
        case = results[name]
        print(name)
        for stage, result in case["weighted_work"].items():
            print(stage, f'mean={result["mean"]:.3f}',
                  f'truth={result["truth"]:.0f}',
                  f'MCSE={result["monte_carlo_standard_error_of_mean"]:.3f}')
        ratio = case["naive_population_over_profiler_ratio_work"]
        print("naive ratio:", round(ratio["mean"], 3), "truth:", ratio["truth"])
    print("negative controls:", {
        name: round(value["mean"], 3) for name, value in results["negative_controls"].items()
        if "mean" in value
    })


if __name__ == "__main__":
    main()
