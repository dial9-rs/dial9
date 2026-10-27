"""Seeded roster-protocol model and probability experiments; Python 3.12+.

This independent model is not the Rust implementation or an S3 capacity model.
It uses the same integer admission formula, shared census/selection rank, two
membership buckets, complete unordered pagination, and one CAS roster object.
"""

from __future__ import annotations

import argparse
from collections import Counter, deque
from dataclasses import dataclass, field, replace
import hashlib
import heapq
import json
import math
from pathlib import Path
import random
import statistics

SPACE = 1 << 64
SEED = 20261027


def tracking_shift(minimum_population: int, target: int) -> int:
    if not target:
        return 0
    shift = 0
    while shift < 58 and minimum_population // (1 << (shift + 1)) >= 32 * target:
        shift += 1
    return shift


def threshold(target: int, shift: int, tracked: int) -> int:
    limit = SPACE >> shift
    return min(limit, target * limit // tracked) if tracked else limit


def identity_rank(host: str) -> int:
    encoded = json.dumps(
        ["simulation", [], host], separators=(",", ":"), ensure_ascii=False
    ).encode()
    return int.from_bytes(hashlib.sha256(encoded).digest()[:8], "big")


@dataclass
class Process:
    identity: str
    incarnation: str
    rank: int
    tracked: bool
    enabled: bool
    state: str
    alive: bool = True
    partitioned: bool = False
    paused: bool = False
    clock_skew: float = 0
    heartbeat_offset: float = 0
    poll_delay: float = 60
    written_bucket: int | None = None
    written_state: str | None = None
    local_version: int | None = None
    history: list[str] = field(default_factory=list)


@dataclass
class Roster:
    version: int
    holder: str
    expires: float
    settled: bool = False
    cutoff_settled: bool = False
    arm: int | None = None
    keep: tuple[int, str] | None = None
    tracked: int = 0
    active: int = 0
    generated: float = 0
    census_time: float | None = None


@dataclass(frozen=True)
class Member:
    bucket: int
    rank: int
    state: str
    incarnation: str


class Fleet:
    """Discrete-event reference model with per-key linearizable CAS."""

    def __init__(self, target=15, minimum_population=0, fallback=None, seed=SEED,
                 boundary_guard=True, nonempty_arm=True, census_freshness=True,
                 poll_interval=60.0):
        self.target = target
        self.shift = tracking_shift(minimum_population, target)
        self.limit = SPACE >> self.shift
        self.cap = target + math.ceil(3 * math.sqrt(target))
        self.fallback = fallback
        self.random = random.Random(seed)
        self.time = 0.0
        self.poll_interval = poll_interval
        self.bucket_width = 300.0
        self.lease = 3 * poll_interval
        self.expiry_margin = 6.0
        self.census_boundary_margin = 11.0  # 2*5s skew + 1s store precision.
        self.boundary_guard = boundary_guard
        self.nonempty_arm = nonempty_arm
        self.census_freshness = census_freshness
        self.list_finish_delay_once = 0
        # Only this GET-to-CAS gap has latency; throughput is deliberately unmodeled.
        self.election_cas_delay = 0.001
        self.roster: Roster | None = None
        self.processes: dict[str, Process] = {}
        self.members: dict[Member, None] = {}
        self.requests = Counter()
        self.events = []
        self.serial = 0
        self.next_incarnation = 0
        self.readings: dict[str, deque] = {}
        self.lost_ack_once = False
        self.fail_list_once = False
        self.repeat_token_once = False
        self.failed_deletes = False
        self.accepted_publications = 0
        self.accepted_census_publications = 0
        self.stale_rejections = 0

    def queue(self, delay, operation, incarnation, argument=None):
        self.serial += 1
        heapq.heappush(
            self.events, (self.time + delay, self.serial, operation, incarnation, argument)
        )

    def warm(self, population):
        self.roster = Roster(
            version=1,
            holder="retired",
            expires=-1000,
            settled=True,
            cutoff_settled=True,
            arm=min(self.limit, self.target * SPACE // population),
            tracked=population >> self.shift,
            generated=-1000,
            census_time=-1000,
        )

    def start(self, identities, fallback=None, clock_skew=0):
        """Choice uses one GET for a tracked process and no membership writes."""
        fallback = self.fallback if fallback is None else fallback
        started = []
        for identity in identities:
            self.next_incarnation += 1
            incarnation = f"{self.next_incarnation:026x}"
            rank = identity_rank(identity)
            tracked = self.target > 0 and rank < self.limit
            arm = 0
            if tracked:
                self.requests["startup_get"] += 1
                if self.roster is not None and self.roster.cutoff_settled:
                    assert self.roster.arm is not None
                    arm = self.roster.arm
                elif fallback is not None:
                    arm = min(self.limit, self.target * SPACE // fallback)
            enabled = tracked and rank < arm
            process = Process(
                identity, incarnation, rank, tracked, enabled,
                "active" if enabled else "unarmed", clock_skew=clock_skew,
                heartbeat_offset=self.random.uniform(0, self.bucket_width),
                poll_delay=self.poll_interval * self.random.uniform(0.9, 1.1),
            )
            process.history.append(process.state)
            self.processes[incarnation] = process
            started.append(incarnation)
            if tracked:
                self.queue(0, "heartbeat", incarnation)
                self.queue(0, "poll", incarnation)
        return started

    def write_member(self, process):
        self.requests["member_put"] += 1
        if process.partitioned:
            self.requests["failed_member_put"] += 1
            return
        bucket = math.floor((self.time + process.clock_skew) / self.bucket_width)
        member = Member(bucket, process.rank, process.state, process.incarnation)
        self.members[member] = None
        process.written_bucket = bucket
        process.written_state = process.state

    def heartbeat(self, process):
        self.write_member(process)
        offset = process.heartbeat_offset
        bucket = math.floor((self.time + process.clock_skew) / self.bucket_width)
        next_time = (bucket + 1) * self.bucket_width + offset - process.clock_skew
        self.queue(max(0.001, next_time - self.time), "heartbeat", process.incarnation)

    def cleanup(self, incarnation, states=None):
        process = self.processes[incarnation]
        bucket = math.floor((self.time + process.clock_skew) / self.bucket_width)
        states = states or {process.state, process.written_state}
        for member in list(self.members):
            if (member.incarnation == incarnation and member.bucket in {bucket, bucket - 1}
                    and member.state in states):
                self.requests["member_delete"] += 1
                if self.failed_deletes or process.partitioned:
                    self.requests["failed_member_delete"] += 1
                else:
                    del self.members[member]

    def end(self, incarnation, graceful=True):
        process = self.processes[incarnation]
        process.alive = False
        if graceful:
            self.cleanup(incarnation, {"active", "carrier", "unarmed"})

    def poll(self, process):
        self.requests["background_get"] += 1
        if not process.partitioned:
            bucket = math.floor((self.time + process.clock_skew) / self.bucket_width)
            offset_due = (self.time + process.clock_skew) % self.bucket_width >= process.heartbeat_offset
            if (process.written_bucket is None or process.written_state != process.state
                    or (process.written_bucket != bucket and offset_due)):
                self.write_member(process)  # Retry a failed initial heartbeat at the next poll.
            roster = self.roster
            if (roster is not None and self.time <= roster.expires + self.expiry_margin
                    and roster.keep is not None and process.state == "active"
                    and (not self.census_freshness or (
                        roster.census_time is not None
                        and self.time - roster.census_time <= self.lease + 10
                        and roster.census_time - self.time <= 10
                    ))
                    and (process.rank, process.incarnation) >= roster.keep):
                process.state = "carrier"
                process.history.append("carrier")
                self.write_member(process)
                self.cleanup(process.incarnation, {"active"})
            if (roster is not None and roster.holder == process.incarnation
                    and self.time <= roster.expires + self.expiry_margin):
                if process.local_version != roster.version:
                    process.local_version = roster.version  # Reconcile a committed unknown write.
                    self.readings[process.incarnation] = deque(maxlen=4)
                    self.queue(0, "pass", process.incarnation)
            elif roster is None or self.time > roster.expires + self.expiry_margin:
                delay = min(2, self.poll_interval / 2) * process.rank / self.limit
                self.queue(delay, "claim", process.incarnation)
        else:
            self.requests["failed_background_get"] += 1
        self.queue(process.poll_delay, "poll", process.incarnation)

    def claim(self, process):
        self.requests["election_recheck_get"] += 1
        if process.partitioned:
            self.requests["failed_election_recheck_get"] += 1
            return
        roster = self.roster
        if roster is not None and self.time <= roster.expires + self.expiry_margin:
            return
        current = None if roster is None else roster.version
        self.queue(self.election_cas_delay, "claim_apply", process.incarnation, current)

    def claim_apply(self, process, expected):
        self.requests["election_cas"] += 1
        current = None if self.roster is None else self.roster.version
        if process.partitioned or current != expected:
            self.requests["failed_election_cas"] += 1
            return
        old = self.roster
        self.roster = (
            Roster(1, process.incarnation, self.time + self.lease)
            if old is None else replace(
                old, version=old.version + 1, holder=process.incarnation,
                expires=self.time + self.lease, settled=False, keep=None,
            )
        )
        if self.lost_ack_once:
            self.lost_ack_once = False
            process.local_version = None
            self.requests["unknown_write_ack"] += 1
            return
        process.local_version = self.roster.version
        self.readings[process.incarnation] = deque(maxlen=4)
        self.queue(0, "pass", process.incarnation)

    def list_members(self, process):
        bucket = math.floor(self.time / self.bucket_width)
        found = {}
        for prefix in (bucket, bucket - 1):
            objects = [m for m in self.members if m.bucket == prefix]
            self.random.shuffle(objects)  # No S3 Express ordering assumption.
            pages = max(1, math.ceil(len(objects) / 1000))
            for page in range(pages):
                self.requests["list_page"] += 1
                if process.partitioned or self.fail_list_once:
                    self.fail_list_once = False
                    self.requests["failed_list_page"] += 1
                    return None
                if self.repeat_token_once and pages > 1 and page > 0:
                    self.repeat_token_once = False
                    self.requests["invalid_continuation"] += 1
                    return None
                for member in objects[page * 1000:(page + 1) * 1000]:
                    prior = found.get(member.incarnation)
                    precedence = {"unarmed": 0, "active": 1, "carrier": 2}
                    if prior is None or (
                        precedence[member.state], member.bucket
                    ) > (precedence[prior.state], prior.bucket):
                        found[member.incarnation] = member
        return found

    def leader_pass(self, process):
        if process.local_version is None:
            return
        start_bucket = math.floor(self.time / self.bucket_width)
        phase = self.time % self.bucket_width
        if self.boundary_guard and not (
            self.census_boundary_margin <= phase
            and phase < self.bucket_width - self.census_boundary_margin
        ):
            # Lease-only renewal; no unusable full-prefix scan at the boundary.
            self.complete_pass(process, {}, start_bucket)
            return
        members = self.list_members(process)
        if members is None:
            process.local_version = None
            return
        if self.list_finish_delay_once:
            delay = self.list_finish_delay_once
            self.list_finish_delay_once = 0
            self.queue(delay, "pass_complete", process.incarnation, (members, start_bucket))
            return
        self.complete_pass(process, members, start_bucket)

    def complete_pass(self, process, members, start_bucket):
        roster = self.roster
        assert roster is not None
        readings = self.readings.setdefault(process.incarnation, deque(maxlen=4))
        phase = self.time % self.bucket_width
        safe = (
            start_bucket == math.floor(self.time / self.bucket_width)
            and self.census_boundary_margin <= phase
            and phase < self.bucket_width - self.census_boundary_margin
        )
        if not self.boundary_guard:
            safe = True
        if safe and (len(members) > 0 or not self.nonempty_arm):
            readings.append(len(members))
        else:
            readings.clear()
        converged = len(readings) == 4 and all(
            after <= before * 1.02 for before, after in zip(readings, list(readings)[1:])
        )
        active = sorted(
            (m.rank, m.incarnation) for m in members.values() if m.state == "active"
        )
        self.requests["publication_cas"] += 1
        if process.partitioned or process.local_version != roster.version:
            self.requests["failed_publication_cas"] += 1
            self.stale_rejections += int(process.local_version != roster.version)
            process.local_version = None
            return
        assert roster.holder == process.incarnation, "A stale owner published."
        arm = threshold(self.target, self.shift, len(members)) if converged else roster.arm
        self.roster = replace(
            roster, version=roster.version + 1, expires=self.time + self.lease,
            settled=converged, cutoff_settled=roster.cutoff_settled or converged,
            arm=arm, tracked=len(members) if safe else roster.tracked,
            active=len(active) if safe else roster.active,
            keep=active[self.cap] if safe and len(active) > self.cap else None,
            generated=self.time,
            census_time=self.time if safe else roster.census_time,
        )
        self.accepted_publications += 1
        self.accepted_census_publications += int(safe)
        if self.lost_ack_once:
            self.lost_ack_once = False
            self.requests["unknown_write_ack"] += 1
            process.local_version = None
            return
        process.local_version = self.roster.version
        cadence = self.poll_interval if self.roster.settled else min(1, self.poll_interval)
        if self.boundary_guard and not safe:
            until_safe = (
                self.census_boundary_margin - phase if phase < self.census_boundary_margin
                else self.bucket_width + self.census_boundary_margin - phase
                if phase >= self.bucket_width - self.census_boundary_margin
                else min(1, self.poll_interval)
            )
            cadence = min(self.poll_interval, until_safe)
        self.queue(cadence, "pass", process.incarnation)

    def advance(self, until):
        assert until >= self.time
        while self.events and self.events[0][0] <= until:
            self.time, _, operation, incarnation, argument = heapq.heappop(self.events)
            process = self.processes[incarnation]
            if not process.alive:
                continue
            if process.paused:
                self.queue(1, operation, incarnation, argument)
                continue
            if operation == "heartbeat":
                self.heartbeat(process)
            elif operation == "poll":
                self.poll(process)
            elif operation == "claim":
                self.claim(process)
            elif operation == "claim_apply":
                self.claim_apply(process, argument)
            elif operation == "pass":
                self.leader_pass(process)
            elif operation == "pass_complete":
                self.complete_pass(process, *argument)
            else:
                raise AssertionError(operation)
        self.time = until
        for process in self.processes.values():
            if not process.enabled:
                assert "active" not in process.history
            if "carrier" in process.history:
                first = process.history.index("carrier")
                assert all(state == "carrier" for state in process.history[first:])

    def active(self):
        return sum(p.alive and p.state == "active" for p in self.processes.values())

    def report(self):
        return {
            "time_seconds": self.time,
            "live_processes": sum(p.alive for p in self.processes.values()),
            "active": self.active(),
            "carrier": sum(p.alive and p.state == "carrier" for p in self.processes.values()),
            "settled": self.roster is not None and self.roster.settled,
            "tracked_in_roster": None if self.roster is None else self.roster.tracked,
            "requests": dict(sorted(self.requests.items())),
            "slot_probes": 0,
        }


def distribution(values, target):
    ordered = sorted(values)
    count = len(values)
    quantile = lambda p: ordered[min(count - 1, math.floor(p * count))]
    cap = target + math.ceil(3 * math.sqrt(target))
    return {
        "trials": count,
        "mean": statistics.mean(values),
        "standard_deviation": statistics.pstdev(values),
        "central_95_percent": [quantile(0.025), quantile(0.975)],
        "within_10_percent": sum(abs(x - target) <= target * 0.1 for x in values) / count,
        "within_25_percent": sum(abs(x - target) <= target * 0.25 for x in values) / count,
        "probability_above_reconciliation_cap": sum(x > cap for x in values) / count,
        "reconciliation_cap": cap,
        "histogram": dict(sorted(Counter(values).items())),
    }


def probability_experiments(trials):
    rng = random.Random(SEED)
    assert hasattr(rng, "binomialvariate"), "Python 3.12+ is required."
    output = []
    population = 100_000
    for target in (15, 500):
        for minimum in (0, population):
            shift = tracking_shift(minimum, target)
            fraction = 2.0 ** -shift
            same, fresh, estimates = [], [], []
            for _ in range(trials):
                tracked = rng.binomialvariate(population, fraction)
                cutoff = threshold(target, shift, tracked) / SPACE
                # Conditional on R, the R tracked ranks are uniform in [0,f).
                same.append(rng.binomialvariate(tracked, cutoff / fraction))
                fresh.append(rng.binomialvariate(population, cutoff))
                estimates.append(tracked / fraction)
            output.append({
                "target": target,
                "population": population,
                "tracking_fraction": fraction,
                "expected_tracked": population * fraction,
                "same_census_cohort": distribution(same, target),
                "independent_next_cohort": distribution(fresh, target),
                "population_relative_sd": statistics.pstdev(estimates) / population,
                "population_relative_bias": statistics.mean(estimates) / population - 1,
                "population_central_95_relative_error": [
                    sorted(estimates)[math.floor(trials * 0.025)] / population - 1,
                    sorted(estimates)[math.floor(trials * 0.975)] / population - 1,
                ],
                "population_within_5_percent": sum(
                    abs(x - population) <= population * 0.05 for x in estimates
                ) / trials,
                "population_within_10_percent": sum(
                    abs(x - population) <= population * 0.1 for x in estimates
                ) / trials,
            })
    return output


def exact_binomial_reference(population, target):
    probability = target / population
    term = math.exp(population * math.log1p(-probability))
    cumulative = term
    low = high = None
    cap = target + math.ceil(3 * math.sqrt(target))
    above_cap = None
    for count in range(1, cap + 1000):
        term *= (population - count + 1) * probability / (count * (1 - probability))
        cumulative += term
        if low is None and cumulative >= 0.025:
            low = count
        if high is None and cumulative >= 0.975:
            high = count
        if count == cap:
            above_cap = max(0, 1 - cumulative)
    assert low is not None and high is not None and above_cap is not None
    tail_count = cap + 1
    return {
        "population": population, "target": target,
        "model": "complete fresh all-tracked census; independent next cohort",
        "mean": target,
        "standard_deviation": math.sqrt(target * (1 - probability)),
        "central_95_percent": [low, high],
        "reconciliation_cap": cap,
        "probability_above_reconciliation_cap": above_cap,
        "chernoff_upper_bound_above_cap": math.exp(
            -tail_count * math.log(tail_count / target) + tail_count - target
        ),
    }


def behavior_experiments():
    output = {}
    zero = Fleet(target=0)
    zero.start(f"zero-{i}" for i in range(1000))
    zero.advance(100)
    assert zero.active() == 0 and not zero.requests
    output["zero_target_no_io"] = zero.report()
    untracked = Fleet(minimum_population=100_000)
    identity = next(f"untracked-{i}" for i in range(1000)
                    if identity_rank(f"untracked-{i}") >= untracked.limit)
    untracked.start([identity])
    untracked.advance(100)
    assert untracked.active() == 0 and not untracked.requests
    output["untracked_no_io"] = untracked.report()
    terminal = Fleet(fallback=1)
    terminal.advance(311)
    incarnation = terminal.start(["terminal-priority"])[0]
    process = terminal.processes[incarnation]
    assert process.state == "active"
    process.state = "carrier"
    process.history.append("carrier")
    # Clock regression/late completion: carrier old bucket, active newer bucket.
    terminal.members[Member(0, process.rank, "carrier", incarnation)] = None
    terminal.members[Member(1, process.rank, "active", incarnation)] = None
    observed = terminal.list_members(process)
    assert observed is not None and observed[incarnation].state == "carrier"
    output["terminal_state_wins_over_newer_active_bucket"] = {
        "visible_member_keys": len(terminal.members),
        "counted_incarnations": len(observed),
        "observed_state": observed[incarnation].state,
    }
    short = Fleet(fallback=2000, poll_interval=0.25)
    short.advance(20)
    short.start(f"short-poll-{i}" for i in range(100))
    short.advance(23)
    assert short.roster.cutoff_settled
    output["short_poll_bounded_election_and_warm_cadence"] = short.report()
    cold = Fleet()
    cold.start(f"cold-{i}" for i in range(2000))
    cold.advance(150)
    assert cold.roster.settled and cold.active() == 0
    output["cold_disabled_is_terminal"] = cold.report()
    bootstrap = Fleet(fallback=2000)
    bootstrap.start(f"cold-{i}" for i in range(2000))
    bootstrap.advance(150)
    assert bootstrap.roster.settled and bootstrap.active() > 0
    output["cold_assume_population"] = bootstrap.report()

    growth = Fleet()
    growth.warm(2000)
    growth.start(f"growth-{i}" for i in range(4000))
    output["growth_before_reconciliation"] = growth.report()
    growth.advance(150)
    assert growth.active() <= growth.cap
    output["growth_after_reconciliation"] = growth.report()
    shrink = Fleet()
    shrink.warm(2000)
    shrink.start(f"shrink-{i}" for i in range(1000))
    output["shrink_cannot_late_enable"] = shrink.report()
    before = shrink.active()
    shrink.advance(150)
    assert shrink.active() <= before
    output["shrink_after_reconciliation"] = shrink.report()

    rolling = Fleet(fallback=2000)
    old = rolling.start(f"sticky-{i}" for i in range(2000))
    rolling.advance(150)
    selected_before = {p.identity for p in rolling.processes.values() if p.enabled}
    for incarnation in old:
        rolling.end(incarnation)
    new = rolling.start(f"sticky-{i}" for i in range(2000))
    selected_after = {rolling.processes[p].identity for p in new if rolling.processes[p].enabled}
    assert selected_before == selected_after
    output["sticky_restart"] = {
        "same_startup_selection": True,
        "selected_before": len(selected_before),
        "selected_after": len(selected_after),
    }
    # A delayed old cleanup must leave all fresh incarnation keys intact.
    rolling.advance(151)
    fresh_members = {m for m in rolling.members if m.incarnation in new}
    for incarnation in old:
        rolling.cleanup(incarnation, {"active", "carrier", "unarmed"})
    assert fresh_members <= set(rolling.members)
    output["delayed_cleanup_incarnation_isolation"] = True

    overlap = Fleet(fallback=2000)
    overlap.start(f"overlap-{i}" for i in range(2000))
    overlap.advance(100)
    overlap.start(f"overlap-{i}" for i in range(2000))
    overlap.advance(200)
    assert overlap.roster.tracked == 4000  # Process incarnations, not logical hosts.
    assert overlap.active() <= overlap.cap
    output["overlapping_restart_counts_each_incarnation"] = overlap.report()

    paused = Fleet(fallback=2000)
    paused.start(f"pause-{i}" for i in range(2000))
    paused.advance(10)
    holder = paused.roster.holder
    paused.processes[holder].paused = True
    old_version = paused.processes[holder].local_version
    paused.advance(250)
    assert paused.roster.holder != holder
    successor = paused.roster.holder
    paused.processes[holder].paused = False
    paused.advance(252)
    assert paused.roster.holder == successor
    assert paused.stale_rejections > 0
    output["paused_leader_fenced_after_takeover"] = {
        "old_version": old_version, "new_version": paused.roster.version,
        "stale_publication_rejections": paused.stale_rejections,
    }

    unknown = Fleet(fallback=2000)
    unknown.lost_ack_once = True
    unknown.start(f"unknown-{i}" for i in range(2000))
    unknown.advance(150)
    assert unknown.roster.settled
    output["committed_claim_unknown_ack_reconciled"] = unknown.report()

    old_keep = Fleet()
    old_keep.warm(2000)
    old_keep.roster.keep = (0, "")
    old_keep.fail_list_once = True
    old_keep.start(f"old-keep-{i}" for i in range(2000))
    initial = old_keep.active()
    old_keep.advance(5)
    assert old_keep.roster.keep is None
    assert old_keep.active() == initial
    output["takeover_does_not_freshen_stale_keep"] = old_keep.report()

    delayed_keep = Fleet()
    delayed_keep.warm(2000)
    delayed_keep.advance(500)
    # A delayed same-version CAS refreshes server time with an old census body.
    delayed_keep.roster = replace(
        delayed_keep.roster, expires=680, generated=0, census_time=0, keep=(0, ""),
    )
    delayed_keep.start(f"delayed-keep-{i}" for i in range(2000))
    initially_enabled = delayed_keep.active()
    delayed_keep.advance(501)
    assert delayed_keep.active() == initially_enabled > 0
    output["fresh_lease_does_not_freshen_old_census_keep"] = delayed_keep.report()
    negative_keep = Fleet(census_freshness=False)
    negative_keep.warm(2000)
    negative_keep.advance(500)
    negative_keep.roster = replace(
        negative_keep.roster, expires=680, generated=0, census_time=0, keep=(0, ""),
    )
    negative_keep.start(f"delayed-keep-{i}" for i in range(2000))
    negative_keep.advance(501)
    assert negative_keep.active() == 0
    output["negative_fresh_lease_old_keep_trims_new_cohort"] = negative_keep.report()

    listing = Fleet(fallback=2500)
    listing.start(f"pages-{i}" for i in range(2500))
    listing.fail_list_once = True
    listing.advance(20)
    assert listing.requests["failed_list_page"] == 1
    assert listing.accepted_census_publications == 0
    listing.advance(80)
    assert listing.accepted_census_publications > 0
    output["failed_paginated_list_not_published"] = listing.report()
    listing.repeat_token_once = True
    listing.advance(130)
    assert listing.requests["invalid_continuation"] == 1
    output["repeated_pagination_token_rejected"] = listing.report()

    partition = Fleet()
    partition.warm(2000)
    ids = partition.start(f"partition-{i}" for i in range(8000))
    initial = partition.active()
    assert initial > partition.cap
    for incarnation in ids:
        partition.processes[incarnation].partitioned = True
    partition.advance(250)
    assert partition.active() == initial > partition.cap
    output["partition_permits_overshoot"] = partition.report()
    for incarnation in ids:
        partition.processes[incarnation].partitioned = False
    partition.advance(500)
    assert partition.active() <= partition.cap
    output["partition_recovery_reconciles"] = partition.report()

    stale = Fleet(fallback=2000)
    ids = stale.start(f"crash-{i}" for i in range(2000))
    stale.advance(100)
    for incarnation in ids[:1000]:
        stale.end(incarnation, graceful=False)
    stale.advance(599)
    assert any(m.incarnation in ids[:1000] for m in stale.members)
    output["crash_members_before_bucket_expiry"] = stale.report()
    stale.advance(700)
    assert stale.roster.tracked <= 1000
    output["crash_members_after_bucket_expiry"] = stale.report()

    clock = Fleet(fallback=2000)
    clock.advance(295)
    clock.start((f"skew-{i}" for i in range(2000)), clock_skew=5)
    clock.advance(299)
    assert clock.roster.tracked == 0 and not clock.roster.cutoff_settled
    output["guard_prevents_future_bucket_empty_arm"] = clock.report()
    clock.start(f"future-cutoff-{i}" for i in range(1000))
    assert clock.active() < 1000  # Startup still uses the explicit population fallback.
    output["guarded_following_burst_uses_fallback"] = clock.report()
    clock.advance(400)
    assert clock.roster.tracked == 3000 and clock.active() <= clock.cap
    output["future_bucket_becomes_visible"] = clock.report()

    negative_clock = Fleet(
        fallback=2000, boundary_guard=False, nonempty_arm=False,
    )
    negative_clock.advance(295)
    negative_clock.start((f"skew-{i}" for i in range(2000)), clock_skew=5)
    negative_clock.advance(299)
    assert negative_clock.roster.tracked == 0 and negative_clock.roster.cutoff_settled
    negative_clock.start(f"future-cutoff-{i}" for i in range(1000))
    assert negative_clock.active() >= 1000
    output["negative_unguarded_empty_plateau_admits_1000"] = negative_clock.report()

    crossing = Fleet(fallback=2000)
    crossing.advance(280)
    crossing.list_finish_delay_once = 31
    crossing.start(f"crossing-{i}" for i in range(2000))
    crossing.advance(311.5)
    assert not crossing.roster.cutoff_settled
    output["list_crossing_bucket_does_not_arm"] = crossing.report()

    cleanup = Fleet(fallback=2000)
    ids = cleanup.start(f"cleanup-{i}" for i in range(2000))
    cleanup.advance(150)
    cleanup.failed_deletes = True
    for incarnation in ids[:100]:
        cleanup.end(incarnation)
    output["failed_cleanup_leaves_stale_members"] = cleanup.report()
    return output


def request_experiments():
    output = []
    for minimum in (0, 100_000):
        fleet = Fleet(target=15, minimum_population=minimum, fallback=100_000)
        fleet.start(f"cost-{i}" for i in range(100_000))
        fleet.advance(20)
        report = fleet.report()
        report.update(
            tracking_fraction=2.0 ** -fleet.shift,
            synchronized_cold_election_cas_upper_bound=sum(
                p.tracked for p in fleet.processes.values()
            ),
            steady_state_expected_requests_per_minute=(
                sum(p.tracked for p in fleet.processes.values()) * (1 + 60 / 300)
                + 2 * max(1, math.ceil(fleet.roster.tracked / 1000)) + 1
            ),
        )
        output.append(report)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trials", type=int, default=50_000)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("simulation-results.json"))
    args = parser.parse_args()
    results = {
        "seed": SEED, "model": "independent discrete-event model; not Rust/S3 capacity",
        "probability_experiments": probability_experiments(args.trials),
        "exact_binomial_references": [
            exact_binomial_reference(100_000, target) for target in (15, 500)
        ],
        "core_behaviors": behavior_experiments(),
        "request_experiments": request_experiments(),
    }
    args.output.write_text(json.dumps(results, indent=2) + "\n")
    print(f"All behavior assertions passed; results written to {args.output}.")


if __name__ == "__main__":
    main()
