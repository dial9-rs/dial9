use super::*;
use crate::backend::{
    ContinuationToken, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreError,
    StoreErrorKind, StoredObject, WriteCondition,
};
#[cfg(test)]
use crate::backend::{ListPage, StoreFuture};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
/// Reason for the current profiling choice; failure fallback can still enable profiling.
pub enum DecisionReason {
    #[non_exhaustive]
    /// A conditional allocation was confirmed.
    Admitted {},
    #[non_exhaustive]
    /// No allocation was available; this process stays off.
    Full {},
    #[non_exhaustive]
    /// Profiling was disabled before storage preparation.
    TargetZero {},
    #[non_exhaustive]
    /// Invalid labels, settings or missing integration prevent profiling.
    InvalidConfiguration {
        /// Explanation of the invalid setting or missing integration.
        message: String,
    },
    #[non_exhaustive]
    /// Caller settings do not match the shared configuration.
    IncompatiblePolicy {},
    #[non_exhaustive]
    /// The startup budget elapsed; the failure fallback determines recording.
    Timeout {},
    #[non_exhaustive]
    /// Storage or I/O initialization failed; the failure fallback determines recording.
    StoreFailure {
        /// Explanation of the storage or I/O initialization failure.
        message: String,
    },
    #[non_exhaustive]
    /// Coordination was lost after startup; the configured lease-loss policy applies.
    LeaseLost {},
}
impl DecisionReason {
    /// Constructs a startup-deadline failure.
    pub fn timeout() -> Self {
        Self::Timeout {}
    }
    /// Constructs an explicitly disabled startup choice.
    pub fn target_zero() -> Self {
        Self::TargetZero {}
    }
    /// Constructs a configuration failure with its explanation.
    pub fn invalid_configuration(message: impl Into<String>) -> Self {
        Self::InvalidConfiguration {
            message: message.into(),
        }
    }
    /// Constructs a storage failure with its explanation.
    pub fn store_failure(message: impl Into<String>) -> Self {
        Self::StoreFailure {
            message: message.into(),
        }
    }
}
#[derive(Clone, Debug)]
/// A point-in-time choice and optional coordination/population diagnostics.
pub struct SamplingSnapshot {
    pub(crate) recording: bool,
    pub(crate) reason: DecisionReason,
    pub(crate) coordinated: bool,
    pub(crate) estimated_population: Option<f64>,
    pub(crate) membership_registered: bool,
    pub(crate) population_updated_at: Option<SystemTime>,
}
impl SamplingSnapshot {
    /// Whether this choice permits profiling. Lease-loss continuity or failure fallback can remain enabled without coordination.
    pub fn recording(&self) -> bool {
        self.recording
    }
    /// Returns the decision or lease-loss reason; consult recording() separately for the actual choice.
    pub fn reason(&self) -> &DecisionReason {
        &self.reason
    }
    /// Whether this process currently has a confirmed valid allocation.
    #[cfg(all(test, not(shuttle)))]
    pub fn coordinated(&self) -> bool {
        self.coordinated
    }
    /// Returns an estimate only when enough live membership has been observed; unknown is never zero.
    #[cfg(all(test, not(shuttle)))]
    pub fn estimated_population(&self) -> Option<f64> {
        self.estimated_population
    }
    /// Whether the most recent successful refresh confirmed this process in the membership sample.
    #[cfg(all(test, not(shuttle)))]
    pub fn membership_registered(&self) -> bool {
        self.membership_registered
    }
    /// Last successful membership refresh. Failed refreshes clear this timestamp
    /// and the estimate; a missing estimate never represents a zero-sized pool.
    #[cfg(all(test, not(shuttle)))]
    pub fn population_updated_at(&self) -> Option<SystemTime> {
        self.population_updated_at
    }
}
/// A completed startup choice and ownership of any background maintenance.
pub struct PreparedSampling {
    snapshot: Arc<Mutex<SamplingSnapshot>>,
    session: Option<SamplingSession>,
}
impl std::fmt::Debug for PreparedSampling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(f)
    }
}
impl PreparedSampling {
    /// Constructs a completed choice without storage or background maintenance.
    ///
    /// `recording` is the final choice, including any startup-failure fallback.
    pub fn disabled(reason: DecisionReason, recording: bool) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(SamplingSnapshot {
                recording,
                reason,
                coordinated: false,
                estimated_population: None,
                membership_registered: false,
                population_updated_at: None,
            })),
            session: None,
        }
    }
    /// Clones current diagnostics. Later maintenance can change coordination and freshness.
    pub fn snapshot(&self) -> SamplingSnapshot {
        self.snapshot.lock().expect("snapshot lock").clone()
    }
    /// Transfers maintenance ownership, or returns None if no session was started.
    pub fn into_session(mut self) -> Option<SamplingSession> {
        self.session.take()
    }
}

