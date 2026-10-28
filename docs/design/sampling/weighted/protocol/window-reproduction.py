"""Exact timing predicates from Config::validate and Worker::census_window."""

import json


def config_valid(poll_ms, bucket_ms, skew_ms):
    return (
        100 <= poll_ms <= 86_400_000 // 3
        and bucket_ms <= 86_400_000
        and bucket_ms > 4 * skew_ms
        and bucket_ms >= 2 * poll_ms + 2 * skew_ms
    )


def safe_phase_count(bucket_ms, skew_ms, uncertainty_ms):
    guard = 2 * skew_ms + uncertainty_ms
    return sum(
        phase >= guard and phase + guard < bucket_ms
        for phase in range(bucket_ms)
    )


if __name__ == "__main__":
    cases = []
    for poll, bucket, skew, precision in [
        (100, 1_000, 0, 1_000),
        (1_000, 10_000, 2_000, 1_000),
        (60_000, 300_000, 5_000, 1_000),
    ]:
        cases.append(
            {
                "poll_ms": poll,
                "bucket_ms": bucket,
                "skew_ms": skew,
                "uncertainty_ms": precision,
                "config_valid": config_valid(poll, bucket, skew),
                "safe_phase_count": safe_phase_count(bucket, skew, precision),
            }
        )
    assert cases[0]["config_valid"] and cases[0]["safe_phase_count"] == 0
    assert cases[1]["config_valid"] and cases[1]["safe_phase_count"] == 0
    assert cases[2]["config_valid"] and cases[2]["safe_phase_count"] > 0
    print(json.dumps(cases, indent=2))
