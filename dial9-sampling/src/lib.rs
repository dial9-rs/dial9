//! Sticky startup admission around a synchronous application entry point.
//!
//! The sampler reads the roster on its own runtime and keeps coordination
//! alive until your application returns. Register the recorder for automatic
//! stop handling; your application owns its normal runtime/recorder shutdown.
//!
//! ```no_run
//! # #[cfg(all(feature = "s3", feature = "dial9"))]
//! # fn example() -> std::io::Result<()> {
//! use dial9::{Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions};
//! use dial9_sampling::{Sampling, SamplingStatus, StartupFailure, s3::S3SamplingStore};
//! use std::time::Duration;
//!
//! let sampling = Sampling::builder()
//!     .scope("checkout/production")
//!     .add_dimension("instance-type", "m7i.large")
//!     .add_dimension("az", "us-east-1a")
//!     .target(15)
//!     .startup_timeout(Duration::from_secs(5))
//!     .if_failed(StartupFailure::disabled())
//!     .build();
//! sampling.with_backend(async {
//!     let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
//!     Ok(S3SamplingStore::builder()
//!         .client(aws_sdk_s3::Client::new(&config))
//!         .bucket("trace-bucket")
//!         .build())
//! }).start_application(|status| {
//!     let SamplingStatus { enabled, .. } = status;
//!     let recorder = if enabled {
//!         dial9::recorder(MemoryBuffer::new(16 * 1024 * 1024)?).build()
//!     } else {
//!         dial9::recorder_disabled()
//!     };
//!     status.register_recorder(&recorder);
//!     let mut builder = tokio::runtime::Builder::new_multi_thread();
//!     builder.enable_all();
//!     let runtime = recorder.handle().attach_tokio_runtime(
//!         builder,
//!         TokioAttachOptions::default(),
//!     )?;
//!     dial9::block_on(&runtime, async {
//!         dial9::spawn(async {
//!             // Run the application's async work here.
//!         }).await.expect("application task");
//!     });
//!     drop(runtime);
//!     recorder.graceful_shutdown(Duration::from_secs(5));
//!     Ok(())
//! })
//! # }
//! ```
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, reason = "Test assertions fail immediately.")
)]

mod protocol;

#[cfg(feature = "dial9")]
mod registration;

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

/// Startup admission for one complete caller-provided dimension tuple.
///
/// The target is an expected count, not a reservation of exactly that many
/// profilers. The failure policy is required. Defaults are a 1000ms startup
/// budget and tracking every process. There is no periodic rotation.
///
/// ```compile_fail
/// use dial9_sampling::Sampling;
/// Sampling::builder().scope("checkout").target(15).build();
/// ```
#[derive(Clone, Debug, bon::Builder)]
pub struct Sampling {
    #[builder(field)]
    dimensions: Vec<(String, String)>,
    /// Service/environment namespace separating unrelated fleets.
    #[builder(into)]
    scope: String,
    /// Expected profilers for each complete combination of dimension values.
    target: u32,
    /// Required behavior on storage failure or timeout.
    if_failed: StartupFailure,
    /// Cooperative budget covering runtime setup, backend construction and admission; defaults to 1000ms.
    #[builder(default = Duration::from_millis(1000))]
    startup_timeout: Duration,
    /// Stable identity for sticky rank across restarts; defaults to a fresh identity per invocation.
    /// Distinct simultaneously live processes should have distinct identities.
    /// Must be nonblank and must not contain `/`.
    #[builder(into)]
    host_id: Option<String>,
    /// Conservative lower bound on the population; zero tracks every process.
    /// A positive bound reduces membership traffic. Exceeding the real population
    /// weakens coverage, especially for small pools.
    #[builder(default)]
    minimum_population: u64,
    /// Additional active profilers tolerated before permanent trimming.
    /// Defaults to the ceiling of three times the square root of the target.
    max_overshoot: Option<u32>,
    /// Background roster polling interval; defaults to one minute.
    /// Must be between 100ms and eight hours, using whole milliseconds.
    #[builder(default = Duration::from_secs(60))]
    poll_interval: Duration,
    /// Membership bucket width; defaults to five minutes.
    /// Must cover two polls plus twice the clock skew, and be at most one day.
    /// Use whole milliseconds.
    #[builder(default = Duration::from_secs(300))]
    bucket_width: Duration,
    /// Assumed maximum wall-clock skew; defaults to five seconds.
    /// Use whole milliseconds.
    #[builder(default = Duration::from_secs(5))]
    max_clock_skew: Duration,
}

impl<S: sampling_builder::State> SamplingBuilder<S> {
    /// Adds a dimension. Names must be unique; names and values must be nonblank.
    pub fn add_dimension(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.dimensions.push((name.into(), value.into()));
        self
    }
}

impl Sampling {
    /// Supplies backend initialization for a synchronous application invocation.
    ///
    /// Nothing runs until [`SamplingStartup::start_application`] is called.
    /// Backend initialization and maintenance use the same private, uninstrumented
    /// Tokio runtime, which stays alive until the application returns.
    /// Construct executor-dependent resources inside the supplied future.
    /// Backend initialization should be asynchronous and cancellation-safe. The
    /// timeout is cooperative and cannot interrupt blocking code.
    pub fn with_backend<S: SamplingStore, F: Future<Output = io::Result<S>>>(
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
        // Each invocation gets a fresh rank unless the caller supplies an identity.
        policy
            .host_id
            .get_or_insert_with(|| ulid::Ulid::new().to_string());
        policy
    }