/// Owns coordination, not a profiler. Drop requests bounded cleanup.
pub struct SamplingSession {
    snapshot: Arc<Mutex<SamplingSnapshot>>,
    stop: CancellationToken,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
    recording: watch::Receiver<bool>,
    cleanup_budget: Arc<Mutex<Duration>>,
}
impl std::fmt::Debug for SamplingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(f)
    }
}
impl SamplingSession {
    /// Clones current diagnostics. Later maintenance can change coordination and freshness.
    pub fn snapshot(&self) -> SamplingSnapshot {
        self.snapshot.lock().expect("snapshot lock").clone()
    }
    /// Subscribes to the recording choice. An optional lease-loss stop is permanent.
    pub fn recording_changes(&self) -> watch::Receiver<bool> {
        self.recording.clone()
    }
    /// Stops maintenance and attempts owner-checked release and membership withdrawal within the budget.
    pub async fn shutdown(mut self, timeout: Duration) {
        *self.cleanup_budget.lock().expect("cleanup budget lock") = timeout;
        self.stop.cancel();
        if let Some(done) = self.done.take()
            && tokio::time::timeout(timeout, done).await.is_err()
        {
            tracing::debug!("sampling cleanup deadline expired");
        }
    }
}
impl Drop for SamplingSession {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Policy {
    schema: u32,
    lease_ms: u64,
    skew_ms: u64,
}
impl Policy {
    fn compatible(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.lease_ms == other.lease_ms
            && self.skew_ms == other.skew_ms
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lease {
    owner: Option<String>,
    nonce: String,
}
struct HeldLease {
    key: String,
    valid_until: Instant,
}
fn serialize(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("sampling record serializes"))
}
fn parse<T: serde::de::DeserializeOwned>(object: &StoredObject) -> Result<T, StoreError> {
    serde_json::from_slice(object.body())
        .map_err(|e| StoreError::new(StoreErrorKind::InvalidResponse {}, e))
}
fn nonce() -> String {
    ulid::Ulid::new().to_string()
}
fn probability_draw(id: &str) -> f64 {
    let digest = Sha256::digest(id.as_bytes());
    let bits = u64::from_be_bytes(digest[..8].try_into().expect("digest length"));
    (bits >> 11) as f64 / (1u64 << 53) as f64
}
/// Generates one uniform startup draw in [0, 1) for a failure fallback.
pub fn startup_draw() -> f64 {
    probability_draw(&nonce())
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

#[cfg(all(test, not(shuttle)))]
pub(crate) async fn prepare(
    policy: Config,
    store: Arc<dyn SamplingStore>,
    timeout: Duration,
) -> PreparedSampling {
    prepare_using(policy, store, timeout, None, Arc::new(WallClock)).await
}

pub(crate) async fn prepare_using(
    policy: Config,
    store: Arc<dyn SamplingStore>,
    timeout: Duration,
    draw: Option<f64>,
    clock: Arc<dyn Clock>,
) -> PreparedSampling {
    if Instant::now().checked_add(timeout).is_none() {
        return PreparedSampling::disabled(
            DecisionReason::invalid_configuration("preparation deadline overflow"),
            false,
        );
    }
    if let Err(message) = policy.validate() {
        return PreparedSampling::disabled(DecisionReason::InvalidConfiguration { message }, false);
    }
    if policy.target() == 0 {
        return PreparedSampling::disabled(DecisionReason::TargetZero {}, false);
    }
    let owner = nonce();
    let draw = draw.unwrap_or_else(|| probability_draw(&owner));
    let started = Instant::now();
    let result =
        tokio::time::timeout(timeout, admit(&policy, &store, &owner, clock.as_ref())).await;
    let (reason, held, recording) = match result {
        Ok(Ok(Some(held))) => (DecisionReason::Admitted {}, Some(held), true),
        Ok(Ok(None)) => (DecisionReason::Full {}, None, false),
        Ok(Err(AdmissionError::Incompatible)) => {
            return PreparedSampling::disabled(DecisionReason::IncompatiblePolicy {}, false);
        }
        Ok(Err(AdmissionError::Store(error))) => {
            if matches!(
                error.kind(),
                StoreErrorKind::InvalidResponse { .. } | StoreErrorKind::Unsupported { .. }
            ) {
                return PreparedSampling::disabled(
                    DecisionReason::invalid_configuration(error.to_string()),
                    false,
                );
            }
            (
                DecisionReason::StoreFailure {
                    message: error.to_string(),
                },
                None,
                policy.failure_decision(draw),
            )
        }
        Err(_) => (
            DecisionReason::Timeout {},
            None,
            policy.failure_decision(draw),
        ),
    };
    let mut prepared = PreparedSampling::disabled(reason, recording);
    prepared.snapshot.lock().expect("snapshot lock").coordinated = held.is_some();
    let snapshot = prepared.snapshot.clone();
    let stop = CancellationToken::new();
    let (done_tx, done) = tokio::sync::oneshot::channel();
    let (recording_tx, recording_rx) = watch::channel(recording);
    let task_stop = stop.clone();
    let cleanup_budget = Arc::new(Mutex::new(DEFAULT_PREPARE_TIMEOUT));
    let task_cleanup_budget = cleanup_budget.clone();
    let stop_on_loss = matches!(policy.lease_loss, LeaseLoss::StopProfiling { .. });
    tokio::spawn(async move {
        let mut guard = MaintenanceGuard {
            snapshot: snapshot.clone(),
            recording: recording_tx.clone(),
            stop_on_loss,
            armed: true,
        };
        maintain(Maintenance {
            policy,
            store,
            owner,
            held,
            snapshot,
            stop: task_stop,
            recording: recording_tx,
            started,
            clock,
            cleanup_budget: task_cleanup_budget,
        })
        .await;
        guard.armed = false;
        if done_tx.send(()).is_err() {
            tracing::trace!("sampling cleanup receiver dropped");
        }
    });
    prepared.session = Some(SamplingSession {
        snapshot: prepared.snapshot.clone(),
        stop,
        done: Some(done),
        recording: recording_rx,
        cleanup_budget,
    });
    prepared
}

#[derive(Debug)]
enum AdmissionError {
    Incompatible,
    Store(StoreError),
}
impl From<StoreError> for AdmissionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
async fn pool_policy(
    policy: &Config,
    store: &Arc<dyn SamplingStore>,
    clock: &dyn Clock,
) -> Result<(), AdmissionError> {
    let key = format!("{}policy.json", policy.pool_prefix());
    let desired = Policy {
        schema: 1,
        lease_ms: policy.lease_duration.as_millis() as u64,
        skew_ms: policy.max_clock_skew.as_millis() as u64,
    };
    let condition = match store.get(key.clone()).await? {
        Some(object) => {
            if !parse::<Policy>(&object)?.compatible(&desired) {
                return Err(AdmissionError::Incompatible);
            }
            if policy_is_fresh(policy, &object, clock) {
                return Ok(());
            }
            WriteCondition::matching(object.version().clone())
        }
        None => WriteCondition::absent(),
    };
    match store
        .put(
            PutRequest::builder()
                .key(key.clone())
                .body(serialize(&desired))
                .condition(condition)
                .build(),
        )
        .await?
    {
        PutOutcome::Stored { .. } => Ok(()),
        _ => match store.get(key).await? {
            Some(object) => {
                if !parse::<Policy>(&object)?.compatible(&desired) {
                    return Err(AdmissionError::Incompatible);
                }
                if policy_is_fresh(policy, &object, clock) {
                    Ok(())
                } else {
                    Err(StoreError::new(
                        StoreErrorKind::unavailable(),
                        "shared policy refresh was not confirmed",
                    )
                    .into())
                }
            }
            None => Err(StoreError::new(
                StoreErrorKind::Unavailable {},
                "pool creation conflicted",
            )
            .into()),
        },
    }
}
fn policy_is_fresh(policy: &Config, object: &StoredObject, clock: &dyn Clock) -> bool {
    object
        .modified_at()
        .checked_add(policy.renewal_interval)
        .is_some_and(|fresh_until| clock.now() < fresh_until)
}
async fn admit(
    policy: &Config,
    store: &Arc<dyn SamplingStore>,
    owner: &str,
    clock: &dyn Clock,
) -> Result<Option<HeldLease>, AdmissionError> {
    pool_policy(policy, store, clock).await?;
    let start = (probability_draw(owner) * f64::from(policy.target())) as u32;
    for offset in 0..policy.target() {
        let slot = (start + offset) % policy.target();
        let key = format!("{}slots/{slot:08}.json", policy.pool_prefix());
        // A release can replace the observed expired object between GET and
        // PUT. Re-read the same slot before declaring a target-one pool full.
        for _ in 0..3 {
            let observed = store.get(key.clone()).await?;
            let condition = match observed {
                None => WriteCondition::absent(),
                Some(ref object) => {
                    let lease: Lease = parse(object)?;
                    let reusable = object.modified_at().checked_add(
                        policy
                            .lease_duration
                            .saturating_add(policy.max_clock_skew)
                            .saturating_add(store.modification_time_uncertainty()),
                    );
                    if lease.owner.is_some() && reusable.is_none_or(|time| clock.now() <= time) {
                        break;
                    }
                    WriteCondition::matching(object.version().clone())
                }
            };
            let sent = Instant::now();
            let lease = Lease {
                owner: Some(owner.to_string()),
                nonce: nonce(),
            };
            match store
                .put(
                    PutRequest::builder()
                        .key(key.clone())
                        .body(serialize(&lease))
                        .condition(condition)
                        .build(),
                )
                .await
            {
                Ok(PutOutcome::Stored { .. }) => {
                    return Ok(Some(HeldLease {
                        key: key.clone(),
                        valid_until: sent + policy.lease_duration - policy.max_clock_skew * 2,
                    }));
                }
                Ok(PutOutcome::PreconditionFailed { .. }) => {}
                Ok(PutOutcome::RetryableConflict { .. }) => {
                    // A 409 can have an uncertain outcome. Reconcile the same slot
                    // before probing another; a process must never own two slots.
                    if let Some(held) = fence_if_owned(policy, store, owner, &key).await? {
                        return Ok(Some(held));
                    }
                    return Err(StoreError::new(
                        StoreErrorKind::WriteOutcomeUnknown {},
                        "conflicted claim may still commit",
                    )
                    .into());
                }
                Err(error) => {
                    if let Some(held) = fence_if_owned(policy, store, owner, &key).await? {
                        return Ok(Some(held));
                    }
                    return Err(error.into());
                }
            }
        }
    }
    Ok(None)
}
async fn fence_if_owned(
    policy: &Config,
    store: &Arc<dyn SamplingStore>,
    owner: &str,
    key: &str,
) -> Result<Option<HeldLease>, StoreError> {
    let Some(object) = store.get(key.to_string()).await? else {
        return Ok(None);
    };
    if parse::<Lease>(&object)?.owner.as_deref() != Some(owner) {
        return Ok(None);
    }
    let sent = Instant::now();
    let lease = Lease {
        owner: Some(owner.into()),
        nonce: nonce(),
    };
    match store
        .put(
            PutRequest::builder()
                .key(key)
                .body(serialize(&lease))
                .condition(WriteCondition::matching(object.version().clone()))
                .build(),
        )
        .await?
    {
        PutOutcome::Stored { .. } => Ok(Some(HeldLease {
            key: key.into(),
            valid_until: sent + policy.lease_duration - policy.max_clock_skew * 2,
        })),
        _ => Ok(None),
    }
}

#[derive(Default)]
struct MembershipScan {
    continuation: Option<ContinuationToken>,
    ranks: Vec<(u64, SystemTime)>,
}

async fn sparse_membership(
    policy: &Config,
    store: &Arc<dyn SamplingStore>,
    owner: &str,
    clock: &dyn Clock,
    scan: &mut MembershipScan,
    current_member: Option<&str>,
) -> Result<(Option<String>, Option<f64>), StoreError> {
    let digest = Sha256::digest(owner.as_bytes());
    let rank = u64::from_be_bytes(digest[..8].try_into().expect("digest length"));
    let prefix = format!("{}members/", policy.pool_prefix());
    let mut complete = false;
    let lifetime = policy
        .lease_duration
        .saturating_add(policy.max_clock_skew)
        .saturating_add(store.modification_time_uncertainty());
    scan.ranks.retain(|(_, modified)| {
        modified
            .checked_add(lifetime)
            .is_some_and(|end| end >= clock.now())
    });
    // Persist completed-page progress across refreshes and timeouts, so stale
    // low ranks cannot permanently hide the rest of the namespace.
    for _ in 0..8 {
        let page = store
            .list(
                ListRequest::builder()
                    .prefix(&prefix)
                    .page_size(policy.membership_sample_size)
                    .maybe_continuation(scan.continuation.clone())
                    .build(),
            )
            .await?;
        if page.continuation().is_some()
            && page.continuation().map(ContinuationToken::as_str)
                == scan.continuation.as_ref().map(ContinuationToken::as_str)
        {
            return Err(StoreError::new(
                StoreErrorKind::invalid_response(),
                "membership continuation did not advance",
            ));
        }
        let mut page_ranks = Vec::new();
        for entry in page.objects() {
            if entry
                .modified_at()
                .checked_add(lifetime)
                .is_some_and(|end| end >= clock.now())
            {
                let suffix = entry.key().strip_prefix(&prefix).ok_or_else(|| {
                    StoreError::new(
                        StoreErrorKind::InvalidResponse {},
                        "list returned key outside requested prefix",
                    )
                })?;
                let (rank, owner) = suffix.split_once('/').ok_or_else(|| {
                    StoreError::new(
                        StoreErrorKind::invalid_response(),
                        "malformed membership key",
                    )
                })?;
                if rank.len() != 16 || owner.is_empty() {
                    return Err(StoreError::new(
                        StoreErrorKind::invalid_response(),
                        "malformed membership key",
                    ));
                }
                let value = u64::from_str_radix(rank, 16)
                    .map_err(|error| StoreError::new(StoreErrorKind::invalid_response(), error))?;
                page_ranks.push((value, entry.modified_at()));
            }
        }
        scan.ranks.extend(page_ranks);
        scan.continuation = page.continuation().cloned();
        complete = scan.continuation.is_none();
        if scan.ranks.len() >= policy.membership_sample_size || complete {
            break;
        }
    }
    let finished = complete || scan.ranks.len() >= policy.membership_sample_size;
    let mut ranks: Vec<_> = scan.ranks.iter().map(|(rank, _)| *rank).collect();
    ranks.sort_unstable();
    ranks.truncate(policy.membership_sample_size);
    let cutoff = if ranks.len() < policy.membership_sample_size {
        u64::MAX
    } else {
        ranks[policy.membership_sample_size - 1]
    };
    let population = if !finished || ranks.len() < policy.membership_sample_size {
        // In a cold sparse pool the observed set is not yet the eligible fleet.
        None
    } else if cutoff > 0 {
        Some((policy.membership_sample_size - 1) as f64 / (cutoff as f64 / u64::MAX as f64))
    } else {
        None
    };
    if finished {
        *scan = MembershipScan::default();
    }
    if rank > cutoff {
        return Ok((None, population));
    }
    let key = format!("{prefix}{rank:016x}/{owner}");
    if current_member != Some(key.as_str()) {
        write_member(store, &key).await?;
    }
    Ok((Some(key), population))
}

async fn write_member(store: &Arc<dyn SamplingStore>, key: &str) -> Result<(), StoreError> {
    match store
        .put(
            PutRequest::builder()
                .key(key)
                .body(Bytes::from(nonce()))
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await?
    {
        PutOutcome::Stored { .. } => Ok(()),
        _ => Err(StoreError::new(
            StoreErrorKind::write_outcome_unknown(),
            "membership write not confirmed",
        )),
    }
}

struct Maintenance {
    policy: Config,
    store: Arc<dyn SamplingStore>,
    owner: String,
    held: Option<HeldLease>,
    snapshot: Arc<Mutex<SamplingSnapshot>>,
    stop: CancellationToken,
    recording: watch::Sender<bool>,
    started: Instant,
    clock: Arc<dyn Clock>,
    cleanup_budget: Arc<Mutex<Duration>>,
}
/// Dropping a panicked/aborted task must not leave strict-mode recording alive.
struct MaintenanceGuard {
    snapshot: Arc<Mutex<SamplingSnapshot>>,
    recording: watch::Sender<bool>,
    stop_on_loss: bool,
    armed: bool,
}
impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.coordinated = false;
        state.estimated_population = None;
        state.population_updated_at = None;
        state.membership_registered = false;
        if state.recording {
            state.reason = DecisionReason::LeaseLost {};
            if self.stop_on_loss {
                state.recording = false;
                self.recording.send_replace(false);
            }
        }
        tracing::error!("sampling maintenance task terminated unexpectedly");
    }
}
async fn maintain(state: Maintenance) {
    let Maintenance {
        policy,
        store,
        owner,
        mut held,
        snapshot,
        stop,
        recording,
        started,
        clock,
        cleanup_budget,
    } = state;
    let allocated_key = held.as_ref().map(|lease| lease.key.clone());
    let mut member: Option<String> = None;
    let mut scan = MembershipScan::default();
    let mut next_membership = started;
    let mut next_renewal = Instant::now() + policy.renewal_interval;
    loop {
        let lease_deadline = held.as_ref().map(|lease| lease.valid_until);
        let wake = next_membership
            .min(next_renewal)
            .min(lease_deadline.unwrap_or(next_membership));
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep_until(wake) => {}
        }
        if held
            .as_ref()
            .is_some_and(|lease| Instant::now() >= lease.valid_until)
        {
            held = None;
            let mut state = snapshot.lock().expect("snapshot lock");
            state.coordinated = false;
            if state.recording {
                state.reason = DecisionReason::LeaseLost {};
            }
            if matches!(policy.lease_loss, LeaseLoss::StopProfiling { .. }) {
                state.recording = false;
                recording.send_replace(false);
            }
        }
        if Instant::now() >= next_renewal {
            if let Some(ref mut lease) = held {
                // Keep shared configuration fresh before extending a lease.
                // If policy refresh fails, let the existing lease expire rather
                // than outliving policy expiration with incompatible settings.
                let policy_budget = DEFAULT_PREPARE_TIMEOUT
                    .min(lease.valid_until.saturating_duration_since(Instant::now()));
                let policy_ready = matches!(
                    tokio::time::timeout(
                        policy_budget,
                        pool_policy(&policy, &store, clock.as_ref()),
                    )
                    .await,
                    Ok(Ok(()))
                );
                let renewal = async {
                    if policy_ready {
                        fence_if_owned(&policy, &store, &owner, &lease.key).await
                    } else {
                        Err(StoreError::new(
                            StoreErrorKind::unavailable(),
                            "shared policy could not be refreshed",
                        ))
                    }
                };
                let budget = DEFAULT_PREPARE_TIMEOUT
                    .min(lease.valid_until.saturating_duration_since(Instant::now()));
                match tokio::time::timeout(budget, renewal).await {
                    Ok(Ok(Some(new_lease))) => *lease = new_lease,
                    Ok(Ok(None)) => lease.valid_until = Instant::now(),
                    _ => {
                        tracing::debug!(
                            "sampling renewal not confirmed; preserving current lease deadline"
                        );
                    }
                }
            }
            next_renewal = Instant::now() + policy.renewal_interval;
        }
        if Instant::now() >= next_membership {
            // Renew the previous unique member before a potentially long scan.
            // A partial scan must not expire its own participating process.
            let refresh = async {
                if let Some(key) = &member {
                    write_member(&store, key).await?;
                }
                sparse_membership(
                    &policy,
                    &store,
                    &owner,
                    clock.as_ref(),
                    &mut scan,
                    member.as_deref(),
                )
                .await
            };
            match tokio::time::timeout(DEFAULT_PREPARE_TIMEOUT, refresh).await {
                Ok(Ok((new_member, population))) => {
                    if new_member.is_none()
                        && let Some(old) = member.take()
                    {
                        cleanup_member(&store, old).await;
                    }
                    member = new_member;
                    let mut state = snapshot.lock().expect("snapshot lock");
                    state.estimated_population = population;
                    state.membership_registered = member.is_some();
                    state.population_updated_at = Some(clock.now());
                }
                result => {
                    if matches!(&result, Ok(Err(error)) if matches!(error.kind(), StoreErrorKind::InvalidResponse { .. }))
                    {
                        scan = MembershipScan::default();
                    }
                    let mut state = snapshot.lock().expect("snapshot lock");
                    state.estimated_population = None;
                    state.population_updated_at = None;
                    state.membership_registered = false;
                    tracing::debug!(?result, "sampling membership refresh unconfirmed");
                }
            }
            next_membership = Instant::now() + policy.renewal_interval;
        }
    }
    // Full follows only definitive failed claims; this process never owned a
    // slot. Timeout and uncertain writes still require owner-checked cleanup.
    let may_own_slot = !matches!(
        snapshot.lock().expect("snapshot lock").reason,
        DecisionReason::Full { .. }
    );
    if may_own_slot {
        let budget = *cleanup_budget.lock().expect("cleanup budget lock");
        match tokio::time::timeout(budget, async {
            match allocated_key {
                Some(key) => release_slot(&store, &owner, key).await,
                None => release_slots(&policy, &store, &owner).await,
            }
        })
        .await
        {
            Ok(Ok(())) => {}
            result => tracing::debug!(?result, "sampling release unconfirmed; allocation expires"),
        }
    }
    if let Some(member) = member {
        cleanup_member(&store, member).await;
    }
}
async fn release_slots(
    policy: &Config,
    store: &Arc<dyn SamplingStore>,
    owner: &str,
) -> Result<(), StoreError> {
    // Resolve ownership afresh: a cancelled renewal could have committed and
    // changed its ETag. Only tombstone an object still owned by this process.
    let prefix = format!("{}slots/", policy.pool_prefix());
    // An admission whose acknowledgement was lost may leave an allocation.
    // Cleanup scans only the bounded quota, never all fleet members.
    for slot in 0..policy.target() {
        let key = format!("{prefix}{slot:08}.json");
        release_slot(store, owner, key).await?;
    }
    Ok(())
}
async fn release_slot(
    store: &Arc<dyn SamplingStore>,
    owner: &str,
    key: String,
) -> Result<(), StoreError> {
    if let Some(object) = store.get(key.clone()).await?
        && parse::<Lease>(&object)?.owner.as_deref() == Some(owner)
    {
        store
            .put(
                PutRequest::builder()
                    .key(key)
                    .body(serialize(&Lease {
                        owner: None,
                        nonce: nonce(),
                    }))
                    .condition(WriteCondition::matching(object.version().clone()))
                    .build(),
            )
            .await?;
    }
    Ok(())
}
async fn cleanup_member(store: &Arc<dyn SamplingStore>, key: String) {
    match tokio::time::timeout(DEFAULT_PREPARE_TIMEOUT, store.delete(key)).await {
        Ok(Ok(())) => {}
        result => tracing::debug!(?result, "sampling membership cleanup unconfirmed"),
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    dial9_core::shuttle_test! {
        default;
        fn shuttle_concurrent_cas_claims_never_exceed_quota() {
            let store = Arc::new(crate::protocol::simulation::MemoryStore::new());
            let policy = Config::builder().scope("shuttle").exact_target(1).if_failed(StartupFailure::disabled())
            .build();
            let threads: Vec<_> = (0..3)
                .map(|index| {
                    let store: Arc<dyn SamplingStore> = store.clone();
                    let policy = policy.clone();
                    shuttle::thread::spawn(move || {
                        shuttle::future::block_on(admit(
                            &policy,
                            &store,
                            &format!("owner-{index}"),
                            &WallClock,
                        ))
                        .unwrap()
                        .is_some()
                    })
                })
                .collect();
            let admitted = threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|admitted| *admitted)
                .count();
            assert_eq!(admitted, 1);
        }
    }
}
#[cfg(all(test, shuttle))]
mod adversarial_shuttle {
    use super::*;
    fn scenario(contenders: usize, pools: usize, lost_ack: bool) {
        let inner = Arc::new(crate::protocol::simulation::MemoryStore::new());
        let store: Arc<dyn SamplingStore> = inner.clone();
        let policies: Vec<_> = (0..pools)
            .map(|group| {
                Config::builder()
                    .scope("shuttle-multi-dimension")
                    .exact_target(1)
                    .add_dimension("az", format!("az-{group}"))
                    .add_dimension("instance-type", "large")
                    .if_failed(StartupFailure::disabled())
                    .build()
            })
            .collect();
        for policy in &policies {
            shuttle::future::block_on(pool_policy(policy, &store, &WallClock)).unwrap();
        }
        if lost_ack {
            inner.lose_write_acknowledgements(1);
        }
        let threads: Vec<_> = (0..contenders)
            .map(|index| {
                let policy = policies[index % pools].clone();
                let store = store.clone();
                shuttle::thread::spawn(move || {
                    (
                        index % pools,
                        shuttle::future::block_on(admit(
                            &policy,
                            &store,
                            &format!("node-{index}"),
                            &WallClock,
                        ))
                        .unwrap()
                        .is_some(),
                    )
                })
            })
            .collect();
        let mut selected = vec![0; pools];
        for t in threads {
            let (group, enabled) = t.join().unwrap();
            selected[group] += usize::from(enabled);
        }
        assert_eq!(selected, vec![1; pools]);
        let objects = inner.objects();
        let mut owned = std::collections::BTreeMap::<String, usize>::new();
        for (key, value) in objects {
            if key.contains("/slots/") {
                if let Some(owner) = parse::<Lease>(&value).unwrap().owner {
                    *owned.entry(owner).or_default() += 1;
                }
            }
        }
        assert!(
            owned.values().all(|&n| n == 1),
            "uncertain outcome allocated two slots to one process"
        );
        assert_eq!(owned.len(), pools);
    }
    dial9_core::shuttle_test! {
        default;
        fn shuttle_sixteen_simultaneous_claimants_target_one() {
            scenario(16, 1, false)
        }
    }
    dial9_core::shuttle_test! {
        default;
        fn shuttle_twelve_claimants_three_dimension_tuples() {
            scenario(12, 3, false)
        }
    }
    dial9_core::shuttle_test! {
        default;
        fn shuttle_twelve_claimants_with_lost_write_ack() {
            scenario(12, 1, true)
        }
    }

