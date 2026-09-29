//! Pre-flight check that telemetry works: recording, CPU sampling,
//! symbolization, and S3 upload.
//!
//! [`SmokeTester`] owns the result cache, so build one at startup and call
//! [`run`](SmokeTester::run) as often as needed. Concurrent calls wait for the
//! running one, then read its cached result.
//!
//! ```no_run
//! # async fn f(handle: dial9_core::handle::Dial9Handle) {
//! use dial9_tokio_telemetry::telemetry::Dial9HandleTokioExt;
//!
//! let tester = handle.smoke_tester().build();
//! let report = tester.run().await;
//! if !report.is_healthy() {
//!     tracing::error!("dial9 smoke test failed:\n{report}");
//! }
//! # }
//! ```

use dial9_core::handle::Dial9Handle;
use std::fmt;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

/// Whether this platform has a CPU profiling backend: the predicate
/// `dial9-perf-self-profile` compiles its samplers under.
#[cfg_attr(not(feature = "cpu-profiling"), allow(dead_code))]
const HAS_BACKEND: bool = cfg!(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
));

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckStatus {
    /// Working.
    Ok,
    /// Working with a known limit, for example the ctimer fallback.
    #[non_exhaustive]
    Degraded {
        /// What is limited.
        reason: DegradedReason,
        /// The same, for people.
        detail: String,
    },
    /// Nothing proved it works or that it's broken, and something the caller
    /// controls could prove it.
    #[non_exhaustive]
    Unverified {
        /// Why it couldn't be proven.
        reason: UnverifiedReason,
        /// The same, for people.
        detail: String,
    },
    /// The check doesn't apply. Not a failure.
    #[non_exhaustive]
    Disabled {
        /// Why it doesn't apply.
        reason: DisabledReason,
        /// The same, for people.
        detail: String,
    },
    /// Should work, doesn't.
    #[non_exhaustive]
    Failed {
        /// Why.
        detail: String,
    },
}

/// Why a check is [`Degraded`](CheckStatus::Degraded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DegradedReason {
    /// CPU samples come from ctimer: only dial9-tracked threads are sampled.
    CtimerFallback,
    /// The binary has no symbol table: stacks show raw addresses.
    NoSymbolTable,
}

/// Why a check is [`Unverified`](CheckStatus::Unverified).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnverifiedReason {
    /// Idle, and no stimulus runs.
    StimulusOff,
    /// The stimulus was attempted and couldn't run.
    StimulusUnavailable,
    /// The burn fell short of its CPU time: the host is too contended.
    HostContended,
    /// The stimulus thread may be outside perf's thread tree.
    OutsidePerfTree,
}

/// Why a check is [`Disabled`](CheckStatus::Disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisabledReason {
    /// The check's feature isn't compiled in.
    NotCompiled,
    /// Turned off in the settings.
    TurnedOff,
    /// The recorder has no source or stage for it.
    NotConfigured,
    /// The platform has no profiling backend.
    Unsupported,
    /// The handle isn't connected to a recorder.
    NotConnected,
    /// The recorder has shut down.
    RecorderStopped,
    /// Recording is paused.
    RecordingPaused,
    /// The pipeline has no dial9 S3 stage.
    NoS3Stage,
    /// The recorder has no pipeline worker.
    NoPipeline,
}

#[cfg_attr(not(feature = "cpu-profiling"), allow(dead_code))]
impl CheckStatus {
    fn degraded(reason: DegradedReason, detail: impl Into<String>) -> Self {
        Self::Degraded {
            reason,
            detail: detail.into(),
        }
    }

    fn unverified(reason: UnverifiedReason, detail: impl Into<String>) -> Self {
        Self::Unverified {
            reason,
            detail: detail.into(),
        }
    }

    fn disabled(reason: DisabledReason, detail: impl Into<String>) -> Self {
        Self::Disabled {
            reason,
            detail: detail.into(),
        }
    }

    fn turned_off() -> Self {
        Self::disabled(DisabledReason::TurnedOff, "turned off")
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
        }
    }

    fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Neither failed nor unproven, so worth reusing.
    fn cacheable(&self) -> bool {
        !matches!(self, Self::Failed { .. } | Self::Unverified { .. })
    }
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Degraded { detail, .. } => write!(f, "degraded: {detail}"),
            Self::Unverified { detail, .. } => write!(f, "unverified: {detail}"),
            Self::Disabled { detail, .. } => write!(f, "disabled: {detail}"),
            Self::Failed { detail } => write!(f, "FAILED: {detail}"),
        }
    }
}

