#![doc = include_str!("../README.md")]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, reason = "Test assertions fail immediately.")
)]

mod object_key;
mod protocol;

mod registration;
mod trace_weights;

#[cfg(all(test, not(shuttle)))]
mod tests;

#[cfg(test)]
mod test_store;

pub mod backend;

/// Native S3 backend
#[cfg(feature = "s3")]
pub mod s3;

pub use protocol::StartupFailure;

use backend::SamplingStore;
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::runtime::Runtime;

/// Startup admission controller
///
/// Dimensions allow grouping processes into separate groups. This allows having
/// a target population for e.g. a specific instance type, cell, etc.
///
/// If a decision can't be reached within `startup_timeout` (default 1s), the `if_failed` behavior will be used.
///
/// See the [complete runnable example][example].
///
/// [example]: https://github.com/dial9-rs/dial9/blob/3297ff097371a7af6d47ac33280e4f5ab639a14e/dial9/examples/manual_sampling.rs
///
/// ```compile_fail
/// use dial9_sampling::Sampling;
/// Sampling::builder().num_target_profilers(15).build();
/// ```
#[derive(Clone, Debug, bon::Builder)]
pub struct Sampling {
    #[builder(field)]
    dimensions: Vec<(String, String)>,
    /// Expected number of profiling processes/containers per dimension.
    num_target_profilers: u32,
    /// Behavior on storage failure or timeout.
    if_failed: StartupFailure,
    /// Timeout for sampler startup.
    ///
    /// If the timeout expires, the profiler will fall back to `if_failed` behavior
    #[builder(default = Duration::from_millis(1000))]
    startup_timeout: Duration,

    #[builder(skip)]
    invocation_id: String,
    #[builder(skip)]
    settings: InternalSettings,
}

#[derive(Clone, Debug)]
struct InternalSettings {
    /// Additional admitted profilers tolerated before randomized thinning.
    /// Defaults to the ceiling of three times the square root of the target.
    max_overshoot: Option<u32>,
    /// Polling interval for the leader to check the profiler roster
    poll_interval: Duration,
    /// Completed census window width; defaults to one minute.
    /// Reporting, observation margins, and fixed roster polls derive from this width.
    bucket_width: Duration,
    /// Assumed maximum wall-clock skew; defaults to five seconds.
    ///
    /// If the real offset exceeds this, counting of active instances can become unreliable
    max_clock_skew: Duration,
}

impl Default for InternalSettings {
    fn default() -> Self {
        Self {
            max_overshoot: None,
            poll_interval: Duration::from_secs(60),
            bucket_width: Duration::from_secs(60),
            max_clock_skew: Duration::from_secs(5),
        }
    }
}

impl<S: sampling_builder::State> SamplingBuilder<S> {
    /// Add a dimension to this sample
    pub fn add_dimension(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.dimensions.push((name.into(), value.into()));
        self
    }
}

impl Sampling {
    /// Configure storage to use with `Sampling`
    ///
    /// This is inert until [`SamplingStartup::start_application`] is called.
    pub fn with_storage<S: SamplingStore, F: Future<Output = io::Result<S>>>(
        &self,
        backend: F,
    ) -> SamplingStartup<F> {
        SamplingStartup {
            sampling: self.clone(),
            backend,
        }
    }

    fn policy(&self) -> protocol::Config {
        let mut policy = self.clone();
        // Independent process runs never share caller-provided census entropy.
        policy.invocation_id = ulid::Ulid::new().to_string();
        policy
    }

    fn preflight(&self, policy: &protocol::Config) -> Option<protocol::PreparedSampling> {
        match policy.validate() {
            Err(message) => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::invalid_configuration(message),
                false,
            )),
            Ok(()) if self.num_target_profilers == 0 => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::TargetZero,
                false,
            )),
            Ok(())
                if std::time::Instant::now()
                    .checked_add(self.startup_timeout)
                    .is_none() =>
            {
                Some(protocol::PreparedSampling::disabled(
                    protocol::DecisionReason::invalid_configuration("startup timeout overflow"),
                    false,
                ))
            }
            Ok(()) => None,
        }
    }

    async fn admit<S: SamplingStore>(
        policy: &protocol::Config,
        backend: impl Future<Output = io::Result<S>>,
        timeout: Duration,
    ) -> protocol::PreparedSampling {
        if timeout.is_zero() {
            return protocol::PreparedSampling::disabled(
                protocol::DecisionReason::Timeout,
                policy.failure_decision(),
            );
        }
        match tokio::time::Instant::now().checked_add(timeout) {
            None => protocol::PreparedSampling::disabled(
                protocol::DecisionReason::invalid_configuration("startup timeout overflow"),
                false,
            ),
            Some(deadline) => match tokio::time::timeout_at(deadline, backend).await {
                Ok(Ok(store)) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    policy
                        .prepare_with_timeout(Arc::new(store), remaining)
                        .await
                }
                Ok(Err(error)) => protocol::PreparedSampling::disabled(
                    protocol::DecisionReason::store_failure(error.to_string()),
                    policy.failure_decision(),
                ),
                Err(_) => protocol::PreparedSampling::disabled(
                    protocol::DecisionReason::Timeout,
                    policy.failure_decision(),
                ),
            },
        }
    }

    fn status(
        &self,
        enabled: bool,
        reason: &protocol::DecisionReason,
        maintenance: Option<&protocol::SamplingSession>,
    ) -> SamplingStatus {
        tracing::info!(enabled, ?reason, "sampling startup decision");
        let probability = maintenance.map_or_else(
            || self.failure_cutoff() as f64 / (1u128 << 64) as f64,
            protocol::SamplingSession::startup_probability,
        );
        let initial_expansion_weight = enabled.then(|| 1.0 / probability);
        SamplingStatus {
            enabled,
            changes: maintenance.map(protocol::SamplingSession::recording_changes),
            registration: std::sync::Weak::new(),
            weights: initial_expansion_weight.map(|weight| {
                maintenance
                    .and_then(protocol::SamplingSession::weights)
                    .unwrap_or_else(|| trace_weights::TraceWeights::new(self.pool_path(), weight))
            }),
        }
    }
}

