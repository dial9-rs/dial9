use super::{Config, rate_limited};
use crate::backend::{
    ListRequest, PutOutcome, PutRequest, SamplingStore, StoreError, StoreErrorKind, StoredObject,
    VersionToken, WriteCondition,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecisionReason {
    Roster,
    OutsideCutoff,
    TargetZero,
    MissingRoster,
    IncompatiblePolicy,
    UnsettledRoster,
    Timeout,
    InvalidConfiguration { message: String },
    StoreFailure { message: String },
}
impl DecisionReason {
    pub(crate) fn invalid_configuration(message: impl Into<String>) -> Self {
        Self::InvalidConfiguration {
            message: message.into(),
        }
    }
    pub(crate) fn store_failure(message: impl Into<String>) -> Self {
        Self::StoreFailure {
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PreparedSampling {
    // Background trimming must never alter the immutable application startup choice.
    pub(crate) enabled: bool,
    pub(crate) reason: DecisionReason,
    session: Option<SamplingSession>,
}
impl PreparedSampling {
    pub(crate) fn disabled(reason: DecisionReason, enabled: bool) -> Self {
        Self {
            enabled,
            reason,
            session: None,
        }
    }
    pub(crate) fn into_session(mut self) -> Option<SamplingSession> {
        self.session.take()
    }
}

/// The invocation owns this session; a status value never owns coordination.
#[derive(Debug)]
pub(crate) struct SamplingSession {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    recording: watch::Receiver<bool>,
    startup_probability: f64,
    weights: Option<crate::trace_weights::TraceWeights>,
}
impl SamplingSession {
    pub(crate) fn startup_probability(&self) -> f64 {
        self.startup_probability
    }
    pub(crate) fn weights(&self) -> Option<crate::trace_weights::TraceWeights> {
        self.weights.clone()
    }
    pub(crate) fn recording_changes(&self) -> watch::Receiver<bool> {
        self.recording.clone()
    }
    pub(crate) async fn shutdown(mut self, timeout: Duration) {
        self.stop.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "sampling maintenance failed"),
                Err(_) => {
                    task.abort();
                    tracing::debug!("sampling cleanup deadline expired");
                }
            }
        }
    }
}
impl Drop for SamplingSession {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}
pub(crate) struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
fn nonce() -> String {
    ulid::Ulid::new().to_string()
}
fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> StoreError {
    StoreError::new(StoreErrorKind::invalid_response(), message)
}
fn millis(time: SystemTime) -> Result<u64, StoreError> {
    u64::try_from(
        time.duration_since(UNIX_EPOCH)
            .map_err(invalid)?
            .as_millis(),
    )
    .map_err(invalid)
}
fn serialize(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("sampling record serializes"))
}