/// One check in a [`SmokeTestReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Check {
    /// [`SmokeTestReport::recording`].
    Recording,
    /// [`SmokeTestReport::cpu_sampling`].
    CpuSampling,
    /// [`SmokeTestReport::symbolization`].
    Symbolization,
    /// [`SmokeTestReport::s3_upload`].
    S3Upload,
}

impl Check {
    /// Stable name for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::CpuSampling => "cpu_sampling",
            Self::Symbolization => "symbolization",
            Self::S3Upload => "s3_upload",
        }
    }
}

/// CPU profiling backend behind [`SmokeTestReport::cpu_sampling`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CpuBackend {
    /// `perf_event_open`.
    Perf,
    /// Per-thread CPU timers; samples only dial9-tracked threads.
    Ctimer,
}

impl CpuBackend {
    /// `"perf"` or `"ctimer"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Perf => "perf",
            Self::Ctimer => "ctimer",
        }
    }
}

/// A status and when it was produced.
#[derive(Debug, Clone)]
struct Checked {
    status: CheckStatus,
    at: SystemTime,
}

impl Checked {
    fn now(status: CheckStatus) -> Self {
        Self {
            status,
            at: SystemTime::now(),
        }
    }
}

/// Result of [`SmokeTester::run`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SmokeTestReport {
    recording: Checked,
    cpu_sampling: Checked,
    symbolization: Checked,
    s3_upload: Checked,
    cpu_backend: Option<CpuBackend>,
}

impl SmokeTestReport {
    /// Whether the handle is connected to a live, unpaused recorder.
    pub fn recording(&self) -> &CheckStatus {
        &self.recording.status
    }

    /// Whether CPU samples reach the trace.
    pub fn cpu_sampling(&self) -> &CheckStatus {
        &self.cpu_sampling.status
    }

    /// Whether the pipeline resolves this binary's addresses.
    pub fn symbolization(&self) -> &CheckStatus {
        &self.symbolization.status
    }

    /// Whether an upload to the pipeline's bucket succeeds.
    pub fn s3_upload(&self) -> &CheckStatus {
        &self.s3_upload.status
    }

    /// The CPU profiling backend, when a CPU profiler is registered.
    pub fn cpu_backend(&self) -> Option<CpuBackend> {
        self.cpu_backend
    }

    fn checked(&self, check: Check) -> &Checked {
        match check {
            Check::Recording => &self.recording,
            Check::CpuSampling => &self.cpu_sampling,
            Check::Symbolization => &self.symbolization,
            Check::S3Upload => &self.s3_upload,
        }
    }

    /// Every check with its identifier.
    pub fn checks(&self) -> impl Iterator<Item = (Check, &CheckStatus)> {
        [
            Check::Recording,
            Check::CpuSampling,
            Check::Symbolization,
            Check::S3Upload,
        ]
        .into_iter()
        .map(|check| (check, &self.checked(check).status))
    }

    /// When `check`'s status was produced: earlier than this run when it came
    /// from the cache.
    pub fn checked_at(&self, check: Check) -> SystemTime {
        self.checked(check).at
    }

    /// `false` if any check [`Failed`](CheckStatus::Failed).
    pub fn is_healthy(&self) -> bool {
        !self.checks().any(|(_, status)| status.is_failed())
    }

    /// `false` if any check failed or is
    /// [`Unverified`](CheckStatus::Unverified).
    pub fn is_verified(&self) -> bool {
        self.checks().all(|(_, status)| status.cacheable())
    }
}

impl fmt::Display for SmokeTestReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (check, status) in self.checks() {
            writeln!(f, "{}: {status}", check.as_str())?;
        }
        if let Some(backend) = self.cpu_backend {
            writeln!(f, "cpu_backend: {}", backend.as_str())?;
        }
        Ok(())
    }
}

/// Resolved [`SmokeTester`] settings.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "cpu-profiling"), allow(dead_code))]
struct Settings {
    cache_ttl: Duration,
    check_timeout: Duration,
    cpu_sampling: bool,
    passive_window: Duration,
    stimulus: bool,
    stimulus_cpu_time: Duration,
    s3_upload: bool,
    symbolization: bool,
}

impl Settings {
    fn timed_out(&self) -> CheckStatus {
        CheckStatus::failed(format!("timed out after {:?}", self.check_timeout))
    }
}

/// A cached result: when it expires is measured on `Instant`.
type Entry = Option<(Instant, Checked)>;