/// Lazy storage initialization and admission for one application invocation.
///
/// The storage future and application callback may borrow local values and need
/// not be `Send` or `'static`. Dropping an unused startup performs no I/O.
#[must_use = "call start_application to prepare sampling and run the application"]
pub struct SamplingStartup<F> {
    sampling: Sampling,
    backend: F,
}

impl<F> std::fmt::Debug for SamplingStartup<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingStartup")
            .field("sampling", &self.sampling)
            .finish_non_exhaustive()
    }
}

impl<F> SamplingStartup<F> {
    /// Starts the application with the provided `SamplingStatus` decision available
    ///
    /// # Panics
    ///
    /// Panics if called from an entered Tokio runtime. Call this synchronous
    /// entry point from ordinary `main`, before entering the application runtime.
    pub fn start_application<S: SamplingStore, R>(
        self,
        application: impl FnOnce(SamplingStatus) -> R,
    ) -> R
    where
        F: Future<Output = io::Result<S>>,
    {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "start_application must be called outside an entered Tokio runtime"
        );
        let start = std::time::Instant::now();
        let Self { sampling, backend } = self;
        let policy = sampling.policy();
        let mut session = ApplicationSession::default();
        let prepared = match sampling.preflight(&policy) {
            Some(prepared) => prepared,
            None => {
                // TODO: trace this runtime separately; future wrappers alone do not
                // expose worker park/unpark events or attach its runtime context.
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("dial9-sampling")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        // Own the runtime before polling user code, so initialization
                        // panics also shut down its spawned tasks without blocking.
                        session.runtime = Some(runtime);
                        session
                            .runtime
                            .as_ref()
                            .expect("sampling runtime")
                            .block_on(Sampling::admit(
                                &policy,
                                backend,
                                sampling.startup_timeout.saturating_sub(start.elapsed()),
                            ))
                    }
                    Err(error) => protocol::PreparedSampling::disabled(
                        protocol::DecisionReason::store_failure(error.to_string()),
                        policy.failure_decision(),
                    ),
                }
            }
        };
        let enabled = prepared.enabled;
        let reason = prepared.reason.clone();
        session.maintenance = prepared.into_session();
        let mut status = policy.status(enabled, &reason, session.maintenance.as_ref());
        let registration = Arc::new(registration::RecorderRegistrations::new(
            session
                .runtime
                .as_ref()
                .map(|runtime| runtime.handle().clone()),
        ));
        status.registration = Arc::downgrade(&registration);
        session.registration = Some(registration);
        application(status)
    }
}

/// Sampling status
///
/// Destructure public fields with `..` so future fields remain compatible.
/// Dropping this value does not stop roster maintenance or withdraw membership.
#[non_exhaustive]
#[derive(Debug)]
pub struct SamplingStatus {
    /// This application should profile
    pub enabled: bool,

    changes: Option<tokio::sync::watch::Receiver<bool>>,
    registration: std::sync::Weak<registration::RecorderRegistrations>,
    weights: Option<trace_weights::TraceWeights>,
}

impl SamplingStatus {
    /// Register the sampler with the `dial9` recorder
    ///
    /// This provides two automatic behaviors:
    /// 1. The relative weight of this segment will be automatically kept up to date and included in the segment metadata of all traces
    /// 2. If the sampler becomes disabled after startup (e.g. more nodes launch), the recorder will be automatically disabled.
    pub fn register_recorder(&self, recorder: &dial9_core::recording::Recorder) {
        let handle = recorder.handle().clone();
        let weights = self.weights.clone();
        let disable: registration::DisableCallback = Arc::new(move || {
            if let Some(weights) = &weights {
                weights.stop();
            }
            handle.disable();
        });
        let Some(registration) = self.registration.upgrade() else {
            disable();
            return;
        };
        if !self.enabled
            || self
                .weights
                .as_ref()
                .is_some_and(|weights| !weights.attach(recorder))
        {
            disable();
            registration.register(disable, None);
            return;
        }
        if let Some(changes) = &self.changes
            && (!*changes.borrow() || changes.has_changed().is_err())
        {
            disable();
        }
        registration.register(disable, self.changes.clone());
    }
}

#[derive(Default)]
struct ApplicationSession {
    registration: Option<Arc<registration::RecorderRegistrations>>,
    maintenance: Option<protocol::SamplingSession>,
    runtime: Option<Runtime>,
}

impl Drop for ApplicationSession {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            registration.stop();
        }
        if let Some(runtime) = self.runtime.take() {
            if let Some(session) = self.maintenance.take() {
                runtime.block_on(session.shutdown(protocol::CLEANUP_TIMEOUT));
            }
            runtime.shutdown_background();
        }
    }
}
