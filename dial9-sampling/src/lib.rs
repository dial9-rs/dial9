//! Sticky startup admission, independent of any recorder or application runtime.
//!
//! The session owns its executor and leases. Your application owns profiling,
//! stop handling and shutdown order. A recorder can retain the session with
//! `recorder.attach_object(session)`.
//!
//! ```no_run
//! # #[cfg(feature = "s3")]
//! # async fn example() {
//! use dial9_sampling::{Sampling, StartupFailure, s3::S3SamplingStore};
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
//! let session = sampling.prepare(async {
//!     let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
//!     Ok(S3SamplingStore::builder()
//!         .client(aws_sdk_s3::Client::new(&config))
//!         .bucket("trace-bucket")
//!         .prefix("_dial9_sampling/checkout/")
//!         .build())
//! }).await;
//! if session.enabled() {
//!     // Build your recorder and attach hooks before constructing the application runtime.
//! }
//! // Stop profiling before releasing coordination.
//! session.shutdown(Duration::from_secs(5)).await;
//! # }
//! ```
#![deny(missing_docs)]

mod protocol;
mod runtime;

#[cfg(all(test, not(shuttle)))]
mod tests;

/// Storage primitives for implementing a custom backend.
pub mod backend {
    pub use crate::protocol::store::*;
}

/// Native S3 backend, using a client independent of the trace uploader.
#[cfg(feature = "s3")]
pub mod s3;

/// Deterministic in-memory storage for simulations and application tests.
#[cfg(feature = "simulation")]
pub mod simulation {
    pub use crate::protocol::simulation::MemoryStore;
}

pub use protocol::StartupFailure;

use backend::SamplingStore;
use runtime::{OwnedRuntime, on_runtime};
use std::{future::Future, io, sync::Arc, time::Duration};

/// Startup admission for one complete caller-provided dimension tuple.
///
/// The failure policy is required. Defaults are a 500ms startup budget and
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
    /// Cooperative budget covering runtime setup, backend construction and admission; defaults to 500ms.
    #[builder(default = Duration::from_millis(500))]
    startup_timeout: Duration,
    /// Request one permanent stop on lease loss; defaults to false for continuity.
    /// The caller must observe `SamplingSession::stopped` and stop profiling.
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
    /// Makes one startup choice, including backend initialization within the budget.
    ///
    /// Preparation starts no profilers. The session owns a private, uninstrumented
    /// Tokio runtime for backend initialization and maintenance. The caller's
    /// executor can be destroyed once preparation completes. Construct resources
    /// that depend on an executor inside the backend future; already-constructed
    /// resources may still require their original executor.
    /// Backend initialization should be asynchronous and cancellation-safe. The
    /// timeout is cooperative and cannot interrupt blocking code.
    pub async fn prepare<S: SamplingStore>(
        &self,
        backend: impl Future<Output = io::Result<S>>,
    ) -> SamplingSession {
        self.prepare_with_runtime(backend, OwnedRuntime::new).await
    }

    async fn prepare_with_runtime<S: SamplingStore>(
        &self,
        backend: impl Future<Output = io::Result<S>>,
        build_runtime: impl FnOnce() -> io::Result<OwnedRuntime>,
    ) -> SamplingSession {
        let start = std::time::Instant::now();
        let policy = self.policy();
        if let Some(prepared) = self.preflight(&policy) {
            return self.session(prepared, None);
        }
        let draw = protocol::startup_draw();
        let runtime = match build_runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                return self.session(
                    protocol::PreparedSampling::disabled(
                        protocol::DecisionReason::store_failure(error.to_string()),
                        policy.failure_decision(draw),
                    ),
                    None,
                );
            }
        };
        let prepared = on_runtime(
            runtime.handle(),
            Self::admit(
                &policy,
                backend,
                self.startup_timeout.saturating_sub(start.elapsed()),
                draw,
            ),
        )
        .await;
        self.session(prepared, Some(runtime))
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

    fn session(
        &self,
        prepared: protocol::PreparedSampling,
        runtime: Option<OwnedRuntime>,
    ) -> SamplingSession {
        let initial = prepared.snapshot();
        tracing::info!(enabled = initial.recording(), reason = ?initial.reason(), "sampling startup decision");
        let maintenance = prepared.into_session();
        SamplingSession {
            initial,
            runtime: runtime.filter(|_| maintenance.is_some()),
            maintenance,
            stop_on_lease_loss: self.stop_on_lease_loss,
        }
    }

    #[cfg(all(test, not(shuttle)))]
    async fn prepare_on_executor<S: SamplingStore>(
        &self,
        backend: impl Future<Output = io::Result<S>>,
    ) -> SamplingSession {
        let policy = self.policy();
        let prepared = match self.preflight(&policy) {
            Some(prepared) => prepared,
            None => {
                Self::admit(
                    &policy,
                    backend,
                    self.startup_timeout,
                    protocol::startup_draw(),
                )
                .await
            }
        };
        self.session(prepared, None)
    }
}