#[derive(Default)]
struct Cache {
    cpu: Entry,
    /// Kept for the tester's life: the binary can't change.
    symbolization: Option<Checked>,
    /// A symbolization still running after its timeout.
    symbolize_task: pipeline::SymbolizeTask,
    s3: Entry,
}

/// Runs the smoke test against one recorder and caches passing results.
///
/// Build it once with `handle.smoke_tester()` and keep it: the cache stops
/// repeated or concurrent runs from each burning CPU and uploading.
pub struct SmokeTester {
    handle: Dial9Handle,
    settings: Settings,
    cache: tokio::sync::Mutex<Cache>,
}

impl fmt::Debug for SmokeTester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmokeTester")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

#[bon::bon]
impl SmokeTester {
    /// Tester for the recorder behind `handle`, reached as
    /// `handle.smoke_tester()` through `Dial9HandleTokioExt`.
    #[builder(
        start_fn(name = builder, vis = "pub(crate)"),
        finish_fn = build,
        builder_type = SmokeTesterBuilder
    )]
    pub fn new(
        #[builder(start_fn)] handle: Dial9Handle,
        /// How long a passing CPU or S3 result is reused. `Duration::ZERO`
        /// re-checks every run. Symbolization ignores it. Default one hour.
        #[builder(default = Duration::from_secs(3600))]
        cache_ttl: Duration,
        /// Upper bound for each check, including waiting for the pipeline
        /// worker to start. Default 10s.
        #[builder(default = Duration::from_secs(10))]
        check_timeout: Duration,
        /// Run the CPU check. Default `true`.
        #[builder(default = true)]
        cpu_sampling: bool,
        /// How long to watch the sample counter before the stimulus. Default
        /// 200ms.
        #[builder(default = Duration::from_millis(200))]
        passive_window: Duration,
        /// Run the stimulus when the passive window saw no samples. Default
        /// `true`.
        #[builder(default = true)]
        stimulus: bool,
        /// CPU time the stimulus burns. Default 50ms.
        #[builder(default = Duration::from_millis(50))]
        stimulus_cpu_time: Duration,
        /// Run the S3 check. Default `true`.
        #[builder(default = true)]
        s3_upload: bool,
        /// Run the symbolization check. Turn off for an app that symbolizes
        /// offline on purpose. Default `true`.
        #[builder(default = true)]
        symbolization: bool,
    ) -> Self {
        Self {
            handle,
            settings: Settings {
                cache_ttl,
                check_timeout,
                cpu_sampling,
                passive_window,
                stimulus,
                stimulus_cpu_time,
                s3_upload,
                symbolization,
            },
            cache: tokio::sync::Mutex::new(Cache::default()),
        }
    }
}

// `run()` must stay spawnable; its nested joins and timeouts overflow the
// compiler's query depth unless the inner checks are boxed.
const _: fn(&'static SmokeTester) = |t| {
    fn assert_send<X: Send>(_: X) {}
    assert_send(t.run());
};

impl SmokeTester {
    /// Run every check. Needs a Tokio runtime with the time driver enabled.
    pub async fn run(&self) -> SmokeTestReport {
        // Held for the whole run: a concurrent caller waits, then reads the
        // cache.
        let mut cache = self.cache.lock().await;
        let Cache {
            cpu,
            symbolization,
            symbolize_task,
            s3,
        } = &mut *cache;
        let recording = Checked::now(check_recording(&self.handle));
        let (cpu_sampling, (symbolization, s3_upload)) = tokio::join!(
            Box::pin(self.cpu_sampling(cpu, &recording.status)),
            Box::pin(pipeline::check(
                &self.handle,
                &self.settings,
                symbolization,
                symbolize_task,
                s3
            )),
        );
        SmokeTestReport {
            recording,
            cpu_sampling,
            symbolization,
            s3_upload,
            cpu_backend: sampling::cpu_backend(&self.handle),
        }
    }

    async fn cpu_sampling(&self, cache: &mut Entry, recording: &CheckStatus) -> Checked {
        if !self.settings.cpu_sampling {
            return Checked::now(CheckStatus::turned_off());
        }
        // Before the cache: a pause or stop shows on the next run.
        if let Some(off) = sampling_off(&self.handle, recording) {
            return Checked::now(off);
        }
        if let Some((expires, checked)) = cache.as_ref()
            && Instant::now() < *expires
        {
            return checked.clone();
        }
        let status = tokio::time::timeout(
            self.settings.check_timeout,
            sampling::check(&self.handle, &self.settings),
        )
        .await
        .unwrap_or_else(|_| self.settings.timed_out());
        let checked = Checked::now(status);
        *cache = checked
            .status
            .cacheable()
            .then(|| (Instant::now() + self.settings.cache_ttl, checked.clone()));
        checked
    }
}

