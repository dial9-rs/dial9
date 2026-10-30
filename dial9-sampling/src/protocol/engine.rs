#[path = "observation.rs"]
mod observation;
use super::census::{self, Cutoff, SPACE};
use super::{Config, rate_limited};
use crate::backend::{
    PutOutcome, PutRequest, SamplingStore, StoreError, StoreErrorKind, StoredObject, VersionToken,
    WriteCondition,
};
use bytes::Bytes;
use observation::ObservationDriver;
use serde::{Deserialize, Serialize};
use std::{
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
    signal: watch::Sender<bool>,
    reports: Arc<observation::JobTracker>,
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
        if let Some(weights) = &self.weights {
            weights.stop();
        }
        self.signal.send_replace(false);
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "sampling maintenance failed"),
                Err(_) => {
                    task.abort();
                    if let Err(error) = task.await
                        && !error.is_cancelled()
                    {
                        tracing::error!(%error, "sampling supervisor failed after cancellation");
                    }
                    tracing::debug!("sampling cleanup deadline expired");
                }
            }
        }
        self.reports.abort_and_join().await;
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

const SCHEMA: u32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Policy {
    schema: u32,
    pool: String,
    target: u32,
    max_overshoot: u32,
    poll_ms: u64,
    window_ms: u64,
    skew_ms: u64,
}
impl Policy {
    fn new(config: &Config) -> Self {
        Self {
            schema: SCHEMA,
            pool: config.pool_path(),
            target: config.target(),
            max_overshoot: config.overshoot(),
            poll_ms: config.settings.poll_interval.as_millis() as u64,
            window_ms: config.settings.bucket_width.as_millis() as u64,
            skew_ms: config.settings.max_clock_skew.as_millis() as u64,
        }
    }
}
// Margins derive from the private window setting, including in paused-time tests.
// Defaults: 3s operation, 2s dispatch, end+11s write, end+22s collection.
struct Timing {
    width: u64,
    poll: u64,
    operation: Duration,
    dispatch: u64,
    submission: u64,
    closure: u64,
    age: u64,
}
impl Timing {
    fn new(config: &Config) -> Self {
        let width = config.settings.bucket_width.as_millis() as u64;
        let op = width / 20;
        let dispatch = width / 30;
        let submission = dispatch + 3 * op;
        Self {
            width,
            poll: width * 5,
            operation: Duration::from_millis(op),
            dispatch,
            submission,
            closure: submission
                + 2 * config.settings.max_clock_skew.as_millis() as u64
                + width / 60,
            age: width * 10,
        }
    }
    fn end(&self, window: u64) -> Option<u64> {
        window.checked_add(1)?.checked_mul(self.width)
    }
    fn completed(&self, now: u64) -> Option<u64> {
        now.checked_sub(self.closure)?
            .checked_div(self.width)?
            .checked_sub(1)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Control {
    source_window: Option<u64>,
    effective_window: u64,
    reporting: Cutoff,
}
impl Control {
    fn bootstrap() -> Self {
        Self {
            source_window: None,
            effective_window: 0,
            reporting: Cutoff(None),
        }
    }
    fn valid(&self) -> bool {
        self.reporting.value() > 0 && self.source_window.is_none_or(|w| w < self.effective_window)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Population {
    window: u64,
    estimate: f64,
    computed_ms: u64,
}
impl Population {
    fn fresh(&self, now: u64, config: &Config) -> bool {
        let timing = Timing::new(config);
        timing.end(self.window).is_some_and(|end| {
            self.computed_ms >= end
                && self.computed_ms
                    <= now.saturating_add(config.settings.max_clock_skew.as_millis() as u64 * 2)
                && now <= end.saturating_add(timing.age)
        })
    }
    fn admission(&self, target: u32) -> Cutoff {
        Cutoff::threshold(
            Cutoff::from_probability(f64::from(target) / self.estimate)
                .value()
                .max(u128::from(target > 0)),
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Trim {
    epoch: u64,
    salt: String,
    source_window: u64,
    effective_window: u64,
    created_ms: u64,
    survival: Cutoff,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Roster {
    policy: Policy,
    holder: String,
    // Every mutation includes fresh entropy so a body-based ETag cannot suffer ABA.
    revision: String,
    epoch: u64,
    control: Control,
    next_control: Option<Control>,
    population: Option<Population>,
    trim: Option<Trim>,
    // Reserve each completed source before LIST. Takeover never repeats that work.
    last_window: Option<u64>,
}
impl Roster {
    fn initial(config: &Config, invocation_id: &str) -> Self {
        Self {
            policy: Policy::new(config),
            holder: invocation_id.into(),
            revision: nonce(),
            epoch: 0,
            control: Control::bootstrap(),
            next_control: None,
            population: None,
            trim: None,
            last_window: None,
        }
    }
    fn validate(&self, config: &Config) -> Result<(), StoreError> {
        if self.policy != Policy::new(config) {
            return Err(StoreError::new(
                StoreErrorKind::unsupported(),
                "incompatible sampling roster policy",
            ));
        }
        if self.holder.parse::<ulid::Ulid>().is_err()
            || self.revision.parse::<ulid::Ulid>().is_err()
            || !self.control.valid()
            || self
                .next_control
                .as_ref()
                .is_some_and(|c| !c.valid() || c.effective_window <= self.control.effective_window)
            || self
                .population
                .as_ref()
                .is_some_and(|p| !p.estimate.is_finite() || p.estimate <= 0.0)
            || self.trim.as_ref().is_some_and(|t| {
                t.epoch > self.epoch
                    || t.salt.parse::<ulid::Ulid>().is_err()
                    || t.source_window >= t.effective_window
                    || t.survival.value() == 0
            })
        {
            return Err(invalid("invalid sampling roster fields"));
        }
        Ok(())
    }
    fn admission(&self, config: &Config, now: u64) -> Option<Cutoff> {
        self.population
            .as_ref()
            .filter(|p| p.fresh(now, config))
            .map(|p| p.admission(config.target()))
    }
    fn applicable(&self, window: u64) -> Option<&Control> {
        self.next_control
            .as_ref()
            .filter(|c| c.effective_window <= window)
            .or_else(|| (self.control.effective_window <= window).then_some(&self.control))
    }
    fn promote(&mut self, window: u64) {
        if self
            .next_control
            .as_ref()
            .is_some_and(|c| c.effective_window <= window)
        {
            self.control = self.next_control.take().expect("present future control");
        }
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
    #[derive(Deserialize)]
    struct PolicySchema {
        schema: u32,
    }
    #[derive(Deserialize)]
    struct Header {
        policy: PolicySchema,
    }
    // Another schema may have an entirely different body and policy layout.
    let header: Header = serde_json::from_slice(object.body()).map_err(invalid)?;
    if header.policy.schema != SCHEMA {
        return Err(StoreError::new(
            StoreErrorKind::unsupported(),
            "incompatible sampling roster schema",
        ));
    }
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
    // Server LastModified determines lease age. Precision/skew margins only delay takeover.
    observed
        .modified_at
        .checked_add(config.settings.poll_interval.saturating_mul(3))
        .and_then(|t| t.checked_add(config.settings.max_clock_skew))
        .and_then(|t| t.checked_add(store.modification_time_uncertainty()))
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
    let mut startup_probability = config.failure_cutoff() as f64 / SPACE as f64;
    let result = if timeout.is_zero() {
        None
    } else {
        Some(
            tokio::time::timeout_at(
                deadline,
                store.get(format!("{}roster.json", config.pool_prefix())),
            )
            .await,
        )
    };
    let (recording, reason, observed) = match result {
        None | Some(Err(_)) => (fallback, DecisionReason::Timeout, None),
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
                        let enabled = config.selects(cutoff.value());
                        (
                            enabled,
                            if enabled {
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
        return PreparedSampling::disabled(reason, recording);
    }
    let authoritative = matches!(
        reason,
        DecisionReason::MissingRoster
            | DecisionReason::Roster
            | DecisionReason::OutsideCutoff
            | DecisionReason::UnsettledRoster
    );
    let weights = recording.then(|| {
        crate::trace_weights::TraceWeights::new(config.pool_path(), 1.0 / startup_probability)
    });
    let (signal, recording_changes) = watch::channel(recording);
    let stop = CancellationToken::new();
    let mut worker = match Worker::new(config, store, clock, observed, signal, authoritative) {
        Ok(worker) => worker,
        Err(error) => {
            return PreparedSampling::disabled(
                DecisionReason::store_failure(error.to_string()),
                fallback,
            );
        }
    };
    let signal = worker.signal.clone();
    let reports = Arc::new(observation::JobTracker::default());
    let mut driver = match ObservationDriver::new(
        worker.config.clone(),
        worker.store.clone(),
        worker.clock.clone(),
        worker.cache.clone(),
        signal.clone(),
        weights.clone(),
        reports.clone(),
    ) {
        Ok(driver) => driver,
        Err(error) => {
            return PreparedSampling::disabled(
                DecisionReason::store_failure(error.to_string()),
                fallback,
            );
        }
    };
    let terminal_weights = weights.clone();
    let terminal_signal = signal.clone();
    let cancellation = stop.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {},
            result = catch_panic(worker.run()) => { if let Err(error) = result { tracing::error!(%error, "sampling controller stopped"); } },
            result = catch_panic(driver.run()) => { match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) | Err(error) => tracing::error!(%error, "sampling observation driver stopped"),
            } },
        }
        if let Some(weights) = terminal_weights {
            weights.stop();
        }
        terminal_signal.send_replace(false);
        driver.shutdown().await;
        // Immutable historical reports expire through storage lifecycle policy.
    });
    PreparedSampling {
        enabled: recording,
        reason,
        session: Some(SamplingSession {
            stop,
            task: Some(task),
            recording: recording_changes,
            signal,
            reports,
            startup_probability,
            weights,
        }),
    }
}

// Catch a maintenance panic while retaining the driver so its report jobs can
// still be aborted and joined before the callback-owned session terminates.
async fn catch_panic<T>(future: impl std::future::Future<Output = T>) -> Result<T, StoreError> {
    let mut future = Box::pin(future);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Err(_) => std::task::Poll::Ready(Err(invalid("sampling maintenance panicked"))),
        }
    })
    .await
}

#[derive(Clone)]
struct Cache {
    observed: Option<Observed>,
    compatible: bool,
}
fn publish_cache(cache: &watch::Sender<Cache>, incoming: Option<Observed>, compatible: bool) {
    cache.send_if_modified(|current| {
        if !compatible {
            current.compatible = false;
            return true;
        }
        // A successful incompatible response is terminal for joining in this run.
        // An older compatible GET may still complete after that response.
        if !current.compatible {
            return false;
        }
        if let Some(incoming) = &incoming
            && let Some(old) = &current.observed
            && (incoming.roster.epoch < old.roster.epoch
                || (incoming.roster.epoch == old.roster.epoch
                    && incoming.roster.revision != old.roster.revision))
        {
            return false;
        }
        // An earlier successful missing read cannot erase a later immutable roster.
        if incoming.is_some() || current.observed.is_none() {
            current.observed = incoming;
        }
        true
    });
}
struct Worker {
    config: Config,
    store: Arc<dyn SamplingStore>,
    clock: Arc<dyn Clock>,
    observed: Option<Observed>,
    leader: Option<Observed>,
    signal: watch::Sender<bool>,
    cache: watch::Sender<Cache>,
    next_pass_ms: u64,
    compatible: bool,
    election_needed: bool,
}
impl Worker {
    fn new(
        config: Config,
        store: Arc<dyn SamplingStore>,
        clock: Arc<dyn Clock>,
        observed: Option<Observed>,
        signal: watch::Sender<bool>,
        authoritative: bool,
    ) -> Result<Self, StoreError> {
        let born_ms = millis(clock.now())?;
        let election_needed = authoritative
            && observed
                .as_ref()
                .is_none_or(|o| expired(o, &config, store.as_ref(), clock.now()));
        let cache = watch::channel(Cache {
            observed: observed.clone(),
            compatible: true,
        })
        .0;
        Ok(Self {
            config,
            store,
            clock,
            observed,
            leader: None,
            signal,
            cache,
            next_pass_ms: born_ms,
            compatible: true,
            election_needed,
        })
    }
    fn now(&self) -> Result<u64, StoreError> {
        millis(self.clock.now())
    }
    fn learn(&mut self, observed: Option<Observed>, _now: u64) {
        publish_cache(&self.cache, observed, true);
        self.consume_cache();
    }
    fn consume_cache(&mut self) {
        let state = self.cache.borrow().clone();
        self.compatible = state.compatible;
        if !self.compatible {
            self.leader = None;
            self.election_needed = false;
            return;
        }
        let observed = state.observed;
        if self.leader.as_ref().is_some_and(|leader| {
            observed.as_ref().is_none_or(|current| {
                current.version != leader.version
                    || current.roster.holder != self.config.invocation_id
            })
        }) {
            self.leader = None;
        }
        if self.leader.is_none()
            && let Some(current) = &observed
            && current.roster.holder == self.config.invocation_id
            && !expired(current, &self.config, self.store.as_ref(), self.clock.now())
        {
            self.leader = Some(current.clone());
        }
        self.election_needed = observed
            .as_ref()
            .is_none_or(|o| expired(o, &self.config, self.store.as_ref(), self.clock.now()));
        self.observed = observed;
    }
    async fn read_roster(&self) -> Result<Option<Observed>, StoreError> {
        let result = bounded(
            Timing::new(&self.config).operation,
            self.store
                .get(format!("{}roster.json", self.config.pool_prefix())),
        )
        .await
        .and_then(|object| object.map(|o| decode(o, &self.config)).transpose());
        if result
            .as_ref()
            .is_err_and(|error| matches!(error.kind(), StoreErrorKind::Unsupported {}))
        {
            publish_cache(&self.cache, None, false);
        }
        result
    }
    async fn replace(
        &self,
        roster: &Roster,
        previous: Option<&VersionToken>,
    ) -> Result<Option<Observed>, StoreError> {
        let result = replace(
            &self.config,
            self.store.as_ref(),
            self.clock.as_ref(),
            roster,
            previous,
        )
        .await;
        if result
            .as_ref()
            .is_err_and(|error| matches!(error.kind(), StoreErrorKind::Unsupported {}))
        {
            publish_cache(&self.cache, None, false);
        }
        result
    }
    async fn election(&mut self) -> Result<(), StoreError> {
        if !self.election_needed || !self.compatible {
            return Ok(());
        }
        self.election_needed = false;
        let span = (self.config.settings.poll_interval.as_millis() / 2).min(2000);
        let delay = u128::from(self.config.rank()) * span / SPACE;
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        // One authoritative re-read after backoff, then one CAS. No immediate retry loop.
        let observed = self.read_roster().await?;
        if observed
            .as_ref()
            .is_some_and(|o| !expired(o, &self.config, self.store.as_ref(), self.clock.now()))
        {
            self.learn(observed, self.now()?);
            return Ok(());
        }
        let mut roster = observed.as_ref().map_or_else(
            || Roster::initial(&self.config, &self.config.invocation_id),
            |o| o.roster.clone(),
        );
        roster.mutate(&self.config.invocation_id)?;
        let claimed = self
            .replace(&roster, observed.as_ref().map(|o| &o.version))
            .await?;
        self.leader = claimed.clone();
        if let Some(claimed) = claimed {
            self.learn(Some(claimed), self.now()?);
            self.next_pass_ms = self.now()?;
        }
        Ok(())
    }
    async fn leader_pass(&mut self) -> Result<(), StoreError> {
        bounded(self.config.settings.poll_interval, self.leader_pass_inner()).await
    }
    async fn leader_pass_inner(&mut self) -> Result<(), StoreError> {
        let Some(previous) = self.leader.take() else {
            return Ok(());
        };
        // A GET reconciles uncertain CAS and fences every publication on this same object.
        let Some(current) = self.read_roster().await? else {
            return Ok(());
        };
        if current.version != previous.version || current.roster.holder != self.config.invocation_id
        {
            self.learn(Some(current), self.now()?);
            return Ok(());
        }
        let now = self.now()?;
        let timing = Timing::new(&self.config);
        let window = timing.completed(now);
        let mut roster = current.roster.clone();
        roster.promote(now / timing.width);
        roster.mutate(&self.config.invocation_id)?;
        let assess = window.filter(|window| roster.last_window.is_none_or(|last| *window > last));
        if let Some(window) = assess {
            roster.last_window = Some(window);
        }
        let Some(reserved) = self.replace(&roster, Some(&current.version)).await? else {
            return Ok(());
        };
        self.leader = Some(reserved.clone());
        self.learn(Some(reserved.clone()), now);
        let Some(window) = assess else {
            return Ok(());
        };
        // Each channel owns its own finite page budget, including when its peer fails.
        let (census, profilers) = tokio::join!(
            census::observe(
                self.store.as_ref(),
                &self.config,
                "census",
                window,
                self.config.target() as usize * 32,
                timing.operation
            ),
            census::observe(
                self.store.as_ref(),
                &self.config,
                "profilers",
                window,
                (self.config.target() as usize * 4).max(64),
                timing.operation
            ),
        );
        let finished = self.now()?;
        let mut result = reserved.roster.clone();
        result.promote(finished / timing.width);
        result.mutate(&self.config.invocation_id)?;
        update(
            &mut result,
            &self.config,
            window,
            finished,
            census,
            profilers,
        );
        let published = self.replace(&result, Some(&reserved.version)).await?;
        self.leader = published.clone();
        if let Some(published) = published {
            self.learn(Some(published), finished);
        }
        Ok(())
    }
    async fn run(&mut self) {
        if let Err(error) = self.run_checked().await {
            tracing::error!(%error, "sampling maintenance stopped after invalid clock");
        }
    }
    async fn run_checked(&mut self) -> Result<(), StoreError> {
        let mut changes = self.cache.subscribe();
        loop {
            self.now()?;
            if changes
                .has_changed()
                .map_err(|_| invalid("sampling control cache closed"))?
            {
                changes.borrow_and_update();
                self.consume_cache();
            }
            if let Err(error) = self.election().await {
                self.leader = None;
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(%error, "sampling election failed");
                });
            }
            if self.leader.is_some() && self.now()? >= self.next_pass_ms {
                if let Err(error) = self.leader_pass().await {
                    self.leader = None;
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(%error, "sampling leader round failed");
                    });
                }
                let timing = Timing::new(&self.config);
                let now = self.now()?;
                let close = now / timing.width * timing.width + timing.closure;
                self.next_pass_ms = if close > now {
                    close
                } else {
                    close.saturating_add(timing.width)
                };
                self.next_pass_ms = self
                    .next_pass_ms
                    .min(now.saturating_add(self.config.settings.poll_interval.as_millis() as u64));
            }
            let now = self.now()?;
            let next = if self.leader.is_some() {
                self.next_pass_ms
            } else {
                now.saturating_add(Timing::new(&self.config).poll)
            };
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(next.saturating_sub(now).max(1))) => {},
                result = changes.changed() => { result.map_err(|_| invalid("sampling control cache closed"))?; self.consume_cache(); },
            }
        }
    }
}