/// Owns lease maintenance, independently of profiling.
///
/// The guard owns its maintenance runtime. Drop schedules cleanup with a 500ms
/// cooperative budget on that runtime. Outside a Tokio context, Drop waits for
/// cleanup; inside a Tokio context it returns without blocking. Explicit shutdown
/// awaits cleanup in either context and remains cancellation-safe.
///
/// Stop profiling before releasing the guard, or retain it with
/// `Recorder::attach_object` so it outlives recorder finalization.
#[must_use = "retain the session until profiling stops, or attach it to the recorder"]
pub struct SamplingSession {
    initial: protocol::SamplingSnapshot,
    maintenance: Option<protocol::SamplingSession>,
    stop_on_lease_loss: bool,
    runtime: Option<OwnedRuntime>,
}

impl std::fmt::Debug for SamplingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.maintenance {
            Some(session) => session.fmt(f),
            None => self.initial.fmt(f),
        }
    }
}

impl SamplingSession {
    /// The frozen startup choice. A later stop notification does not change it.
    pub fn enabled(&self) -> bool {
        self.initial.recording()
    }

    /// An owned future completing when profiling should stop. It never resumes.
    ///
    /// An initially disabled session completes immediately. An enabled session
    /// with the default continuity policy never completes, including after loss
    /// of its executor. An enabled failure fallback has no lease and also never
    /// completes. The future does not borrow the guard and can be spawned.
    pub fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        let enabled = self.enabled();
        let strict = self.stop_on_lease_loss;
        let changes = self
            .maintenance
            .as_ref()
            .filter(|_| self.initial.coordinated)
            .map(|s| s.recording_changes());
        async move {
            if !enabled {
                return;
            }
            if strict && let Some(mut changes) = changes {
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

    /// Stops maintenance and attempts owner-checked release within the budget.
    ///
    /// Canceling this wait does not cancel the cleanup attempt on the owned runtime.
    pub async fn shutdown(mut self, timeout: Duration) {
        if let Some(done) = self.start_shutdown(timeout) {
            if let Err(error) = done.await {
                tracing::warn!(%error, "sampling cleanup task ended unexpectedly");
            }
        } else if let Some(session) = self.maintenance.take() {
            session.shutdown(timeout).await;
        }
    }

    fn start_shutdown(&mut self, timeout: Duration) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let runtime = self.runtime.take()?;
        let maintenance = self.maintenance.take();
        let (done, completed) = tokio::sync::oneshot::channel();
        runtime.handle().clone().spawn(async move {
            if let Some(session) = maintenance {
                session.shutdown(timeout).await;
            }
            drop(runtime);
            // The caller may have canceled explicit shutdown or dropped the guard.
            let _ = done.send(());
        });
        Some(completed)
    }
}

impl Drop for SamplingSession {
    fn drop(&mut self) {
        if let Some(done) = self.start_shutdown(Duration::from_millis(500))
            && tokio::runtime::Handle::try_current().is_err()
            && let Err(error) = done.blocking_recv()
        {
            tracing::warn!(%error, "sampling cleanup task ended unexpectedly");
        }
    }
}