/// Why a check can't run while the recorder isn't connected or has stopped.
fn recorder_gone(handle: &Dial9Handle) -> Option<CheckStatus> {
    if !handle.is_connected() {
        Some(CheckStatus::disabled(
            DisabledReason::NotConnected,
            "handle is not connected to a recorder",
        ))
    } else if handle.is_stopped() {
        Some(CheckStatus::disabled(
            DisabledReason::RecorderStopped,
            "recorder has shut down",
        ))
    } else {
        None
    }
}

/// CPU status while recording isn't live: the flush thread drains sources
/// only while recording is enabled, so the counter can't move.
fn sampling_off(handle: &Dial9Handle, recording: &CheckStatus) -> Option<CheckStatus> {
    match recording {
        CheckStatus::Ok => None,
        _ => Some(recorder_gone(handle).unwrap_or_else(|| {
            CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
        })),
    }
}

fn check_recording(handle: &Dial9Handle) -> CheckStatus {
    if !handle.is_connected() {
        CheckStatus::failed("handle is not connected to a recorder")
    } else if handle.is_stopped() {
        CheckStatus::failed("recorder has shut down")
    } else if !handle.is_enabled() {
        CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
    } else {
        CheckStatus::Ok
    }
}

/// Every dial9 S3 stage the recorder builder saw, in one source so the S3
/// check finds them all with `with_source`. It records nothing. A stage left
/// out of the running pipeline (an uploader replaced by a later one, or by a
/// custom pipeline) never fills its client slot, and the check skips it.
#[cfg(feature = "worker-s3")]
#[derive(Default)]
pub(crate) struct PipelineS3Targets {
    targets: Vec<S3Target>,
}

#[cfg(feature = "worker-s3")]
impl PipelineS3Targets {
    pub(crate) fn extend(&mut self, targets: impl IntoIterator<Item = S3Target>) {
        self.targets.extend(targets);
    }
}

#[cfg(feature = "worker-s3")]
impl dial9_core::source::Source for PipelineS3Targets {
    fn flush(&mut self, _ctx: &dial9_core::source::FlushContext<'_>) {}

    fn name(&self) -> &'static str {
        "s3_smoke_targets"
    }
}

/// One S3 stage: its config, and the slot its uploader publishes the client
/// it resolved in `initialize()` into.
#[cfg(feature = "worker-s3")]
#[derive(Clone)]
pub(crate) struct S3Target {
    config: dial9_destinations_s3::S3Config,
    client: dial9_destinations_s3::S3ClientSlot,
}

#[cfg(feature = "worker-s3")]
impl S3Target {
    pub(crate) fn new(
        config: dial9_destinations_s3::S3Config,
        client: dial9_destinations_s3::S3ClientSlot,
    ) -> Self {
        Self { config, client }
    }
}

/// Symbolization and S3: separate checks, gated on the pipeline's worker state
/// and stage order.
mod pipeline {
    use super::{CheckStatus, Checked, DisabledReason, Entry, Settings};
    use dial9_core::handle::Dial9Handle;
    use dial9_core::pipeline::{PipelineStatus, WorkerState};
    use std::time::Duration;
    use tokio::time::Instant;

    /// A symbolization still running after its timeout.
    #[cfg(feature = "cpu-profiling")]
    pub(super) type SymbolizeTask =
        Option<tokio::task::JoinHandle<crate::telemetry::smoke_probe::SymbolizationProbe>>;
    #[cfg(not(feature = "cpu-profiling"))]
    pub(super) type SymbolizeTask = ();

    const SYMBOLIZE: &str = "Symbolize";
    const GZIP: &str = "Gzip";