/// A null threshold means the entire 64-bit space; zero means nobody.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
struct Cutoff(Option<u64>);
impl Cutoff {
    fn value(&self) -> u128 {
        self.0.map_or(1u128 << 64, u128::from)
    }
    fn probability(&self) -> f64 {
        self.value() as f64 / (1u128 << 64) as f64
    }
    fn threshold(value: u128) -> Self {
        Self(if value >= (1u128 << 64) {
            None
        } else {
            Some(value as u64)
        })
    }
    fn includes(&self, rank: u64) -> bool {
        self.0.is_none_or(|threshold| rank < threshold)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Policy {
    schema: u32,
    pool: String,
    target: u32,
    max_overshoot: u32,
    poll_ms: u64,
    bucket_ms: u64,
    skew_ms: u64,
}
impl Policy {
    fn new(config: &Config) -> Self {
        Self {
            schema: 4,
            pool: config.pool_path(),
            target: config.target(),
            max_overshoot: config.overshoot(),
            poll_ms: config.poll_interval.as_millis() as u64,
            bucket_ms: config.bucket_width.as_millis() as u64,
            skew_ms: config.max_clock_skew.as_millis() as u64,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Counts {
    observed: u64,
    tracked: u64,
    active: u64,
    carriers: u64,
    unarmed: u64,
}
/// Membership from different fractions must never share a counting namespace.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ReportingEpoch {
    id: String,
    cutoff: Cutoff,
}
impl ReportingEpoch {
    fn bootstrap() -> Self {
        Self {
            id: "bootstrap".into(),
            cutoff: Cutoff(None),
        }
    }
    fn valid(&self) -> bool {
        ((self.id == "bootstrap" && self.cutoff.0.is_none())
            || self.id.parse::<ulid::Ulid>().is_ok())
            && self.cutoff.value() > 0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Handoff {
    reporting: ReportingEpoch,
    created_ms: u64,
    collect_after_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Population {
    reporting: ReportingEpoch,
    reporters: u64,
    computed_ms: u64,
}
impl Population {
    fn estimate(&self) -> f64 {
        self.reporters as f64 / self.reporting.cutoff.probability()
    }
    fn admission(&self, target: u32) -> Cutoff {
        Cutoff::threshold(
            u128::from(target) * self.reporting.cutoff.value() / u128::from(self.reporters),
        )
    }
    fn fresh(&self, now: u64, config: &Config) -> bool {
        let skew = config.max_clock_skew.as_millis() as u64 * 2;
        self.computed_ms <= now.saturating_add(skew)
            && now <= self.computed_ms.saturating_add(observation_age_ms(config))
    }
}
// Keep the preceding observation usable through a healthy reporting handoff,
// while rejecting arbitrarily stale startup probabilities during outages.
fn observation_age_ms(config: &Config) -> u64 {
    config.bucket_width.as_millis() as u64 * 2
        + config.poll_interval.as_millis() as u64 * 6
        + config.max_clock_skew.as_millis() as u64 * 2
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Trim {
    epoch: u64,
    salt: String,
    created_ms: u64,
    survival: Cutoff,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Roster {
    policy: Policy,
    holder: String,
    // Fresh on every mutation: body-based ETags must never repeat (ABA).
    revision: String,
    epoch: u64,
    settled: bool,
    reporting: ReportingEpoch,
    pending: Option<Handoff>,
    outside_budget: u8,
    population: Option<Population>,
    trim: Option<Trim>,
    counts: Counts,
    census_computed_at_ms: Option<u64>,
}
impl Roster {
    fn initial(config: &Config, invocation_id: &str) -> Self {
        Self {
            policy: Policy::new(config),
            holder: invocation_id.into(),
            revision: nonce(),
            epoch: 0,
            settled: false,
            reporting: ReportingEpoch::bootstrap(),
            pending: None,
            outside_budget: 0,
            population: None,
            trim: None,
            counts: Counts::default(),
            census_computed_at_ms: None,
        }
    }
    fn validate(&self, config: &Config) -> Result<(), StoreError> {
        if self.policy != Policy::new(config) {
            return Err(StoreError::new(
                StoreErrorKind::unsupported(),
                "incompatible sampling roster policy",
            ));
        }
        if !self.reporting.valid()
            || self.holder.parse::<ulid::Ulid>().is_err()
            || self.revision.parse::<ulid::Ulid>().is_err()
            || (self.trim.is_some() && self.census_computed_at_ms.is_none())
            || self
                .counts
                .active
                .checked_add(self.counts.carriers)
                .and_then(|n| n.checked_add(self.counts.unarmed))
                != Some(self.counts.observed)
            || self.counts.tracked > self.counts.observed
            || self.outside_budget > 3
            || self.pending.as_ref().is_some_and(|pending| {
                !pending.reporting.valid()
                    || pending.reporting.id == self.reporting.id
                    || pending.collect_after_ms
                        < pending.created_ms.saturating_add(
                            config.poll_interval.as_millis() as u64 * 5
                                + config.max_clock_skew.as_millis() as u64 * 2,
                        )
            })
            || self.population.as_ref().is_some_and(|population| {
                !population.reporting.valid() || population.reporters == 0
            })
            || self.trim.as_ref().is_some_and(|trim| {
                trim.epoch > self.epoch
                    || trim.salt.parse::<ulid::Ulid>().is_err()
                    || trim.survival.value() == 0
            })
        {
            return Err(invalid("invalid sampling roster fields"));
        }
        Ok(())
    }
    fn admission(&self, config: &Config, now: u64) -> Option<Cutoff> {
        self.population
            .as_ref()
            .filter(|population| population.fresh(now, config))
            .map(|population| population.admission(config.target()))
    }
    fn mutate(&mut self, invocation_id: &str) -> Result<(), StoreError> {
        self.holder = invocation_id.into();
        self.revision = nonce();
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid("roster epoch overflow"))?;
        Ok(())
    }
}
#[derive(Clone)]
struct Observed {
    roster: Roster,
    version: VersionToken,
    modified_at: SystemTime,
}
fn decode(object: StoredObject, config: &Config) -> Result<Observed, StoreError> {
    let roster: Roster = serde_json::from_slice(object.body()).map_err(invalid)?;
    roster.validate(config)?;
    Ok(Observed {
        roster,
        version: object.version().clone(),
        modified_at: object.modified_at(),
    })
}
fn expired(
    observed: &Observed,
    config: &Config,
    store: &dyn SamplingStore,
    now: SystemTime,
) -> bool {
    // Server LastModified determines lease age. Conservative precision/skew
    // margins only delay takeover.
    observed
        .modified_at
        .checked_add(config.poll_interval.saturating_mul(3))
        .and_then(|time| time.checked_add(config.max_clock_skew))
        .and_then(|time| time.checked_add(store.modification_time_uncertainty()))
        .is_some_and(|deadline| now > deadline)
}

pub(crate) async fn prepare_using(
    config: Config,
    store: Arc<dyn SamplingStore>,
    timeout: Duration,
    clock: Arc<dyn Clock>,
) -> PreparedSampling {
    if let Err(message) = config.validate() {
        return PreparedSampling::disabled(DecisionReason::invalid_configuration(message), false);
    }
    let guard_ms = config
        .max_clock_skew
        .saturating_mul(2)
        .saturating_add(store.modification_time_uncertainty())
        .as_nanos()
        .div_ceil(1_000_000);
    if config.bucket_width.as_millis() <= guard_ms.saturating_mul(2) {
        return PreparedSampling::disabled(
            DecisionReason::invalid_configuration(
                "bucket width must exceed twice the census guard (two clock skews plus backend timestamp uncertainty)",
            ),
            false,
        );
    }
    if config.target() == 0 {
        return PreparedSampling::disabled(DecisionReason::TargetZero, false);
    }
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return PreparedSampling::disabled(
            DecisionReason::invalid_configuration("startup timeout overflow"),
            false,
        );
    };
    let fallback = config.failure_decision();
    let mut startup_probability = config.failure_cutoff() as f64 / (1u128 << 64) as f64;
    let key = format!("{}roster.json", config.pool_prefix());
    let result = if timeout.is_zero() {
        None
    } else {
        Some(tokio::time::timeout_at(deadline, store.get(key)).await)
    };
    let (recording, reason, observed) = match result {
        None => (fallback, DecisionReason::Timeout, None),
        Some(Err(_)) => (fallback, DecisionReason::Timeout, None),
        Some(Ok(Err(error))) => (
            fallback,
            DecisionReason::store_failure(error.to_string()),
            None,
        ),
        Some(Ok(Ok(None))) => (fallback, DecisionReason::MissingRoster, None),
        Some(Ok(Ok(Some(object)))) => match decode(object, &config) {
            Err(error) if matches!(error.kind(), StoreErrorKind::Unsupported {}) => {
                (fallback, DecisionReason::IncompatiblePolicy, None)
            }
            Err(error) => (
                fallback,
                DecisionReason::store_failure(error.to_string()),
                None,
            ),
            Ok(observed) => match millis(clock.now()) {
                Err(error) => (
                    fallback,
                    DecisionReason::store_failure(error.to_string()),
                    Some(observed),
                ),
                Ok(now) => match observed.roster.admission(&config, now) {
                    None => (fallback, DecisionReason::UnsettledRoster, Some(observed)),
                    Some(cutoff) => {
                        startup_probability = cutoff.probability();
                        let active = config.selects(cutoff.value());
                        (
                            active,
                            if active {
                                DecisionReason::Roster
                            } else {
                                DecisionReason::OutsideCutoff
                            },
                            Some(observed),
                        )
                    }
                },
            },
        },
    };
    if reason == DecisionReason::IncompatiblePolicy {
        // Known mismatches cannot safely contribute to this policy's census.
        return PreparedSampling::disabled(reason, recording);
    }
    let mut prepared = PreparedSampling::disabled(reason, recording);
    let membership_allowed = matches!(
        prepared.reason,
        DecisionReason::MissingRoster
            | DecisionReason::Roster
            | DecisionReason::OutsideCutoff
            | DecisionReason::UnsettledRoster
    );
    tracing::debug!(
        population = ?observed.as_ref().and_then(|o| o.roster.population.as_ref()).map(Population::estimate),
        "sampling startup roster"
    );
    let weights = recording.then(|| {
        crate::trace_weights::TraceWeights::new(config.pool_path(), 1.0 / startup_probability)
    });
    let (signal, changes) = watch::channel(recording);
    let stop = CancellationToken::new();
    let mut worker = Worker::new(config, store, clock, observed, signal, membership_allowed);
    worker.weights = weights.clone();
    let cancellation = stop.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {},
            () = worker.run() => {},
        }
        if let Some(weights) = &worker.weights {
            weights.stop();
        }
        worker.signal.send_replace(false);
        if tokio::time::timeout(super::CLEANUP_TIMEOUT, worker.cleanup())
            .await
            .is_err()
        {
            tracing::debug!("sampling member cleanup timed out");
        }
    });
    prepared.session = Some(SamplingSession {
        stop,
        task: Some(task),
        startup_probability,
        weights,
        recording: changes,
    });
    prepared
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MemberState {
    Unarmed,
    Active,
    Carrier,
}
impl MemberState {
    fn code(&self) -> char {
        match self {
            Self::Unarmed => 'u',
            Self::Active => 'a',
            Self::Carrier => 'c',
        }
    }
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "u" => Ok(Self::Unarmed),
            "a" => Ok(Self::Active),
            "c" => Ok(Self::Carrier),
            _ => Err(invalid("invalid member state")),
        }
    }
}
#[derive(Clone, Debug)]
struct Member {
    rank: u64,
    invocation_id: String,
    bucket: u64,
    state: MemberState,
}
fn member_key(
    config: &Config,
    epoch: &str,
    bucket: u64,
    rank: u64,
    state: &MemberState,
    invocation_id: &str,
) -> String {
    format!(
        "{}{epoch}/{:016}/{rank:016x}.{}.{invocation_id}",
        config.members_prefix(),
        bucket,
        state.code()
    )
}
fn parse_member(key: &str, prefix: &str, bucket: u64) -> Result<Member, StoreError> {
    let suffix = key
        .strip_prefix(prefix)
        .ok_or_else(|| invalid("member outside requested prefix"))?;
    let mut parts = suffix.split('.');
    let rank_hex = parts.next().ok_or_else(|| invalid("missing member rank"))?;
    let rank = u64::from_str_radix(rank_hex, 16).map_err(invalid)?;
    let state = MemberState::parse(
        parts
            .next()
            .ok_or_else(|| invalid("missing member state"))?,
    )?;
    let invocation_id = parts
        .next()
        .ok_or_else(|| invalid("missing invocation identity"))?;
    if rank_hex.len() != 16
        || format!("{rank:016x}") != rank_hex
        || parts.next().is_some()
        || invocation_id.parse::<ulid::Ulid>().is_err()
    {
        return Err(invalid("invalid sampling member key"));
    }
    Ok(Member {
        rank,
        invocation_id: invocation_id.into(),
        bucket,
        state,
    })
}
async fn list_bucket(
    store: &dyn SamplingStore,
    config: &Config,
    epoch: &str,
    bucket: u64,
) -> Result<Vec<Member>, StoreError> {
    let prefix = format!("{}{epoch}/{:016}/", config.members_prefix(), bucket);
    let mut continuation = None;
    let mut seen = HashSet::new();
    let mut members = Vec::new();
    loop {
        let page = store
            .list_page(
                ListRequest::builder()
                    .prefix(&prefix)
                    .page_size(1000)
                    .maybe_continuation(continuation)
                    .build(),
            )
            .await?;
        for object in page.objects() {
            members.push(parse_member(object.key(), &prefix, bucket)?);
        }
        continuation = page.continuation().cloned();
        match &continuation {
            None => return Ok(members),
            Some(token) if !seen.insert(token.as_str().to_owned()) => {
                return Err(invalid("repeated LIST continuation token"));
            }
            Some(_) => {}
        }
    }
}
async fn census(
    store: &dyn SamplingStore,
    config: &Config,
    reporting: &ReportingEpoch,
    bucket: u64,
) -> Result<Vec<Member>, StoreError> {
    let current = list_bucket(store, config, &reporting.id, bucket);
    let previous = async {
        if bucket == 0 {
            Ok(Vec::new())
        } else {
            list_bucket(store, config, &reporting.id, bucket - 1).await
        }
    };
    let (current, previous) = tokio::try_join!(current, previous)?;
    let mut unique: HashMap<String, Member> = HashMap::new();
    for member in previous.into_iter().chain(current) {
        match unique.entry(member.invocation_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(member);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let old = entry.get();
                if old.rank != member.rank {
                    return Err(invalid("invocation identity has inconsistent ranks"));
                }
                // State is terminal within an application run. A delayed active
                // PUT or backwards clock adjustment cannot outrank a carrier.
                if (&member.state, member.bucket) > (&old.state, old.bucket) {
                    entry.insert(member);
                }
            }
        }
    }
    Ok(unique.into_values().collect())
}
fn compute(
    roster: &mut Roster,
    config: &Config,
    members: &[Member],
    history: &mut VecDeque<u64>,
    now: u64,
    collection_delay_ms: u64,
) {
    let counts = count_members(members, &roster.reporting);
    let settled = settle(history, counts.tracked);
    if settled && counts.tracked > 0 {
        roster.population = Some(Population {
            reporting: roster.reporting.clone(),
            reporters: counts.tracked,
            computed_ms: now,
        });
    }
    let budget = u64::from(config.target()) * 32;
    let outside = counts.tracked < budget / 2 || counts.tracked > budget * 2;
    roster.outside_budget = if outside {
        (roster.outside_budget + 1).min(3)
    } else {
        0
    };
    if counts.tracked == 0
        && roster
            .pending
            .as_ref()
            .is_some_and(|pending| pending.reporting.cutoff.0.is_some())
    {
        // A smaller pending subset can also have lost every reporter. Recovery
        // must not wait forever for that abandoned census to become nonempty.
        roster.pending = None;
    }
    if roster.pending.is_none() {
        let next = if counts.tracked == 0 {
            // Zero sampled reporters is loss of visibility, not an empty fleet.
            Some(Cutoff(None))
        } else if settled && roster.outside_budget == 3 {
            Some(Cutoff::threshold(
                (u128::from(budget) * roster.reporting.cutoff.value() / u128::from(counts.tracked))
                    .max(1),
            ))
        } else {
            None
        };
        if let Some(cutoff) = next
            && cutoff != roster.reporting.cutoff
        {
            roster.pending = Some(Handoff {
                reporting: ReportingEpoch {
                    id: nonce(),
                    cutoff,
                },
                created_ms: now,
                collect_after_ms: now.saturating_add(collection_delay_ms),
            });
        }
    }
    let cap = u64::from(config.target()) + u64::from(config.overshoot());
    if counts.active <= cap {
        roster.trim = None;
    } else if roster.trim.as_ref().is_none_or(|trim| {
        now.saturating_sub(trim.created_ms)
            >= config.poll_interval.as_millis() as u64 * 3
                + config.max_clock_skew.as_millis() as u64 * 2
    }) {
        roster.trim = Some(Trim {
            epoch: roster.epoch,
            salt: nonce(),
            created_ms: now,
            survival: Cutoff::threshold(
                (u128::from(config.target()) << 64) / u128::from(counts.active),
            ),
        });
    }
    roster.settled = settled && counts.tracked > 0;
    roster.counts = counts;
    roster.census_computed_at_ms = Some(now);
}
fn count_members(members: &[Member], reporting: &ReportingEpoch) -> Counts {
    let mut counts = Counts::default();
    for member in members {
        counts.observed += 1;
        if reporting.cutoff.includes(member.rank) {
            counts.tracked += 1;
        }
        match member.state {
            MemberState::Unarmed => counts.unarmed += 1,
            MemberState::Carrier => counts.carriers += 1,
            MemberState::Active => {
                counts.active += 1;
            }
        }
    }
    counts
}
fn settle(history: &mut VecDeque<u64>, tracked: u64) -> bool {
    history.push_back(tracked);
    if history.len() > 4 {
        history.pop_front();
    }
    // This only detects a plateau; it cannot certify LIST snapshot completeness
    // or that a future cohort has the same population.
    history.len() == 4
        && history
            .iter()
            .zip(history.iter().skip(1))
            .all(|(&a, &b)| u128::from(b) * 100 <= u128::from(a) * 102)
}

struct Worker {
    config: Config,
    store: Arc<dyn SamplingStore>,
    clock: Arc<dyn Clock>,
    invocation_id: String,
    rank: u64,
    state: MemberState,
    observed: Option<Observed>,
    leader: Option<Observed>,
    history: VecDeque<u64>,
    pending_history: VecDeque<u64>,
    pending_history_epoch: Option<String>,
    signal: watch::Sender<bool>,
    keys: BTreeMap<(String, u64), HashSet<String>>,
    written_buckets: BTreeMap<String, u64>,
    terminal_until_bucket: Option<u64>,
    offset_ms: u64,
    poll_ms: u64,
    membership_allowed: bool,
    last_trim_epoch: u64,
    weights: Option<crate::trace_weights::TraceWeights>,
}
impl Worker {
    fn new(
        config: Config,
        store: Arc<dyn SamplingStore>,
        clock: Arc<dyn Clock>,
        observed: Option<Observed>,
        signal: watch::Sender<bool>,
        membership_allowed: bool,
    ) -> Self {
        let recording = *signal.borrow();
        let invocation_id = config.invocation_id.clone();
        let last_trim_epoch = observed.as_ref().map_or(0, |o| o.roster.epoch);
        let entropy = invocation_id
            .parse::<ulid::Ulid>()
            .expect("fresh invocation identity")
            .random() as u64;
        let offset_ms = entropy % config.bucket_width.as_millis() as u64;
        // A fixed per-run jitter also spreads polls and remains testable.
        let poll_ms = (config.poll_interval.as_millis() as u64 * (900 + entropy % 201)) / 1000;
        let rank = config.rank();
        Self {
            config,
            store,
            clock,
            invocation_id,
            rank,
            state: if recording {
                MemberState::Active
            } else {
                MemberState::Unarmed
            },
            observed,
            leader: None,
            history: VecDeque::new(),
            pending_history: VecDeque::new(),
            pending_history_epoch: None,
            signal,
            keys: BTreeMap::new(),
            written_buckets: BTreeMap::new(),
            terminal_until_bucket: None,
            offset_ms,
            poll_ms,
            membership_allowed,
            last_trim_epoch,
            weights: None,
        }
    }
    fn bucket(&self) -> Result<u64, StoreError> {
        Ok(millis(self.clock.now())? / self.config.bucket_width.as_millis() as u64)
    }
    fn reporting_epochs(&self) -> Vec<ReportingEpoch> {
        let Some(observed) = &self.observed else {
            return vec![ReportingEpoch::bootstrap()];
        };
        std::iter::once(observed.roster.reporting.clone())
            .chain(
                observed
                    .roster
                    .pending
                    .as_ref()
                    .map(|p| p.reporting.clone()),
            )
            .collect()
    }
    fn should_report(&self, epoch: &ReportingEpoch, bucket: u64) -> bool {
        epoch.cutoff.includes(self.config.reporting_rank(&epoch.id))
            || self.state == MemberState::Active
            || (self.state == MemberState::Carrier
                && self
                    .terminal_until_bucket
                    .is_some_and(|until| bucket <= until))
    }
    async fn write_member(&mut self, epoch: &str, bucket: u64) -> Result<(), StoreError> {
        let key = member_key(
            &self.config,
            epoch,
            bucket,
            self.config.reporting_rank(epoch),
            &self.state,
            &self.invocation_id,
        );
        // Track even an unacknowledged PUT: cleanup is safe because every key
        // belongs to this run, and never to a replacement process.
        self.keys.retain(|(_, b), _| *b >= bucket.saturating_sub(1));
        self.keys
            .entry((epoch.into(), bucket))
            .or_default()
            .insert(key.clone());
        match self
            .store
            .put(
                PutRequest::builder()
                    .key(key)
                    .body(Bytes::new())
                    .condition(WriteCondition::unconditional())
                    .build(),
            )
            .await?
        {
            PutOutcome::Stored { .. } => {
                self.written_buckets.insert(epoch.into(), bucket);
                Ok(())
            }
            other => Err(invalid(format!(
                "unconditional membership PUT returned {other:?}"
            ))),
        }
    }
    async fn refresh_member(&mut self, immediate: bool) -> Result<(), StoreError> {
        if !self.membership_allowed {
            return Ok(());
        }
        let now = millis(self.clock.now())?;
        let width = self.config.bucket_width.as_millis() as u64;
        let bucket = now / width;
        let epochs = self.reporting_epochs();
        self.written_buckets
            .retain(|id, _| epochs.iter().any(|epoch| &epoch.id == id));
        for epoch in epochs {
            if self.should_report(&epoch, bucket)
                && (immediate
                    || !self.written_buckets.contains_key(&epoch.id)
                    || (self.written_buckets.get(&epoch.id) != Some(&bucket)
                        && now % width >= self.offset_ms))
            {
                self.write_member(&epoch.id, bucket).await?;
            }
        }
        Ok(())
    }
    async fn read_roster(&self) -> Result<Option<Observed>, StoreError> {
        self.store
            .get(format!("{}roster.json", self.config.pool_prefix()))
            .await?
            .map(|object| decode(object, &self.config))
            .transpose()
    }
    async fn replace(
        &self,
        roster: &Roster,
        previous: Option<&VersionToken>,
    ) -> Result<Option<Observed>, StoreError> {
        let condition = match previous {
            Some(version) => WriteCondition::matching(version.clone()),
            None => WriteCondition::absent(),
        };
        match self
            .store
            .put(
                PutRequest::builder()
                    .key(format!("{}roster.json", self.config.pool_prefix()))
                    .body(serialize(roster))
                    .condition(condition)
                    .build(),
            )
            .await?
        {
            PutOutcome::Stored { version } => Ok(Some(Observed {
                roster: roster.clone(),
                version,
                modified_at: self.clock.now(),
            })),
            PutOutcome::PreconditionFailed {} => Ok(None),
        }
    }
    async fn election(&mut self) -> Result<(), StoreError> {
        if self.observed.as_ref().is_some_and(|observed| {
            !expired(
                observed,
                &self.config,
                self.store.as_ref(),
                self.clock.now(),
            )
        }) {
            return Ok(());
        }
        let delay_span_ms = (self.config.poll_interval.as_millis() / 2).min(2000);
        let delay_ms = u128::from(self.rank) * delay_span_ms / (1u128 << 64);
        tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
        // Most contenders see the winner during this one background re-read.
        // Close races can still cause losing CAS writes; there is no probe loop.
        self.observed = self.read_roster().await?;
        if self.observed.as_ref().is_some_and(|observed| {
            !expired(
                observed,
                &self.config,
                self.store.as_ref(),
                self.clock.now(),
            )
        }) {
            return Ok(());
        }
        let mut roster = match &self.observed {
            Some(observed) => observed.roster.clone(),
            None => Roster::initial(&self.config, &self.invocation_id),
        };
        // Acquisition must not freshen an old cohort's thinning round.
        // Preserve the settled startup cutoff, and recompute thinning after LIST.
        roster.trim = None;
        roster.settled = false;
        roster.mutate(&self.invocation_id)?;
        let claimed = self
            .replace(
                &roster,
                self.observed.as_ref().map(|observed| &observed.version),
            )
            .await?;
        self.leader = claimed.clone();
        if let Some(claimed) = claimed {
            self.observed = Some(claimed);
            self.history.clear();
            self.pending_history.clear();
            self.pending_history_epoch = None;
        }
        Ok(())
    }
    fn guard_ms(&self) -> u128 {
        self.config
            .max_clock_skew
            .saturating_mul(2)
            .saturating_add(self.store.modification_time_uncertainty())
            .as_nanos()
            .div_ceil(1_000_000)
    }
    fn census_window(&self, now: u64) -> bool {
        let width = self.config.bucket_width.as_millis();
        let phase = u128::from(now) % width;
        let guard = self.guard_ms();
        phase >= guard && phase.saturating_add(guard) < width
    }
    fn pass_cadence(&self) -> Duration {
        if let Ok(now) = millis(self.clock.now()) {
            let width = self.config.bucket_width.as_millis();
            let phase = u128::from(now) % width;
            let guard = self.guard_ms();
            if guard.saturating_mul(2) >= width {
                rate_limited!(Duration::from_secs(60), {
                    tracing::error!("sampling timestamp uncertainty leaves no safe census window");
                });
                return self.config.poll_interval;
            }
            if !self.census_window(now) {
                let wait = if phase < guard {
                    guard - phase
                } else {
                    width - phase + guard
                };
                return Duration::from_millis(wait as u64).min(self.config.poll_interval);
            }
        }
        if self.leader.as_ref().is_some_and(|o| {
            o.roster
                .pending
                .as_ref()
                .map_or(o.roster.settled, |pending| {
                    millis(self.clock.now()).is_ok_and(|now| now < pending.collect_after_ms)
                })
        }) {
            self.config.poll_interval
        } else {
            Duration::from_secs(1).min(self.config.poll_interval)
        }
    }
    async fn leader_pass(&mut self) -> Result<(), StoreError> {
        let Some(previous) = self.leader.take() else {
            return Ok(());
        };
        let started = millis(self.clock.now())?;
        let width = self.config.bucket_width.as_millis() as u64;
        let bucket = started / width;
        let mut roster = previous.roster.clone();
        roster.mutate(&self.invocation_id)?;
        let members = if self.census_window(started) {
            Some(census(self.store.as_ref(), &self.config, &roster.reporting, bucket).await?)
        } else {
            None
        };
        let pending_members = match &roster.pending {
            Some(pending)
                if members.is_some()
                    && started
                        >= pending
                            .collect_after_ms
                            .saturating_add(self.config.max_clock_skew.as_millis() as u64 * 2) =>
            {
                Some((
                    pending.reporting.clone(),
                    census(
                        self.store.as_ref(),
                        &self.config,
                        &pending.reporting,
                        bucket,
                    )
                    .await?,
                ))
            }
            _ => None,
        };
        let now = millis(self.clock.now())?;
        if let Some(members) = members
            && now >= started
            && now / width == bucket
            && self.census_window(now)
        {
            let collection_delay_ms = self.config.poll_interval.as_millis() as u64 * 5
                + self.config.max_clock_skew.as_millis() as u64 * 2
                + u64::try_from(
                    self.store
                        .modification_time_uncertainty()
                        .as_nanos()
                        .div_ceil(1_000_000),
                )
                .map_err(invalid)?;
            compute(
                &mut roster,
                &self.config,
                &members,
                &mut self.history,
                now,
                collection_delay_ms,
            );
            if let Some((reporting, pending_members)) = pending_members
                && roster
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.reporting.id == reporting.id)
            {
                if self.pending_history_epoch.as_ref() != Some(&reporting.id) {
                    self.pending_history.clear();
                    self.pending_history_epoch = Some(reporting.id.clone());
                }
                let counts = count_members(&pending_members, &reporting);
                if settle(&mut self.pending_history, counts.tracked) && counts.tracked > 0 {
                    roster.population = Some(Population {
                        reporting: reporting.clone(),
                        reporters: counts.tracked,
                        computed_ms: now,
                    });
                    roster.reporting = reporting;
                    roster.pending = None;
                    roster.outside_budget = 0;
                    roster.counts = counts;
                    roster.settled = true;
                    // Only one epoch's profilers may drive a trim. Start the
                    // next assessment from the promoted membership namespace.
                    roster.trim = None;
                    self.history = std::mem::take(&mut self.pending_history);
                    self.pending_history_epoch = None;
                }
            }
        } else {
            // Opposite-skew clients can temporarily fall outside the two listed
            // buckets. Skip edge LISTs, renew the lease, and retain the arm cutoff.
            // A census crossing the boundary also cannot supply a new cutoff.
            roster.settled = false;
            roster.trim = None;
            self.history.clear();
            self.pending_history.clear();
            self.pending_history_epoch = None;
        }
        let published = self.replace(&roster, Some(&previous.version)).await?;
        if let Some(observed) = &published {
            tracing::debug!(tracked = observed.roster.counts.tracked, active = observed.roster.counts.active,
                population = ?observed.roster.population.as_ref().map(Population::estimate),
                settled = observed.roster.settled,
                slice_too_small = ?observed.roster.census_computed_at_ms.map(|_| {
                    observed.roster.counts.tracked < u64::from(self.config.target()) * 4
                }),
                "sampling census published");
            self.observed = Some(observed.clone());
        }
        self.leader = published;
        self.apply_trim().await
    }
    async fn apply_trim(&mut self) -> Result<(), StoreError> {
        let Some(observed) = &self.observed else {
            return Ok(());
        };
        if expired(
            observed,
            &self.config,
            self.store.as_ref(),
            self.clock.now(),
        ) {
            return Ok(());
        }
        // A delayed server commit gets a fresh LastModified even when its
        // census is old. Require the embedded census age independently.
        let now = u128::from(millis(self.clock.now())?);
        let skew = self.config.max_clock_skew.as_millis() * 2;
        let Some(computed) = observed.roster.census_computed_at_ms.map(u128::from) else {
            return Ok(());
        };
        if computed > now + skew
            || now > computed + self.config.poll_interval.as_millis() * 3 + skew
        {
            return Ok(());
        }
        let Some(trim) = &observed.roster.trim else {
            return Ok(());
        };
        if self.state != MemberState::Active || trim.epoch <= self.last_trim_epoch {
            return Ok(());
        }
        self.last_trim_epoch = trim.epoch;
        if trim.survival.includes(self.config.draw("trim", &trim.salt)) {
            if let Some(weights) = &self.weights {
                weights.multiply(1.0 / trim.survival.probability())?;
            }
        } else {
            if let Some(weights) = &self.weights {
                weights.stop();
            }
            self.state = MemberState::Carrier;
            self.written_buckets.clear();
            self.signal.send_replace(false);
            tracing::info!(
                rank = self.rank,
                "sampling active process excluded by thinning"
            );
            let bucket = self.bucket()?;
            self.terminal_until_bucket = Some(bucket.saturating_add(1));
            self.refresh_member(true).await?;
            // Carrier wins dedup even when removal fails. Never delete peers.
            let old_keys: Vec<_> = self
                .keys
                .iter()
                .flat_map(|((epoch, bucket), keys)| {
                    keys.iter()
                        .filter(|key| key.contains(".a."))
                        .map(|key| (epoch.clone(), *bucket, key.clone()))
                })
                .collect();
            for (epoch, old, key) in old_keys {
                if let Err(error) = self.store.delete(key.clone()).await {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(%error, "sampling active member cleanup failed");
                    });
                } else if let Some(keys) = self.keys.get_mut(&(epoch, old)) {
                    keys.remove(&key);
                }
            }
        }
        Ok(())
    }
    async fn poll(&mut self) -> Result<(), StoreError> {
        // A failed/malformed/incompatible GET cannot be treated as missing. In
        // particular, it never permits overwriting an incompatible namespace.
        self.observed = match self.read_roster().await {
            Ok(observed) => {
                self.membership_allowed = true;
                observed
            }
            Err(error) => {
                if matches!(error.kind(), StoreErrorKind::Unsupported {}) {
                    self.membership_allowed = false;
                    self.cleanup().await;
                }
                return Err(error);
            }
        };
        // New epoch eligibility is handled immediately, rather than waiting for
        // this process's next bucket heartbeat offset.
        self.refresh_member(false).await?;
        if self.leader.as_ref().is_some_and(|leader| {
            self.observed.as_ref().is_none_or(|current| {
                current.version != leader.version || current.roster.holder != self.invocation_id
            })
        }) {
            self.leader = None;
        }
        if self.leader.is_none()
            && let Some(current) = &self.observed
            && current.roster.holder == self.invocation_id
            && !expired(current, &self.config, self.store.as_ref(), self.clock.now())
        {
            // A GET reconciles an acknowledged-lost CAS on the same object.
            self.leader = Some(current.clone());
            self.history.clear();
            self.pending_history.clear();
            self.pending_history_epoch = None;
        }
        self.apply_trim().await?;
        if self.leader.is_none() {
            self.election().await?;
        }
        Ok(())
    }
    async fn run(&mut self) {
        let mut next_poll = Instant::now();
        let mut next_pass = Instant::now();
        loop {
            let operation_budget = self.config.poll_interval;
            if let Err(error) = bounded(operation_budget, self.refresh_member(false)).await {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(%error, "sampling membership refresh failed");
                });
            }
            if Instant::now() >= next_poll {
                if let Err(error) = bounded(operation_budget, self.poll()).await {
                    self.leader = None;
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(%error, "sampling roster poll/election failed");
                    });
                }
                next_poll = Instant::now() + Duration::from_millis(self.poll_ms);
            }
            if self.leader.is_some() && Instant::now() >= next_pass {
                if let Err(error) = bounded(operation_budget, self.leader_pass()).await {
                    self.leader = None;
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(%error, "sampling leader census failed");
                    });
                }
                let cadence = self.pass_cadence();
                next_pass = Instant::now() + cadence;
            }
            let mut next = next_poll;
            if self.leader.is_some() {
                next = next.min(next_pass);
            }
            // Wake at the per-host heartbeat offset, even when polls are spread.
            if let Ok(now) = millis(self.clock.now()) {
                let width = self.config.bucket_width.as_millis() as u64;
                let bucket = now / width;
                let due = bucket * width + self.offset_ms;
                let due = if due <= now {
                    due.saturating_add(width)
                } else {
                    due
                };
                next = next.min(Instant::now() + Duration::from_millis(due - now));
            }
            tokio::time::sleep_until(next).await;
        }
    }
    async fn cleanup(&mut self) {
        // Include a key whose PUT may be in flight when cancellation wins. The
        // backend contract requires cancellation-safe operations; a late server
        // commit is still bounded by bucket visibility and lifecycle expiration.
        for key in self.keys.values().flat_map(|keys| keys.iter()) {
            if let Err(error) = self.store.delete(key.clone()).await {
                tracing::debug!(%error, "sampling member shutdown cleanup failed");
            }
        }
    }
}
async fn bounded<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T, StoreError>>,
) -> Result<T, StoreError> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        StoreError::new(
            StoreErrorKind::unavailable(),
            "sampling maintenance deadline expired",
        )
    })?
}

#[cfg(all(test, not(shuttle)))]
#[path = "tests.rs"]
mod tests;