    #[derive(Debug)]
    struct ExpiredSeed(Arc<crate::protocol::simulation::MemoryStore>);
    impl SamplingStore for ExpiredSeed {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            Box::pin(async move {
                let slot = key.contains("/slots/");
                let Some(object) = self.0.get(key).await? else {
                    return Ok(None);
                };
                if slot && parse::<Lease>(&object)?.nonce == "seed" {
                    return Ok(Some(
                        StoredObject::builder()
                            .body(object.body().clone())
                            .version(object.version().clone())
                            .modified_at(std::time::UNIX_EPOCH)
                            .build(),
                    ));
                }
                Ok(Some(object))
            })
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            self.0.put(request)
        }
        fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.0.list(request)
        }
        fn delete(&self, key: String) -> StoreFuture<'_, ()> {
            self.0.delete(key)
        }
    }
    fn expired_seed() -> (Config, Arc<dyn SamplingStore>, String) {
        let store: Arc<dyn SamplingStore> = Arc::new(ExpiredSeed(Arc::new(
            crate::protocol::simulation::MemoryStore::new(),
        )));
        let policy = Config::builder()
            .scope("replacement")
            .exact_target(1)
            .if_failed(StartupFailure::disabled())
            .build();
        shuttle::future::block_on(pool_policy(&policy, &store, &WallClock)).unwrap();
        let key = format!("{}slots/00000000.json", policy.pool_prefix());
        shuttle::future::block_on(
            store.put(
                PutRequest::builder()
                    .key(&key)
                    .body(serialize(&Lease {
                        owner: Some("old".into()),
                        nonce: "seed".into(),
                    }))
                    .build(),
            ),
        )
        .unwrap();
        (policy, store, key)
    }
    dial9_core::shuttle_test! {
        default;
        fn shuttle_release_cannot_delete_concurrent_replacement() {
            let (policy, store, key) = expired_seed();
            let release_policy = policy.clone();
            let release_store = store.clone();
            let release = shuttle::thread::spawn(move || {
                shuttle::future::block_on(release_slots(
                    &release_policy,
                    &release_store,
                    "old",
                ))
                .unwrap();
            });
            let admit_store = store.clone();
            let claim = shuttle::thread::spawn(move || {
                shuttle::future::block_on(admit(&policy, &admit_store, "new", &WallClock))
                    .unwrap()
                    .is_some()
            });
            release.join().unwrap();
            let admitted = claim.join().unwrap();
            let object = shuttle::future::block_on(store.get(key)).unwrap().unwrap();
            let owner = parse::<Lease>(&object).unwrap().owner;
            assert!(
                admitted,
                "a definitive release conflict must re-read the vacant slot"
            );
            assert_eq!(owner.as_deref(), Some("new"));
            assert_ne!(owner.as_deref(), Some("old"));
        }
    }
    dial9_core::shuttle_test! {
        default;
        fn shuttle_delayed_renewal_and_replacement_cannot_both_win() {
            let (policy, store, key) = expired_seed();
            let renew_policy = policy.clone();
            let renew_store = store.clone();
            let renew_key = key.clone();
            let renew = shuttle::thread::spawn(move || {
                shuttle::future::block_on(fence_if_owned(
                    &renew_policy,
                    &renew_store,
                    "old",
                    &renew_key,
                ))
                .unwrap()
                .is_some()
            });
            let admit_store = store.clone();
            let claim = shuttle::thread::spawn(move || {
                shuttle::future::block_on(admit(&policy, &admit_store, "new", &WallClock))
                    .unwrap()
                    .is_some()
            });
            let renewed = renew.join().unwrap();
            let admitted = claim.join().unwrap();
            assert!(!(renewed && admitted));
            let object = shuttle::future::block_on(store.get(key)).unwrap().unwrap();
            let owner = parse::<Lease>(&object).unwrap().owner;
            if admitted {
                assert_eq!(owner.as_deref(), Some("new"));
            }
            if renewed {
                assert_eq!(owner.as_deref(), Some("old"));
            }
        }
    }
}