fn next_slot(now: u64, period: u64, phase: u64) -> Result<u64, StoreError> {
    let slot = (now / period)
        .checked_mul(period)
        .and_then(|base| base.checked_add(phase % period))
        .ok_or_else(|| invalid("poll time overflow"))?;
    if slot >= now {
        Ok(slot)
    } else {
        slot.checked_add(period)
            .ok_or_else(|| invalid("poll time overflow"))
    }
}

async fn replace(
    config: &Config,
    store: &dyn SamplingStore,
    clock: &dyn Clock,
    roster: &Roster,
    previous: Option<&VersionToken>,
) -> Result<Option<Observed>, StoreError> {
    let key = format!("{}roster.json", config.pool_prefix());
    let condition = previous.map_or_else(WriteCondition::absent, |v| {
        WriteCondition::matching(v.clone())
    });
    let timeout = Timing::new(config).operation;
    match bounded(
        timeout,
        store.put(
            PutRequest::builder()
                .key(&key)
                .body(serialize(roster))
                .condition(condition)
                .build(),
        ),
    )
    .await
    {
        Ok(PutOutcome::Stored { version }) => Ok(Some(Observed {
            roster: roster.clone(),
            version,
            modified_at: clock.now(),
        })),
        Ok(PutOutcome::PreconditionFailed {}) => Ok(None),
        Err(error) => {
            if !matches!(
                error.kind(),
                StoreErrorKind::WriteOutcomeUnknown { .. } | StoreErrorKind::Unavailable { .. }
            ) {
                return Err(error);
            }
            rate_limited!(Duration::from_secs(60), {
                tracing::debug!(%error, "reconciling uncertain roster write");
            });
            // Cancelled writes may still commit. One readback reconciles the exact fresh revision.
            match bounded(timeout, store.get(key)).await {
                Ok(Some(object)) => {
                    let observed = decode(object, config)?;
                    if observed.roster.revision == roster.revision
                        && observed.roster.holder == config.invocation_id
                    {
                        Ok(Some(observed))
                    } else {
                        Ok(None)
                    }
                }
                Ok(None) => Err(error),
                Err(read_error) => Err(read_error),
            }
        }
    }
}
fn update(
    roster: &mut Roster,
    config: &Config,
    window: u64,
    now: u64,
    census: Result<census::Sample, StoreError>,
    profilers: Result<census::Sample, StoreError>,
) {
    let timing = Timing::new(config);
    if let Ok(sample) = census {
        let budget = config.target() as usize * 32;
        let nominal = (sample.estimate > 0.0)
            .then(|| Cutoff::from_probability(budget as f64 / sample.estimate));
        let next = if sample.retained < budget / 2 {
            Cutoff::threshold(
                (roster.control.reporting.value() * 2)
                    .min(SPACE)
                    .max(nominal.as_ref().map_or(1, Cutoff::value)),
            )
        } else {
            nominal.unwrap_or_else(|| roster.control.reporting.clone())
        };
        roster.next_control = Some(Control {
            source_window: Some(window),
            effective_window: now / timing.width + 1,
            reporting: Cutoff::threshold(next.value().max(1)),
        });
        if sample.estimate > 0.0 {
            roster.population = Some(Population {
                window,
                estimate: sample.estimate,
                computed_ms: now,
            });
        }
    } else if let Err(error) = census {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(%error, window, "unusable completed population census");
        });
    }
    match profilers {
        Ok(sample)
            if sample.estimate > f64::from(config.target().saturating_add(config.overshoot()))
                && roster
                    .trim
                    .as_ref()
                    .is_none_or(|t| window >= t.effective_window.saturating_add(1)) =>
        {
            let lead = timing.width
                + 2 * config.settings.max_clock_skew.as_millis() as u64
                + 3 * timing.operation.as_millis() as u64
                + timing.dispatch;
            roster.trim = Some(Trim {
                epoch: roster.epoch,
                salt: nonce(),
                source_window: window,
                effective_window: now.saturating_add(lead) / timing.width + 1,
                created_ms: now,
                survival: Cutoff::threshold(
                    Cutoff::from_probability(f64::from(config.target()) / sample.estimate)
                        .value()
                        .max(1),
                ),
            });
        }
        Ok(_) => {}
        Err(error) => {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(%error, window, "unusable completed profiler census");
            });
        }
    }
}
pub(super) async fn bounded<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T, StoreError>>,
) -> Result<T, StoreError> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        StoreError::new(
            StoreErrorKind::unavailable(),
            "sampling operation deadline expired",
        )
    })?
}
#[cfg(all(test, not(shuttle)))]
#[path = "tests.rs"]
mod tests;
