#!/usr/bin/env python3
"""Exact and seeded observation-delivery checks; not an async Rust model."""
from fractions import Fraction
import json
import math
from pathlib import Path
import random

HERE = Path(__file__).resolve().parent
WINDOW = Fraction(60)
SUBMISSION_END = WINDOW + 11
DRAIN = Fraction(9)
SEED, TRIALS = 1093, 200_000


def expected_contribution(birth, departure, duration, drain, successful=True):
    """Integrate an independent uniform opportunity over the labelled window.

    The report captures presence/recording at the opportunity. Delivery after
    departure is allowed only for an already captured job, within both its
    drain budget and the window submission deadline. Backend failure remains
    failure regardless of that budget.
    """
    if not successful:
        return Fraction()
    lower = max(Fraction(), birth)
    upper = min(WINDOW, departure, departure + drain - duration,
                SUBMISSION_END - duration)
    return max(Fraction(), upper - lower) / WINDOW


def delivered(opportunity, birth, departure, duration, drain, successful=True):
    captured = birth <= opportunity < departure and opportunity < WINDOW
    return (captured and successful
            and opportunity + duration <= departure + drain
            and opportunity + duration <= SUBMISSION_END)


def main():
    cases = [
        ("census_immediate_cancel", Fraction(5), Fraction(), True),
        ("profiler_immediate_cancel", Fraction(5, 2), Fraction(), True),
        ("census_normal_exit_drain", Fraction(5), DRAIN, True),
        ("profiler_normal_exit_drain", Fraction(5, 2), DRAIN, True),
        ("census_one_failed_put_then_success", Fraction(15, 2), DRAIN, True),
        # A crash is represented by no post-departure completion. Actual
        # uncertain remote writes may commit; that requires a backend model.
        ("census_crash_no_post_exit_delivery", Fraction(5), Fraction(), True),
        ("census_both_puts_fail", Fraction(15, 2), DRAIN, False),
    ]
    expected = {
        name: expected_contribution(Fraction(), WINDOW, duration, drain, ok)
        for name, duration, drain, ok in cases
    }
    assert expected["census_immediate_cancel"] == Fraction(11, 12)
    assert expected["profiler_immediate_cancel"] == Fraction(23, 24)
    assert expected["census_normal_exit_drain"] == 1
    assert expected["profiler_normal_exit_drain"] == 1
    assert expected["census_one_failed_put_then_success"] == 1
    assert expected["census_both_puts_fail"] == 0

    # A run alive for half the window has mean contribution 1/2. Normal exit
    # drains captured reports; it does not report every run once or create an
    # opportunity at birth, resume, or departure.
    assert expected_contribution(Fraction(30), WINDOW, Fraction(5), DRAIN) == Fraction(1, 2)
    assert not delivered(Fraction(20), Fraction(30), WINDOW, Fraction(5), DRAIN)
    assert not delivered(WINDOW, Fraction(), WINDOW, Fraction(5), DRAIN)
    assert delivered(Fraction(59), Fraction(), WINDOW, Fraction(5), DRAIN)

    # A shared stream makes the cancellation/drain difference reproducible.
    # Rank eligibility is independent of opportunity and RPC duration; its
    # actual cutoff supplies 1/c, which cannot repair censored delivery.
    cutoff = Fraction(1, 4)
    rng = random.Random(SEED)
    counts = {name: 0 for name, *_ in cases}
    for _ in range(TRIALS):
        opportunity = rng.random() * float(WINDOW)
        eligible = rng.random() < float(cutoff)
        if not eligible:
            continue
        for name, duration, drain, ok in cases:
            counts[name] += delivered(opportunity, 0, float(WINDOW),
                                      float(duration), float(drain), ok)

    rows = []
    for name, duration, drain, ok in cases:
        truth = expected[name]
        ratio = counts[name] / (TRIALS * float(cutoff))
        delivered_probability = float(cutoff * truth)
        standard_error = math.sqrt(
            delivered_probability * (1 - delivered_probability) / TRIALS
        ) / float(cutoff)
        assert abs(ratio - float(truth)) <= 6 * standard_error
        rows.append({
            "case": name,
            "successful_report_path_s": str(duration),
            "post_exit_drain_s": str(drain),
            "backend_path_succeeds": ok,
            "exact_mean_population_contribution": str(truth),
            "systematic_bias_fraction": str(1 - truth),
            "seeded_weighted_contribution": ratio,
            "standard_error": standard_error,
        })

    result = {
        "scope": "Independent opportunity/delivery reference, not a Rust scheduler or S3 refinement.",
        "seed": SEED,
        "trials": TRIALS,
        "window_and_lifetime_s": [0, int(WINDOW)],
        "recorded_cutoff": str(cutoff),
        "submission_deadline_s": str(SUBMISSION_END),
        "shared_normal_exit_drain_s": str(DRAIN),
        "cases": rows,
        "structural_checks": [
            "Birth after the frozen opportunity creates no catch-up report.",
            "Departure at the opportunity is outside the half-open lifetime.",
            "Drain completes already captured reports without new post-exit observations.",
            "Half-window lifetime contributes 1/2 even with a successful drain.",
        ],
        "qualifications": [
            "Healthy census path is GET 2.5s then PUT 2.5s; profiler path is PUT 2.5s.",
            "The retry path adds one failed 2.5s PUT and a successful 2.5s PUT.",
            "The 9s budget is shared by concurrent captured jobs, not renewed per job.",
            "No bound is claimed for crashes, suspension, failed delivery, backend hangs, or late server commits after client deadlines.",
            "Operation and closure margins do not turn systematic delivery censorship into Gaussian sampling error.",
            "Changing recording or presence after capture must not change the immutable report's captured state.",
        ],
    }
    (HERE / "report-lifetime-results.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