    fn preflight(&self, policy: &protocol::Config) -> Option<protocol::PreparedSampling> {
        match policy.validate() {
            Err(message) => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::invalid_configuration(message),
                false,
            )),
            Ok(()) if self.target == 0 => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::TargetZero,
                false,
            )),
            Ok(()) if !policy.tracked() => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::Untracked,
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
        _maintenance: Option<&protocol::SamplingSession>,
    ) -> SamplingStatus {
        tracing::info!(enabled, ?reason, "sampling startup decision");
        SamplingStatus {
            enabled,
            #[cfg(any(test, feature = "dial9"))]
            changes: _maintenance.map(protocol::SamplingSession::recording_changes),
            #[cfg(feature = "dial9")]
            registration: std::sync::Weak::new(),
        }
    }
}

/// Lazy backend initialization and admission for one application invocation.
///
/// The backend future and application callback may borrow local values and need
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
    /// Resolves sampling, runs the application, then attempts bounded cleanup.
    ///
    /// The callback runs outside the sampling runtime's entered context. It may
    /// construct its own runtime and block on the application's main future.
    /// Finish application and recorder shutdown before returning; coordination
    /// remains alive until then. The callback's result is returned unchanged.
    ///
    /// Preparation uses the configured startup budget. Cleanup has a separate
    /// 500ms cooperative budget, including when the callback unwinds. Abrupt
    /// process exit or failed cleanup still relies on membership bucket expiry.
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
        self.start_application_with_runtime(application, || {
            // TODO: trace this runtime separately; future wrappers alone do not
            // expose worker park/unpark events or attach its runtime context.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("dial9-sampling")
                .enable_all()
                .build()
        })
    }

    fn start_application_with_runtime<S: SamplingStore, R>(
        self,
        application: impl FnOnce(SamplingStatus) -> R,
        build_runtime: impl FnOnce() -> io::Result<Runtime>,
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
                match build_runtime() {
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
        #[allow(unused_mut)]
        let mut status = sampling.status(enabled, &reason, session.maintenance.as_ref());
        #[cfg(feature = "dial9")]
        {
            let registration = Arc::new(registration::RecorderRegistrations::new(
                session
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.handle().clone()),
            ));
            status.registration = Arc::downgrade(&registration);
            session.registration = Some(registration);
        }
        application(status)
    }
}

/// Sampling status supplied to the application, without coordination ownership.
///
/// Destructure public fields with `..` so future fields remain compatible.
/// Dropping this value does not stop roster maintenance or withdraw membership.
#[non_exhaustive]
#[derive(Debug)]
pub struct SamplingStatus {
    /// The frozen startup choice. A later stop notification does not change it.
    pub enabled: bool,
    #[cfg(any(test, feature = "dial9"))]
    changes: Option<tokio::sync::watch::Receiver<bool>>,
    #[cfg(feature = "dial9")]
    registration: std::sync::Weak<registration::RecorderRegistrations>,
}

impl SamplingStatus {
    /// Registers automatic recorder stop handling for this application invocation.
    ///
    /// An initially disabled choice disables the recorder immediately. The
    /// sampler's runtime disables active recorders when a roster permanently
    /// trims this process. Storage failures preserve the current state.
    /// The sampler never re-enables recording.
    ///
    /// All registered recorders are disabled before sampling cleanup when the
    /// application returns or unwinds. Registration holds only a weak recording
    /// reference and does not take ownership of the recorder or its shutdown.
    /// Registering through a status returned from an ended invocation disables
    /// the recorder immediately.
    #[cfg(feature = "dial9")]
    pub fn register_recorder(&self, recorder: &dial9_core::recording::Recorder) {
        let disable = registration::StopAction::new(recorder.handle().disable_callback());
        let Some(registration) = self.registration.upgrade() else {
            disable.run();
            return;
        };
        if !self.enabled {
            disable.run();
            return;
        }
        if let Some(changes) = &self.changes
            && (!*changes.borrow() || changes.has_changed().is_err())
        {
            disable.run();
        }
        registration.register(disable, self.changes.as_ref().map(|_| self.stopped()));
    }

    /// An owned future completing when profiling should stop. It never resumes.
    ///
    /// An initially disabled application completes immediately. A permanent
    /// roster trim or maintenance termination completes this future permanently.
    /// A fallback without a constructed backend keeps recording until cleanup.
    /// The future does not borrow the status and can be spawned.
    #[cfg(any(test, feature = "dial9"))]
    fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        let enabled = self.enabled;
        let changes = self.changes.clone();
        async move {
            if !enabled {
                return;
            }
            if let Some(mut changes) = changes {
                while *changes.borrow_and_update() {
                    if changes.changed().await.is_err() {
                        return;
                    }
                }
                return;
            }
            std::future::pending().await
        }
    }
}

#[derive(Default)]
struct ApplicationSession {
    #[cfg(feature = "dial9")]
    registration: Option<Arc<registration::RecorderRegistrations>>,
    maintenance: Option<protocol::SamplingSession>,
    runtime: Option<Runtime>,
}

impl Drop for ApplicationSession {
    fn drop(&mut self) {
        #[cfg(feature = "dial9")]
        if let Some(registration) = self.registration.take() {
            registration.stop();
        }
        if let Some(runtime) = self.runtime.take() {
            if let Some(session) = self.maintenance.take() {
                runtime.block_on(session.shutdown(Duration::from_millis(500)));
            }
            runtime.shutdown_background();
        }
    }
}