    pub(super) async fn check(
        handle: &Dial9Handle,
        settings: &Settings,
        symbolization_cache: &mut Option<Checked>,
        symbolize_task: &mut SymbolizeTask,
        s3_cache: &mut Entry,
    ) -> (Checked, Checked) {
        // One deadline for each check, waiting for the worker included.
        let deadline = Instant::now() + settings.check_timeout;
        let symbolization_off = symbolization_precheck(handle, settings);
        let s3_off = s3_precheck(handle, settings);
        // Only wait for the worker when a check still needs it.
        let worker = match (&symbolization_off, &s3_off) {
            (Some(_), Some(_)) => None,
            _ => Some(wait_for_running(handle, deadline).await),
        };
        let symbolization = async {
            if let Some(off) = symbolization_off {
                return Checked::now(off);
            }
            let status = match worker.as_ref().expect("waited for the worker") {
                Ok(status) => status,
                Err(e) => return Checked::now(e.clone()),
            };
            if let Some(failed) = shape_symbolization(status.stages()) {
                return Checked::now(failed);
            }
            if let Some(cached) = symbolization_cache.as_ref() {
                return cached.clone();
            }
            let checked = Checked::now(symbolize(settings, deadline, symbolize_task).await);
            if checked.status.cacheable() {
                *symbolization_cache = Some(checked.clone());
            }
            checked
        };
        let s3 = async {
            if let Some(off) = &s3_off {
                return Checked::now(off.clone());
            }
            match worker.as_ref().expect("waited for the worker") {
                Ok(_) => {}
                // No pipeline means no S3 stage.
                Err(CheckStatus::Disabled {
                    reason: DisabledReason::NoPipeline,
                    ..
                }) => {
                    return Checked::now(CheckStatus::disabled(
                        DisabledReason::NoS3Stage,
                        "no pipeline, so no S3 stage",
                    ));
                }
                Err(e) => return Checked::now(e.clone()),
            }
            if let Some((expires, checked)) = s3_cache.as_ref()
                && Instant::now() < *expires
            {
                return checked.clone();
            }
            let checked = Checked::now(upload(handle, settings, deadline).await);
            *s3_cache = checked
                .status
                .cacheable()
                .then(|| (Instant::now() + settings.cache_ttl, checked.clone()));
            checked
        };
        tokio::join!(symbolization, s3)
    }

    /// Wait for every stage's `initialize()` to return.
    async fn wait_for_running(
        handle: &Dial9Handle,
        deadline: Instant,
    ) -> Result<PipelineStatus, CheckStatus> {
        loop {
            let Some(status) = handle.pipeline_status() else {
                return Err(CheckStatus::disabled(
                    DisabledReason::NoPipeline,
                    "no pipeline worker",
                ));
            };
            match status.worker() {
                WorkerState::Running => return Ok(status),
                WorkerState::Stopped => {
                    return Err(CheckStatus::failed("pipeline worker not running"));
                }
                WorkerState::Initializing { stage, .. } if Instant::now() >= deadline => {
                    return Err(CheckStatus::failed(format!(
                        "pipeline worker still initializing ({})",
                        stage.unwrap_or("not started")
                    )));
                }
                // Every worker is briefly initializing at start.
                WorkerState::Initializing { .. } => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                other => {
                    return Err(CheckStatus::failed(format!(
                        "unrecognized pipeline worker state: {other:?}"
                    )));
                }
            }
        }
    }

