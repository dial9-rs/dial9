"""Numerical quantiles for the stable, continuous bottom-k population model."""

import json
import math


def binomial_cdf_before_k(n, p, k):
    if p <= 0:
        return 1.0
    if p >= 1:
        return float(n < k)
    terms = [
        math.lgamma(n + 1)
        - math.lgamma(j + 1)
        - math.lgamma(n - j + 1)
        + j * math.log(p)
        + (n - j) * math.log1p(-p)
        for j in range(k)
    ]
    peak = max(terms)
    return min(1.0, math.exp(peak) * sum(math.exp(t - peak) for t in terms))


def quantile_u(n, k, probability):
    lo, hi = 0.0, 1.0
    for _ in range(64):
        mid = (lo + hi) / 2
        cdf = 1.0 - binomial_cdf_before_k(n, mid, k)
        if cdf < probability:
            lo = mid
        else:
            hi = mid
    return (lo + hi) / 2


def bounds(n, k):
    variance = n * (n - k + 1) / (k - 2)
    low_u = (k - 1) / (n * 1.1)
    high_u = (k - 1) / (n * 0.9)
    outside_ten = (
        1 - binomial_cdf_before_k(n, low_u, k)
        + binomial_cdf_before_k(n, high_u, k)
    )
    return {
        "n": n,
        "k": k,
        "relative_sd": math.sqrt(variance) / n,
        "central_95_relative_lower": (k - 1) / (n * quantile_u(n, k, 0.975)) - 1,
        "central_95_relative_upper": (k - 1) / (n * quantile_u(n, k, 0.025)) - 1,
        "probability_within_10_percent": 1 - outside_ten,
        "chebyshev_95_relative_bound": math.sqrt(variance / 0.05) / n,
        "rank_collision_union_bound": n * (n - 1) / (2 * 2**64),
    }


if __name__ == "__main__":
    print(json.dumps([bounds(n, 256) for n in [256, 300, 1000, 5000, 100_000]], indent=2))