#[cfg(all(test, not(shuttle)))]
mod precision_tests {
    use super::*;
    #[derive(Debug)]
    struct SecondsStore(Arc<crate::protocol::simulation::MemoryStore>);
    impl SamplingStore for SecondsStore {
        fn modification_time_uncertainty(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            Box::pin(async move {
                Ok(self.0.get(key).await?.map(|object| {
                    StoredObject::builder()
                        .body(object.body().clone())
                        .version(object.version().clone())
                        .modified_at(std::time::UNIX_EPOCH + Duration::from_secs(10000))
                        .build()
                }))
            })
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            self.0.put(request)
        }
        fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.0.list(request)
        }
        fn delete(&self, key: String) -> StoreFuture<'_, ()> {
            self.0.delete(key)
        }
    }
    struct FractionalClock(Instant);
    impl Clock for FractionalClock {
        fn now(&self) -> SystemTime {
            std::time::UNIX_EPOCH + Duration::from_millis(10000700) + self.0.elapsed()
        }
    }
    #[tokio::test(start_paused = true)]
    async fn timestamp_rounding_cannot_reuse_a_still_valid_strict_lease() {
        let store: Arc<dyn SamplingStore> = Arc::new(SecondsStore(Arc::new(
            crate::protocol::simulation::MemoryStore::new(),
        )));
        let policy = Config::builder()
            .scope("rounded-time")
            .exact_target(1)
            .max_clock_skew(Duration::ZERO)
            .lease_loss(LeaseLoss::stop_profiling())
            .if_failed(StartupFailure::disabled())
            .build();
        let clock = FractionalClock(Instant::now());
        let original = admit(&policy, &store, "old", &clock)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(299500)).await;
        assert!(Instant::now() < original.valid_until);
        assert!(
            admit(&policy, &store, "new", &clock)
                .await
                .unwrap()
                .is_none()
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(Instant::now() > original.valid_until);
        assert!(
            admit(&policy, &store, "new", &clock)
                .await
                .unwrap()
                .is_some()
        );
    }
}