    /// Why the stage order can't symbolize, if it can't.
    fn shape_symbolization(stages: &[&'static str]) -> Option<CheckStatus> {
        let Some(symbolize) = stages.iter().position(|s| *s == SYMBOLIZE) else {
            return Some(CheckStatus::failed("pipeline has no symbolize stage"));
        };
        match stages.iter().position(|s| *s == GZIP) {
            Some(gzip) if gzip < symbolize => Some(CheckStatus::failed(
                "segment compressed before symbolization",
            )),
            _ => None,
        }
    }

    /// The symbolization status when it doesn't need the pipeline, if any.
    #[cfg(feature = "cpu-profiling")]
    fn symbolization_precheck(handle: &Dial9Handle, settings: &Settings) -> Option<CheckStatus> {
        use dial9_perf_self_profile::{CpuProfiler, SchedProfiler, StartFailed};
        if !settings.symbolization {
            return Some(CheckStatus::turned_off());
        }
        if let Some(gone) = super::recorder_gone(handle) {
            return Some(gone);
        }
        if !super::HAS_BACKEND {
            return Some(CheckStatus::disabled(
                DisabledReason::Unsupported,
                "no symbolizer on this platform",
            ));
        }
        // Only profilers that record stacks need symbols. A placeholder
        // counts: the app configured one.
        let has = |found: Option<()>| found.is_some();
        let stacks = has(handle.with_source(|_: &mut CpuProfiler| ()))
            || has(handle.with_source(|_: &mut SchedProfiler| ()))
            || has(handle.with_source(|_: &mut StartFailed<CpuProfiler>| ()))
            || has(handle.with_source(|_: &mut StartFailed<SchedProfiler>| ()));
        (!stacks).then(|| {
            CheckStatus::disabled(
                DisabledReason::NotConfigured,
                "no stack-recording profiler, nothing to symbolize",
            )
        })
    }

    #[cfg(not(feature = "cpu-profiling"))]
    fn symbolization_precheck(_handle: &Dial9Handle, settings: &Settings) -> Option<CheckStatus> {
        Some(match settings.symbolization {
            false => CheckStatus::turned_off(),
            true => CheckStatus::disabled(
                DisabledReason::NotCompiled,
                "built without the `cpu-profiling` feature",
            ),
        })
    }

    #[cfg(feature = "cpu-profiling")]
    async fn symbolize(
        settings: &Settings,
        deadline: Instant,
        task: &mut SymbolizeTask,
    ) -> CheckStatus {
        use crate::telemetry::smoke_probe::{SymbolizationProbe, probe_symbolization};
        // A probe that timed out keeps running: wait for it on the next run
        // instead of starting a second symbolizer.
        let running = task.get_or_insert_with(|| tokio::task::spawn_blocking(probe_symbolization));
        let Ok(result) = tokio::time::timeout_at(deadline, running).await else {
            return settings.timed_out();
        };
        *task = None;
        match result {
            Ok(SymbolizationProbe::Resolved(name)) => name_status(&name),
            Ok(SymbolizationProbe::NoEntry) => {
                CheckStatus::failed("no symbol table entry for the probe address")
            }
            Ok(SymbolizationProbe::Error(e)) => CheckStatus::failed(e),
            Err(e) => CheckStatus::failed(format!("symbolization probe panicked: {e}")),
        }
    }

    #[cfg(not(feature = "cpu-profiling"))]
    async fn symbolize(
        _settings: &Settings,
        _deadline: Instant,
        _task: &mut SymbolizeTask,
    ) -> CheckStatus {
        unreachable!("symbolization_precheck disables the check without `cpu-profiling`")
    }

    /// Status for the name the probe address resolved to.
    #[cfg(feature = "cpu-profiling")]
    fn name_status(name: &str) -> CheckStatus {
        if name.starts_with("[unknown]") {
            CheckStatus::degraded(
                super::DegradedReason::NoSymbolTable,
                "binary has no symbol table: CPU stacks will show raw addresses",
            )
        } else if name.starts_with("[symbolize-failed]") {
            CheckStatus::failed(name)
        } else if name.contains("symbolization_probe") {
            CheckStatus::Ok
        } else {
            CheckStatus::failed(format!("probe resolved to the wrong symbol: {name}"))
        }
    }

    /// The S3 status when it doesn't need the pipeline, if any.
    fn s3_precheck(handle: &Dial9Handle, settings: &Settings) -> Option<CheckStatus> {
        if !settings.s3_upload {
            return Some(CheckStatus::turned_off());
        }
        if let Some(gone) = super::recorder_gone(handle) {
            return Some(gone);
        }
        s3_targets_precheck(handle)
    }

    #[cfg(feature = "worker-s3")]
    fn s3_targets_precheck(handle: &Dial9Handle) -> Option<CheckStatus> {
        handle
            .with_source(|_: &mut super::PipelineS3Targets| ())
            .is_none()
            .then(|| CheckStatus::disabled(DisabledReason::NoS3Stage, "no dial9 S3 stage"))
    }

    #[cfg(not(feature = "worker-s3"))]
    fn s3_targets_precheck(_handle: &Dial9Handle) -> Option<CheckStatus> {
        Some(CheckStatus::disabled(
            DisabledReason::NotCompiled,
            "built without the `worker-s3` feature",
        ))
    }

    /// Upload the marker to every S3 stage's bucket, with the client each
    /// stage published.
    #[cfg(feature = "worker-s3")]
    async fn upload(handle: &Dial9Handle, settings: &Settings, deadline: Instant) -> CheckStatus {
        let Some(targets) =
            handle.with_source(|t: &mut super::PipelineS3Targets| t.targets.clone())
        else {
            return CheckStatus::disabled(DisabledReason::NoS3Stage, "no dial9 S3 stage");
        };
        // A stage outside the running pipeline (an S3 uploader replaced by a
        // later one, or by a custom pipeline) never initialized.
        let ready: Vec<_> = targets
            .into_iter()
            .filter_map(|t| t.client.get().map(|client| (t.config, client)))
            .collect();
        if ready.is_empty() {
            return CheckStatus::disabled(
                DisabledReason::NoS3Stage,
                "no dial9 S3 stage in the running pipeline",
            );
        }
        let several = ready.len() > 1;
        let mut failures = Vec::new();
        for (config, client) in &ready {
            let left = deadline.saturating_duration_since(Instant::now());
            // The worker wait used the whole budget: report that, not an
            // SDK timeout.
            if left.is_zero() {
                return CheckStatus::failed(format!(
                    "timed out after {:?} waiting for the pipeline worker",
                    settings.check_timeout
                ));
            }
            if let Err(e) = config.upload_smoke_marker(client, left).await {
                failures.push(match several {
                    true => format!("bucket {}: {}", e.bucket(), e.detail()),
                    false => e.detail().to_string(),
                });
            }
        }
        match failures.is_empty() {
            true => CheckStatus::Ok,
            false => CheckStatus::failed(failures.join("; ")),
        }
    }

    #[cfg(not(feature = "worker-s3"))]
    async fn upload(
        _handle: &Dial9Handle,
        _settings: &Settings,
        _deadline: Instant,
    ) -> CheckStatus {
        unreachable!("s3_precheck disables the check without `worker-s3`")
    }
}

#[cfg(feature = "cpu-profiling")]
mod sampling {
    use super::{
        CheckStatus, CpuBackend, DegradedReason, DisabledReason, Settings, UnverifiedReason,
    };
    use crate::telemetry::recorder::on_attached_runtime;
    use crate::telemetry::smoke_probe::burn_thread_cpu;
    use dial9_core::handle::Dial9Handle;
    use dial9_perf_self_profile::{ActiveCpuBackend, CpuProfiler, StartFailed};
    use std::time::Duration;
    use tokio::time::Instant;

