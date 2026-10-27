//! Sticky startup admission around a synchronous application entry point.
//!
//! The sampler drives preparation on its own runtime and keeps coordination
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
//!     .exact_target(15)
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
/// The failure policy is required. Defaults are a 1000ms startup budget and
/// keeping profiling enabled after lease loss. There is no periodic rotation.
/// Fixed-probability failure fallback can exceed the requested target.
///
/// ```compile_fail
/// use dial9_sampling::Sampling;
/// Sampling::builder().scope("checkout").exact_target(15).build();
/// ```
#[derive(Clone, Debug, bon::Builder)]
pub struct Sampling {
    #[builder(field)]
    dimensions: Vec<(String, String)>,
    /// Service/environment namespace separating unrelated fleets.
    #[builder(into)]
    scope: String,
    /// Desired profilers for each complete combination of dimension values.
    exact_target: u32,
    /// Required behavior on storage failure or timeout.
    if_failed: StartupFailure,
    /// Cooperative budget covering runtime setup, backend construction and admission; defaults to 1000ms.
    #[builder(default = Duration::from_millis(1000))]
    startup_timeout: Duration,
    /// Request one permanent stop on lease loss; defaults to false for continuity.
    /// Register the recorder with `SamplingStatus::register_recorder` (`dial9` feature).
    #[builder(default)]
    stop_on_lease_loss: bool,
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
        let mut builder = protocol::Config::builder()
            .scope(&self.scope)
            .exact_target(self.exact_target)
            .if_failed(self.if_failed.clone())
            .lease_loss(if self.stop_on_lease_loss {
                protocol::LeaseLoss::stop_profiling()
            } else {
                protocol::LeaseLoss::keep_profiling()
            });
        for (name, value) in &self.dimensions {
            builder = builder.add_dimension(name, value);
        }
        builder.build()
    }

    fn preflight(&self, policy: &protocol::Config) -> Option<protocol::PreparedSampling> {
        match policy.validate() {
            Err(message) => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::invalid_configuration(message),
                false,
            )),
            Ok(()) if self.exact_target == 0 => Some(protocol::PreparedSampling::disabled(
                protocol::DecisionReason::target_zero(),
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
        draw: f64,
    ) -> protocol::PreparedSampling {
        if timeout.is_zero() {
            return protocol::PreparedSampling::disabled(
                protocol::DecisionReason::timeout(),
                policy.failure_decision(draw),
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
                    if remaining.is_zero() {
                        return protocol::PreparedSampling::disabled(
                            protocol::DecisionReason::timeout(),
                            policy.failure_decision(draw),
                        );
                    }
                    policy
                        .prepare_with_timeout_and_draw(Arc::new(store), remaining, draw)
                        .await
                }
                Ok(Err(error)) => protocol::PreparedSampling::disabled(
                    protocol::DecisionReason::store_failure(error.to_string()),
                    policy.failure_decision(draw),
                ),
                Err(_) => protocol::PreparedSampling::disabled(
                    protocol::DecisionReason::timeout(),
                    policy.failure_decision(draw),
                ),
            },
        }
    }

    fn status(
        &self,
        initial: protocol::SamplingSnapshot,
        _maintenance: Option<&protocol::SamplingSession>,
    ) -> SamplingStatus {
        tracing::info!(enabled = initial.recording(), reason = ?initial.reason(), "sampling startup decision");
        SamplingStatus {
            enabled: initial.recording(),
            #[cfg(any(test, feature = "dial9"))]
            changes: _maintenance
                .filter(|_| self.stop_on_lease_loss && initial.coordinated)
                .map(protocol::SamplingSession::recording_changes),
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
    /// process exit or failed cleanup still relies on lease expiry.
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
                let draw = protocol::startup_draw();
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
                                draw,
                            ))
                    }
                    Err(error) => protocol::PreparedSampling::disabled(
                        protocol::DecisionReason::store_failure(error.to_string()),
                        policy.failure_decision(draw),
                    ),
                }
            }
        };
        let initial = prepared.snapshot();
        session.maintenance = prepared.into_session();
        #[allow(unused_mut)]
        let mut status = sampling.status(initial, session.maintenance.as_ref());
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
/// Dropping this value does not stop lease maintenance or release capacity.
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
    /// An initially disabled choice disables the recorder immediately. With
    /// `stop_on_lease_loss`, the sampler's runtime disables it on lease loss or
    /// maintenance termination. The sampler never re-enables it.
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
    /// An initially disabled application completes immediately. Default continuity
    /// and enabled failure fallback never complete. With `stop_on_lease_loss`,
    /// lease loss or maintenance termination completes this future permanently.
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
