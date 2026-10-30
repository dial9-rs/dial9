//! Observation and trim timers run independently of control and leader RPCs.
use super::*;
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, Id, JoinSet};

#[derive(Debug, Default)]
pub(super) struct JobTracker(Mutex<HashMap<Id, (AbortHandle, oneshot::Receiver<()>)>>);
impl JobTracker {
    pub(super) async fn abort_and_join(&self) {
        let jobs = std::mem::take(&mut *self.0.lock().expect("sampling job tracker"));
        for (abort, _) in jobs.values() {
            abort.abort();
        }
        for (_, complete) in jobs.into_values() {
            if complete.await.is_err() {
                tracing::trace!("sampling job completion producer closed");
            }
        }
    }
}
struct Completion(Option<oneshot::Sender<()>>);
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take()
            && sender.send(()).is_err()
        {
            tracing::trace!("sampling job completion listener closed");
        }
    }
}

struct Plan {
    window: u64,
    cutoff: Cutoff,
    census_rank: u64,
    census_at: u64,
    profiler_rank: u64,
    profiler_at: u64,
    census_done: bool,
    profiler_done: bool,
}
pub(super) struct ObservationDriver {
    config: Config,
    store: Arc<dyn SamplingStore>,
    clock: Arc<dyn Clock>,
    cache: watch::Sender<Cache>,
    signal: watch::Sender<bool>,
    weights: Option<crate::trace_weights::TraceWeights>,
    born_ms: u64,
    cutoff: Cutoff,
    plan: Option<Plan>,
    last_trim_epoch: u64,
    next_poll_ms: u64,
    poll_phase_ms: u64,
    poll_period_ms: u64,
    tracker: Arc<JobTracker>,
    jobs: JoinSet<Result<(), StoreError>>,
    in_flight: HashMap<Id, (u64, AbortHandle, bool)>,
}
impl ObservationDriver {
    pub(super) fn new(
        config: Config,
        store: Arc<dyn SamplingStore>,
        clock: Arc<dyn Clock>,
        cache: watch::Sender<Cache>,
        signal: watch::Sender<bool>,
        weights: Option<crate::trace_weights::TraceWeights>,
        tracker: Arc<JobTracker>,
    ) -> Result<Self, StoreError> {
        let born_ms = millis(clock.now())?;
        let window = born_ms / Timing::new(&config).width;
        let state = cache.borrow().clone();
        let cutoff = state
            .observed
            .as_ref()
            .and_then(|o| o.roster.applicable(window))
            .map_or(Cutoff(None), |c| c.reporting.clone());
        let last_trim_epoch = state.observed.as_ref().map_or(0, |o| o.roster.epoch);
        let timing = Timing::new(&config);
        let poll_phase_ms = config.draw("roster-phase", "") % timing.poll;
        let poll_period_ms = if *signal.borrow() {
            timing.width
        } else {
            timing.poll
        };
        let next_poll_ms = next_slot(
            born_ms
                .checked_add(1)
                .ok_or_else(|| invalid("poll time overflow"))?,
            poll_period_ms,
            poll_phase_ms,
        )?;
        Ok(Self {
            config,
            store,
            clock,
            cache,
            signal,
            weights,
            born_ms,
            cutoff,
            plan: None,
            last_trim_epoch,
            next_poll_ms,
            poll_phase_ms,
            poll_period_ms,
            tracker,
            jobs: JoinSet::new(),
            in_flight: HashMap::new(),
        })
    }
    fn boundary(&mut self, now: u64) -> Result<(), StoreError> {
        let timing = Timing::new(&self.config);
        let window = now / timing.width;
        if self.plan.as_ref().is_some_and(|p| p.window >= window) {
            return Ok(());
        }
        let state = self.cache.borrow().clone();
        if state.compatible()
            && let Some(observed) = &state.observed
        {
            if let Some(control) = observed.roster.applicable(window)
                && control.source_window.is_none_or(|source| source < window)
            {
                self.cutoff = control.reporting.clone();
            }
            if let Some(trim) = &observed.roster.trim
                && trim.epoch > self.last_trim_epoch
                && trim.effective_window <= window
            {
                self.last_trim_epoch = trim.epoch;
                if trim.effective_window == window
                    && self.born_ms / timing.width <= trim.source_window
                    && now <= window * timing.width + timing.dispatch
                    && *self.signal.borrow()
                    && !expired(
                        observed,
                        &self.config,
                        self.store.as_ref(),
                        self.clock.now(),
                    )
                    && timing
                        .end(trim.source_window)
                        .is_some_and(|end| now <= end.saturating_add(timing.age))
                {
                    if trim.survival.includes(self.config.draw("trim", &trim.salt)) {
                        if let Some(weights) = &self.weights {
                            weights.multiply(1.0 / trim.survival.probability())?;
                        }
                    } else {
                        if let Some(weights) = &self.weights {
                            weights.stop();
                        }
                        self.signal.send_replace(false);
                        tracing::info!("sampling active process excluded by thinning");
                    }
                }
            }
        }
        let label = window.to_string();
        let start = window * timing.width;
        let census_at = start
            + (u128::from(self.config.draw("census-instant", &label)) * u128::from(timing.width)
                / SPACE) as u64;
        let profiler_at = start
            + (u128::from(self.config.draw("profiler-instant", &label)) * u128::from(timing.width)
                / SPACE) as u64;
        self.plan = Some(Plan {
            window,
            cutoff: self.cutoff.clone(),
            census_rank: self.config.draw("census-rank", &label),
            census_at,
            profiler_rank: self.config.draw("profiler-rank", &label),
            profiler_at,
            census_done: census_at < self.born_ms
                || now > census_at.saturating_add(timing.dispatch),
            profiler_done: profiler_at < self.born_ms
                || now > profiler_at.saturating_add(timing.dispatch),
        });
        Ok(())
    }
    fn launch(
        &mut self,
        channel: &'static str,
        window: u64,
        rank: u64,
        cutoff: Cutoff,
    ) -> Result<(), StoreError> {
        if self.in_flight.values().filter(|(_, _, poll)| !poll).count() >= 4 && channel != "control"
        {
            return Err(invalid("sampling observation job budget exhausted"));
        }
        let config = self.config.clone();
        let store = self.store.clone();
        let clock = self.clock.clone();
        let cache = self.cache.clone();
        let (done, complete) = oneshot::channel();
        let completion = Completion(Some(done));
        let handle = self.jobs.spawn(async move {
            let _completion = completion;
            if channel == "census" || channel == "control" {
                let read = bounded(
                    Timing::new(&config).operation,
                    store.get(format!("{}roster.json", config.pool_prefix())),
                )
                .await;
                match read.and_then(|object| object.map(|o| decode(o, &config)).transpose()) {
                    Ok(observed) => publish_cache(&cache, observed),
                    Err(error) => {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(%error, "sampling report control read failed");
                        });
                    }
                }
            }
            if channel == "control" {
                return Ok(());
            }
            if !cache.borrow().compatible() {
                return Ok(());
            }
            // Eligibility and recording were captured at the independent observation instant.
            write_report(
                &config,
                store.as_ref(),
                clock.as_ref(),
                channel,
                window,
                rank,
                &cutoff,
            )
            .await
        });
        self.tracker
            .0
            .lock()
            .expect("sampling job tracker")
            .insert(handle.id(), (handle.clone(), complete));
        self.in_flight
            .insert(handle.id(), (window, handle, channel == "control"));
        Ok(())
    }
    fn completed(
        &mut self,
        result: Result<(Id, Result<(), StoreError>), tokio::task::JoinError>,
    ) -> Result<(), StoreError> {
        match result {
            Ok((id, result)) => {
                self.in_flight.remove(&id);
                self.tracker
                    .0
                    .lock()
                    .expect("sampling job tracker")
                    .remove(&id);
                if let Err(error) = result {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(%error, "sampling immutable report failed");
                    });
                }
            }
            Err(error) => {
                self.in_flight.remove(&error.id());
                self.tracker
                    .0
                    .lock()
                    .expect("sampling job tracker")
                    .remove(&error.id());
                if error.is_panic() {
                    return Err(invalid(format!(
                        "sampling observation task panicked: {error}"
                    )));
                }
            }
        }
        Ok(())
    }
    async fn remove_stale(&mut self, window: u64) -> Result<(), StoreError> {
        for (source, handle, poll) in self.in_flight.values() {
            if !poll && source.saturating_add(1) < window {
                handle.abort();
            }
        }
        while self
            .in_flight
            .values()
            .any(|(source, _, poll)| !poll && source.saturating_add(1) < window)
        {
            if let Some(result) = self.jobs.join_next_with_id().await {
                self.completed(result)?;
            }
        }
        Ok(())
    }
    pub(super) async fn run(&mut self) -> Result<(), StoreError> {
        loop {
            while let Some(result) = self.jobs.try_join_next_with_id() {
                self.completed(result)?;
            }
            let now = millis(self.clock.now())?;
            let timing = Timing::new(&self.config);
            self.remove_stale(now / timing.width).await?;
            self.boundary(now)?;
            let period = if *self.signal.borrow() {
                timing.width
            } else {
                timing.poll
            };
            if period != self.poll_period_ms {
                self.poll_period_ms = period;
                self.next_poll_ms = next_slot(now, period, self.poll_phase_ms)?;
            }
            if now >= self.next_poll_ms {
                self.next_poll_ms = next_slot(
                    now.checked_add(1)
                        .ok_or_else(|| invalid("poll time overflow"))?,
                    period,
                    self.poll_phase_ms,
                )?;
                if !self.in_flight.values().any(|(_, _, poll)| *poll) {
                    self.launch("control", now / timing.width, 0, Cutoff(None))?;
                }
            }
            let compatible = self.cache.borrow().compatible();
            let plan = self.plan.as_mut().expect("window prepared");
            let census = if !plan.census_done && now >= plan.census_at {
                plan.census_done = true;
                (compatible
                    && now <= plan.census_at.saturating_add(timing.dispatch)
                    && plan.cutoff.includes(plan.census_rank))
                .then(|| (plan.window, plan.census_rank, plan.cutoff.clone()))
            } else {
                None
            };
            let profiler = if !plan.profiler_done && now >= plan.profiler_at {
                plan.profiler_done = true;
                (compatible
                    && now <= plan.profiler_at.saturating_add(timing.dispatch)
                    && *self.signal.borrow())
                .then_some((plan.window, plan.profiler_rank))
            } else {
                None
            };
            let mut next = ((now / timing.width + 1) * timing.width).min(self.next_poll_ms);
            if !plan.census_done {
                next = next.min(plan.census_at);
            }
            if !plan.profiler_done {
                next = next.min(plan.profiler_at);
            }
            if let Some((window, rank, cutoff)) = census {
                self.launch("census", window, rank, cutoff)?;
            }
            if let Some((window, rank)) = profiler {
                self.launch("profilers", window, rank, Cutoff(None))?;
            }
            tokio::select! {
                result = self.jobs.join_next_with_id(), if !self.jobs.is_empty() => { if let Some(result) = result { self.completed(result)?; } },
                () = tokio::time::sleep(Duration::from_millis(next.saturating_sub(now).max(1))) => {},
            }
        }
    }
    pub(super) async fn shutdown(&mut self) {
        // Stop control polling immediately; captured live observations may finish
        // within the session's shared cleanup deadline before forced cancellation.
        for (_, handle, poll) in self.in_flight.values() {
            if *poll {
                handle.abort();
            }
        }
        while let Some(result) = self.jobs.join_next_with_id().await {
            if let Err(error) = self.completed(result) {
                tracing::error!(%error, "sampling report panicked during shutdown");
            }
        }
        self.in_flight.clear();
    }
}

pub(super) async fn write_report(
    config: &Config,
    store: &dyn SamplingStore,
    clock: &dyn Clock,
    channel: &str,
    window: u64,
    rank: u64,
    cutoff: &Cutoff,
) -> Result<(), StoreError> {
    let timing = Timing::new(config);
    let deadline = timing
        .end(window)
        .and_then(|end| end.checked_add(timing.submission))
        .ok_or_else(|| invalid("report deadline overflow"))?;
    let key = census::report_key(config, channel, window, rank, cutoff);
    for attempt in 0..2 {
        let now = millis(clock.now())?;
        if now >= deadline {
            return Err(invalid("report submission deadline expired"));
        }
        let remaining = Duration::from_millis(deadline - now);
        let result = bounded(
            timing.operation.min(remaining),
            store.put(
                PutRequest::builder()
                    .key(&key)
                    .body(Bytes::new())
                    .condition(WriteCondition::absent())
                    .build(),
            ),
        )
        .await;
        match result {
            Ok(PutOutcome::Stored { .. } | PutOutcome::PreconditionFailed {}) => return Ok(()),
            Err(error) if attempt == 0 => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::debug!(%error, "retrying immutable sampling report");
                });
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two report attempts return")
}