    /// Poll interval; the flush thread drains sources every ~5ms.
    const POLL: Duration = Duration::from_millis(10);
    /// How long to wait for the stimulus's samples to be drained.
    const DRAIN_WAIT: Duration = Duration::from_millis(300);
    /// Name of the stimulus thread; its CPU samples carry it.
    pub(crate) const STIMULUS_THREAD: &str = "dial9-smoke";
    /// Longest the stimulus thread stays tracked after its burn.
    const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

    fn samples_seen(handle: &Dial9Handle) -> Option<u64> {
        handle.with_source(|p: &mut CpuProfiler| p.samples_seen())
    }

    /// Wait up to `window` for the counter to pass `before`.
    async fn moved(handle: &Dial9Handle, before: u64, window: Duration) -> bool {
        let deadline = Instant::now() + window;
        loop {
            if samples_seen(handle).is_some_and(|now| now > before) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub(super) fn cpu_backend(handle: &Dial9Handle) -> Option<CpuBackend> {
        match handle.with_source(|p: &mut CpuProfiler| p.effective_backend())? {
            ActiveCpuBackend::Perf => Some(CpuBackend::Perf),
            ActiveCpuBackend::Ctimer => Some(CpuBackend::Ctimer),
            other => {
                crate::rate_limit::rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("smoke test: unrecognized CPU backend {other:?}");
                });
                None
            }
        }
    }

    pub(super) async fn check(handle: &Dial9Handle, settings: &Settings) -> CheckStatus {
        let Some(before) = samples_seen(handle) else {
            return absent(handle);
        };
        let backend = cpu_backend(handle);
        let working = || match backend {
            Some(CpuBackend::Ctimer) => CheckStatus::degraded(
                DegradedReason::CtimerFallback,
                "ctimer fallback: perf_event_open unavailable, only dial9-tracked threads are \
                 sampled",
            ),
            _ => CheckStatus::Ok,
        };
        // A busy service produces samples on its own.
        if moved(handle, before, settings.passive_window).await {
            return working();
        }
        if !settings.stimulus {
            return CheckStatus::unverified(
                UnverifiedReason::StimulusOff,
                "no CPU samples while idle, and the stimulus is off",
            );
        }
        let (stimulus, release) = match stimulate(handle, settings.stimulus_cpu_time).await {
            Ok(ran) => ran,
            Err(why) => {
                return CheckStatus::unverified(
                    UnverifiedReason::StimulusUnavailable,
                    format!("no CPU samples while idle, and the stimulus couldn't run: {why}"),
                );
            }
        };
        let got = moved(handle, before, DRAIN_WAIT).await;
        // Drained: the stimulus thread may untrack and exit.
        drop(release);
        if got {
            return working();
        }
        // ctimer samples only tracked threads.
        if backend == Some(CpuBackend::Ctimer)
            && let Some(e) = &stimulus.enroll_error
        {
            return CheckStatus::failed(format!("could not enroll the stimulus thread: {e}"));
        }
        if stimulus.spent < settings.stimulus_cpu_time {
            return CheckStatus::unverified(
                UnverifiedReason::HostContended,
                format!(
                    "burn fell short ({:?} of {:?}): host too contended",
                    stimulus.spent, settings.stimulus_cpu_time
                ),
            );
        }
        if backend == Some(CpuBackend::Perf) && stimulus.outside_perf_tree {
            return CheckStatus::unverified(
                UnverifiedReason::OutsidePerfTree,
                "no CPU samples from the stimulus thread, which may be outside perf's thread \
                 tree: call from a dial9-attached runtime",
            );
        }
        CheckStatus::failed("no CPU samples, even after a stimulus")
    }

    /// What the stimulus did.
    struct Stimulus {
        /// CPU time the burn consumed.
        spent: Duration,
        /// Why `track_current_thread` refused the thread, if it did.
        enroll_error: Option<String>,
        /// Spawned outside an attached runtime, so possibly outside perf's
        /// thread tree.
        outside_perf_tree: bool,
    }

    /// Burn on a new `dial9-smoke` thread. The returned sender keeps the
    /// thread tracked and alive until dropped, so its samples drain with its
    /// name. `Err` when the thread couldn't run.
    async fn stimulate(
        handle: &Dial9Handle,
        cpu_time: Duration,
    ) -> Result<(Stimulus, std::sync::mpsc::Sender<()>), String> {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<(Duration, Option<String>)>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread_handle = handle.clone();
        let body = move || {
            // Burn even when enrollment fails: perf samples the thread anyway.
            let (guard, enroll_error) = match thread_handle.track_current_thread() {
                Ok(guard) => (Some(guard), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let spent = burn_thread_cpu(cpu_time, cpu_time * 20);
            if done_tx.send((spent, enroll_error)).is_err() {
                tracing::debug!("smoke test gave up before the stimulus finished");
            }
            // Untracking closes the thread's perf buffer without draining it,
            // and an exited thread's name can't be read.
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                release_rx.recv_timeout(RELEASE_TIMEOUT)
            {
                tracing::debug!("smoke stimulus thread released by timeout");
            }
            drop(guard);
        };
        let spawn = move || {
            std::thread::Builder::new()
                .name(STIMULUS_THREAD.to_string())
                .spawn(body)
                .map(drop)
        };
        // perf samples only threads descended from the one that started the
        // profiler. Workers of an attached runtime are; the caller's own
        // thread may predate the profiler.
        let attached = on_attached_runtime(handle);
        let spawned = if attached {
            tokio::spawn(async move { spawn() })
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))
                .and_then(|spawned| spawned)
        } else {
            spawn()
        };
        spawned.map_err(|e| format!("could not spawn the stimulus thread: {e}"))?;
        let (spent, enroll_error) = done_rx
            .await
            .map_err(|_| "stimulus thread exited early".to_string())?;
        Ok((
            Stimulus {
                spent,
                enroll_error,
                outside_perf_tree: !attached,
            },
            release_tx,
        ))
    }

    /// Status when no CPU profiler is registered: it failed to start, or was
    /// never configured.
    fn absent(handle: &Dial9Handle) -> CheckStatus {
        match handle.with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_string()) {
            // The kind alone can't tell: blocked perf on Linux can also be
            // `Unsupported`.
            Some(msg) if !super::HAS_BACKEND => {
                CheckStatus::disabled(DisabledReason::Unsupported, msg)
            }
            Some(msg) => CheckStatus::failed(format!("CPU profiling failed to start: {msg}")),
            None => {
                CheckStatus::disabled(DisabledReason::NotConfigured, "no CPU profiler configured")
            }
        }
    }
}

#[cfg(not(feature = "cpu-profiling"))]
mod sampling {
    use super::{CheckStatus, CpuBackend, DisabledReason, Settings};
    use dial9_core::handle::Dial9Handle;

    pub(super) async fn check(_handle: &Dial9Handle, _settings: &Settings) -> CheckStatus {
        CheckStatus::disabled(
            DisabledReason::NotCompiled,
            "built without the `cpu-profiling` feature",
        )
    }

    pub(super) fn cpu_backend(_handle: &Dial9Handle) -> Option<CpuBackend> {
        None
    }
}
