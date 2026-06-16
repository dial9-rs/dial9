//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! Start with [`Dial9Config::builder()`] and pick a writer mode:
//! [`on_disk_buffer`](Dial9ConfigBuilder::on_disk_buffer) for disk or
//! [`in_memory_buffer`](Dial9ConfigBuilder::in_memory_buffer). Each returns a builder
//! carrying only that mode's knobs, so disk and in-memory settings can't be
//! mixed. `with_tokio` and `with_runtime` reach the underlying
//! [`tokio::runtime::Builder`] and [`TracedRuntimeBuilder`]; `.enabled(false)`
//! turns telemetry off while keeping a plain tokio runtime. `graceful_shutdown`
//! / `disable_graceful_shutdown` tune the implicit drain the `#[main]` macro
//! runs after the async body returns (default 1s).
//!
//! Two finish functions cover the strict / lenient axis:
//!
//! - `build` — strict. Returns a `Result<Dial9Config, Dial9ConfigBuilderError>`.
//!   Both required-field validation and the writer's I/O probing happen here, so any error
//!   surfaces at config-build time before the runtime is touched.
//! - `build_or_disabled` — lenient. Returns a [`Dial9Config`] that
//!   is *infallible at build time*: validation or I/O failures are logged at `error!` level
//!   and downgraded to a disabled config that still carries the user's `with_tokio` configurators.
//!
//! To run without telemetry while preserving tokio knobs, call
//! `.enabled(false)` — the builder then skips required-field validation
//! and any queued runtime configurators are ignored.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::telemetry::recorder::{
    BuildAndStartRuntime, HasTracePath, PipelineUnset, TelemetryGuard, TracedRuntime,
    TracedRuntimeBuilder,
};
use crate::telemetry::writer::{
    Disk, DiskWriter, InMemoryWriter, Memory, SegmentWriter, WriterMode,
};

/// Type-erased terminal step: a closure that builds and starts the runtime,
/// capturing the configured [`TracedRuntimeBuilder`] and its writer at the
/// point both the pipeline marker `M` and writer `Mode` are concrete. Keeps
/// [`Inner::Enabled`] non-generic across disk and in-memory configs.
///
/// Built where `M`/`Mode` are known, so `build_and_start` resolves to the
/// right per-pipeline-state method (the no-pipeline state infers `Mode` from
/// the writer, the mode-bound states require a matching writer).
pub(crate) type RuntimeBuilderFn = Box<
    dyn FnOnce(
            tokio::runtime::Builder,
        ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)>
        + Send,
>;

// ---------------------------------------------------------------------------
// Dial9ConfigBuilderError — unified error for builder validation and writer I/O
// ---------------------------------------------------------------------------

/// Errors produced while building a [`Dial9Config`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Dial9ConfigBuilderError {
    /// Telemetry is enabled (the default) but one or more required writer
    /// fields were never set on the builder.
    Validation(ValidationError),
    /// Failure constructing the writer backing telemetry — for example, an
    /// unwritable `base_path`.
    Io(std::io::Error),
}

/// Opaque payload for [`Dial9ConfigBuilderError::Validation`].
#[derive(Debug)]
pub struct ValidationError {
    fields: Vec<&'static str>,
}

impl ValidationError {
    /// The names of the required builder setters that were not called.
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing required Dial9Config fields: {}",
            self.fields.join(", ")
        )
    }
}

impl std::fmt::Display for Dial9ConfigBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dial9ConfigBuilderError::Validation(v) => write!(f, "{v}"),
            Dial9ConfigBuilderError::Io(e) => write!(f, "rotating writer: {e}"),
        }
    }
}

impl std::error::Error for Dial9ConfigBuilderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Dial9ConfigBuilderError::Validation(_) => None,
            Dial9ConfigBuilderError::Io(e) => Some(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9Config::builder()`] followed by either:
///
/// - `build` — strict; returns
///   `Result<Dial9Config, Dial9ConfigBuilderError>`. The writer is probed
///   eagerly inside `build`, so any I/O failure surfaces here rather than
///   later when the runtime is built.
/// - `build_or_disabled` — lenient; never reports a build error, downgrades
///   to a disabled config that preserves the user's `with_tokio`
///   configurators on validation or I/O failure.
#[derive(Debug)]
pub struct Dial9Config {
    pub(crate) inner: Inner,
    /// Graceful-shutdown timeout applied by the `#[dial9_tokio_telemetry::main]`
    /// macro after the async body completes. `Some(timeout)` drains the
    /// background worker with that deadline; `None` skips the implicit drain
    /// (the guard's `Drop` still flushes and seals the final segment).
    ///
    /// Defaults to `Some(`[`DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT`]`)`.
    pub(crate) graceful_shutdown_timeout: Option<Duration>,
}

/// Default graceful-shutdown timeout used by the `#[dial9_tokio_telemetry::main]`
/// macro when the user does not override it.
pub(crate) const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// A configurator closure that customizes a [`tokio::runtime::Builder`].
///
/// Stored as `Arc<dyn Fn ...>` so that the configurator vector is cheaply
/// cloneable — the `build_or_disabled` path needs to preserve the
/// configurators on the disabled-fallback variant when validation or
/// writer-I/O setup fails.
pub(crate) type TokioConfigurator =
    Arc<dyn Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static>;

pub(crate) enum Inner {
    Enabled {
        tokio_configurators: Vec<TokioConfigurator>,
        runtime_builder: RuntimeBuilderFn,
    },
    Disabled {
        tokio_configurators: Vec<TokioConfigurator>,
    },
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inner::Enabled {
                tokio_configurators,
                runtime_builder: _,
            } => f
                .debug_struct("Enabled")
                .field("tokio_configurators", &tokio_configurators.len())
                .finish(),
            Inner::Disabled {
                tokio_configurators,
            } => f
                .debug_struct("Disabled")
                .field("tokio_configurators", &tokio_configurators.len())
                .finish(),
        }
    }
}

pub(crate) fn materialize_tokio_builder(
    configurators: &[TokioConfigurator],
) -> tokio::runtime::Builder {
    let mut b = default_tokio_builder();
    for c in configurators {
        c(&mut b);
    }
    b
}

// ---------------------------------------------------------------------------
// Dial9ConfigBuilder — writer-mode selector and per-mode builders
// ---------------------------------------------------------------------------

/// Deferred runtime config set by `with_runtime`, applied at `build()`: it
/// configures the seed builder, pairs it with the writer, and erases both into
/// a [`RuntimeBuilderFn`].
///
/// The seed starts in `Disk`, the default for [`TracedRuntime::builder`], so
/// the closure can call `with_custom_pipeline` / `with_s3_uploader`. The writer
/// then fixes the `Mode`, like `build_and_start(writer)` does in the non-macro
/// path.
type RuntimeFinalizer<Mode> = Box<
    dyn FnOnce(
        TracedRuntimeBuilder<HasTracePath, PipelineUnset, Disk>,
        SegmentWriter<Mode>,
    ) -> RuntimeBuilderFn,
>;

fn finalizer<F, N, Mode>(f: F) -> RuntimeFinalizer<Mode>
where
    Mode: WriterMode,
    F: FnOnce(
            TracedRuntimeBuilder<HasTracePath, PipelineUnset, Disk>,
        ) -> TracedRuntimeBuilder<HasTracePath, N, Mode>
        + 'static,
    N: Send + 'static,
    TracedRuntimeBuilder<HasTracePath, N, Mode>: BuildAndStartRuntime<Mode>,
{
    // Two stages. The outer closure runs at config-build time, when seed and
    // writer exist but the tokio handle does not. The inner one defers the
    // actual build until that handle is available.
    Box::new(move |seed, writer| {
        // `f(seed)` pins marker `N` concrete, so `build_and_start_runtime`
        // resolves to the right per-state method, captured into the inner closure.
        let built = f(seed);
        Box::new(move |tk| built.build_and_start_runtime(tk, writer))
    })
}

const ENV_DIAL9_ENABLED: &str = "DIAL9_ENABLED";
const ENV_DIAL9_TRACE_DIR: &str = "DIAL9_TRACE_DIR";
const ENV_DIAL9_ROTATION_SECS: &str = "DIAL9_ROTATION_SECS";
const ENV_DIAL9_MAX_DISK_USAGE_MB: &str = "DIAL9_MAX_DISK_USAGE_MB";
const ENV_DIAL9_MAX_FILE_SIZE_MB: &str = "DIAL9_MAX_FILE_SIZE_MB";
const ENV_DIAL9_TOKIO_INSTRUMENTATION_ENABLED: &str = "DIAL9_TOKIO_INSTRUMENTATION_ENABLED";
const ENV_DIAL9_TASK_TRACKING_ENABLED: &str = "DIAL9_TASK_TRACKING_ENABLED";
const ENV_DIAL9_RUNTIME_NAME: &str = "DIAL9_RUNTIME_NAME";
const ENV_DIAL9_S3_BUCKET: &str = "DIAL9_S3_BUCKET";
const ENV_DIAL9_SERVICE_NAME: &str = "DIAL9_SERVICE_NAME";
const ENV_DIAL9_S3_PREFIX: &str = "DIAL9_S3_PREFIX";
const ENV_DIAL9_CPU_PROFILE_ENABLED: &str = "DIAL9_CPU_PROFILE_ENABLED";
const ENV_DIAL9_CPU_SAMPLE_HZ: &str = "DIAL9_CPU_SAMPLE_HZ";
const ENV_DIAL9_SCHEDULE_PROFILE_ENABLED: &str = "DIAL9_SCHEDULE_PROFILE_ENABLED";
const ENV_DIAL9_TASK_DUMP_ENABLED: &str = "DIAL9_TASK_DUMP_ENABLED";
const ENV_DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS: &str = "DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS";
const ENV_DIAL9_PROCESS_RESOURCE_USAGE_ENABLED: &str = "DIAL9_PROCESS_RESOURCE_USAGE_ENABLED";
const ENV_DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS: &str =
    "DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS";
const ENV_DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED: &str = "DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED";
const ENV_DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS: &str =
    "DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS";
const ENV_DIAL9_GC_DEAD_NAMESPACES: &str = "DIAL9_GC_DEAD_NAMESPACES";

const DEFAULT_ENABLED: bool = false;
const DEFAULT_TRACE_DIR: &str = "/tmp/dial9-traces";
const DEFAULT_S3_PREFIX: &str = "dial9-traces";
const DEFAULT_MAX_DISK_USAGE_MB: u64 = 1024;
const DEFAULT_TASK_TRACKING_ENABLED: bool = true;
const DEFAULT_GC_DEAD_NAMESPACES: bool = true;
const DEFAULT_CPU_PROFILE_ENABLED: bool = cfg!(all(target_os = "linux", feature = "cpu-profiling"));
const DEFAULT_SCHEDULE_PROFILE_ENABLED: bool =
    cfg!(all(target_os = "linux", feature = "cpu-profiling"));
const DEFAULT_TASK_DUMP_ENABLED: bool = false;
const DEFAULT_PROCESS_RESOURCE_USAGE_ENABLED: bool = cfg!(unix);

const BYTES_PER_MIB: u64 = 1024 * 1024;

trait EnvSource {
    fn get(&self, name: &str) -> Result<String, std::env::VarError>;
}

struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, name: &str) -> Result<String, std::env::VarError> {
        std::env::var(name)
    }
}

impl<S: EnvSource + ?Sized> EnvSource for &S {
    fn get(&self, name: &str) -> Result<String, std::env::VarError> {
        (*self).get(name)
    }
}

#[derive(Debug)]
struct ParsedEnvConfig {
    enabled: Option<bool>,
    trace_dir: Option<PathBuf>,
    rotation_period: Option<Duration>,
    max_total_size: Option<u64>,
    max_file_size: Option<u64>,
    tokio_instrumentation_enabled: Option<bool>,
    task_tracking_enabled: Option<bool>,
    runtime_name: Option<String>,
    s3: Option<ParsedS3Config>,
    cpu_profile_enabled: Option<bool>,
    cpu_sample_hz: Option<u64>,
    schedule_profile_enabled: Option<bool>,
    task_dump_enabled: Option<bool>,
    task_dump_idle_threshold: Option<Duration>,
    process_resource_usage_enabled: Option<bool>,
    process_resource_usage_sample_interval: Option<Duration>,
    socket_accept_queues_enabled: Option<bool>,
    socket_accept_queues_sample_interval: Option<Duration>,
    gc_dead_namespaces: Option<bool>,
}

#[derive(Debug)]
#[cfg_attr(not(feature = "worker-s3"), allow(dead_code))]
struct ParsedS3Config {
    bucket: String,
    service_name: Option<String>,
    prefix: Option<String>,
}

#[derive(Debug)]
#[cfg_attr(not(feature = "worker-s3"), allow(dead_code))]
struct ResolvedS3Config {
    bucket: String,
    service_name: Option<String>,
    prefix: String,
}

#[derive(Debug)]
struct ResolvedEnvConfig {
    enabled: bool,
    trace_dir: PathBuf,

    // None means the underlying DiskWriter builder owns the default.
    rotation_period: Option<Duration>,

    max_total_size: u64,

    // None means the underlying DiskWriter builder owns the default.
    max_file_size: Option<u64>,

    tokio_instrumentation_enabled: Option<bool>,

    task_tracking_enabled: bool,

    // Optional config: None means do not set a runtime name.
    runtime_name: Option<String>,

    // Optional integration: None means do not configure S3 upload.
    s3: Option<ResolvedS3Config>,

    cpu_profile_enabled: bool,

    // None means CpuProfilingConfig::default() owns the sample rate.
    cpu_sample_hz: Option<u64>,

    schedule_profile_enabled: bool,
    task_dump_enabled: bool,

    // None means TaskDumpConfig::default() owns the idle threshold.
    task_dump_idle_threshold: Option<Duration>,

    process_resource_usage_enabled: bool,

    // None means ProcessResourceUsageConfig::default() owns the sample interval.
    process_resource_usage_sample_interval: Option<Duration>,

    // Optional source: Some(true) registers it; otherwise leave the builder untouched.
    socket_accept_queues_enabled: Option<bool>,

    // None means SocketAcceptQueuesConfig::default() owns the sample interval.
    socket_accept_queues_sample_interval: Option<Duration>,

    gc_dead_namespaces: bool,
}

struct RuntimeEnvConfig {
    tokio_instrumentation_enabled: Option<bool>,
    task_tracking_enabled: bool,
    runtime_name: Option<String>,
    cpu_profile_enabled: bool,
    #[cfg_attr(not(feature = "cpu-profiling"), allow(dead_code))]
    cpu_sample_hz: Option<u64>,
    schedule_profile_enabled: bool,
    task_dump_enabled: bool,
    task_dump_idle_threshold: Option<Duration>,
    process_resource_usage_enabled: bool,
    process_resource_usage_sample_interval: Option<Duration>,
    socket_accept_queues_enabled: Option<bool>,
    #[cfg_attr(not(feature = "linux-socket"), allow(dead_code))]
    socket_accept_queues_sample_interval: Option<Duration>,
}

fn parse_env_config(env: &impl EnvSource) -> ParsedEnvConfig {
    let env = EnvSourceParser::new(env);

    let max_total_size = env
        .get_positive_u64(ENV_DIAL9_MAX_DISK_USAGE_MB)
        .map(|mb| mb.saturating_mul(BYTES_PER_MIB));
    let max_file_size = env
        .get_positive_u64(ENV_DIAL9_MAX_FILE_SIZE_MB)
        .map(|mb| mb.saturating_mul(BYTES_PER_MIB));
    let s3 = env
        .get_string(ENV_DIAL9_S3_BUCKET)
        .map(|bucket| ParsedS3Config {
            bucket,
            service_name: env.get_string(ENV_DIAL9_SERVICE_NAME),
            prefix: env.get_string(ENV_DIAL9_S3_PREFIX),
        });

    ParsedEnvConfig {
        enabled: env.get_bool(ENV_DIAL9_ENABLED),
        trace_dir: env.get_string(ENV_DIAL9_TRACE_DIR).map(PathBuf::from),
        rotation_period: env
            .get_positive_u64(ENV_DIAL9_ROTATION_SECS)
            .map(Duration::from_secs),
        max_total_size,
        max_file_size,
        tokio_instrumentation_enabled: env.get_bool(ENV_DIAL9_TOKIO_INSTRUMENTATION_ENABLED),
        task_tracking_enabled: env.get_bool(ENV_DIAL9_TASK_TRACKING_ENABLED),
        runtime_name: env.get_string(ENV_DIAL9_RUNTIME_NAME),
        s3,
        cpu_profile_enabled: env.get_bool(ENV_DIAL9_CPU_PROFILE_ENABLED),
        cpu_sample_hz: env.get_positive_u64(ENV_DIAL9_CPU_SAMPLE_HZ),
        schedule_profile_enabled: env.get_bool(ENV_DIAL9_SCHEDULE_PROFILE_ENABLED),
        task_dump_enabled: env.get_bool(ENV_DIAL9_TASK_DUMP_ENABLED),
        task_dump_idle_threshold: env
            .get_positive_u64(ENV_DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS)
            .map(Duration::from_millis),
        process_resource_usage_enabled: env.get_bool(ENV_DIAL9_PROCESS_RESOURCE_USAGE_ENABLED),
        process_resource_usage_sample_interval: env
            .get_positive_u64(ENV_DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS)
            .map(Duration::from_millis),
        socket_accept_queues_enabled: env.get_bool(ENV_DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED),
        socket_accept_queues_sample_interval: env
            .get_positive_u64(ENV_DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS)
            .map(Duration::from_millis),
        gc_dead_namespaces: env.get_bool(ENV_DIAL9_GC_DEAD_NAMESPACES),
    }
}

fn resolve_env_config(parsed: ParsedEnvConfig) -> ResolvedEnvConfig {
    let max_total_size = parsed
        .max_total_size
        .unwrap_or_else(|| DEFAULT_MAX_DISK_USAGE_MB.saturating_mul(BYTES_PER_MIB));

    ResolvedEnvConfig {
        enabled: parsed.enabled.unwrap_or(DEFAULT_ENABLED),
        trace_dir: parsed
            .trace_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TRACE_DIR)),
        rotation_period: parsed.rotation_period,
        max_total_size,
        max_file_size: parsed.max_file_size,
        tokio_instrumentation_enabled: parsed.tokio_instrumentation_enabled,
        task_tracking_enabled: parsed
            .task_tracking_enabled
            .unwrap_or(DEFAULT_TASK_TRACKING_ENABLED),
        runtime_name: parsed.runtime_name,
        s3: parsed.s3.map(|s3| ResolvedS3Config {
            bucket: s3.bucket,
            service_name: s3.service_name,
            prefix: s3.prefix.unwrap_or_else(|| DEFAULT_S3_PREFIX.to_string()),
        }),
        cpu_profile_enabled: parsed
            .cpu_profile_enabled
            .unwrap_or(DEFAULT_CPU_PROFILE_ENABLED),
        cpu_sample_hz: parsed.cpu_sample_hz,
        schedule_profile_enabled: parsed
            .schedule_profile_enabled
            .unwrap_or(DEFAULT_SCHEDULE_PROFILE_ENABLED),
        task_dump_enabled: parsed
            .task_dump_enabled
            .unwrap_or(DEFAULT_TASK_DUMP_ENABLED),
        task_dump_idle_threshold: parsed.task_dump_idle_threshold,
        process_resource_usage_enabled: parsed
            .process_resource_usage_enabled
            .unwrap_or(DEFAULT_PROCESS_RESOURCE_USAGE_ENABLED),
        process_resource_usage_sample_interval: parsed.process_resource_usage_sample_interval,
        socket_accept_queues_enabled: parsed.socket_accept_queues_enabled,
        socket_accept_queues_sample_interval: parsed.socket_accept_queues_sample_interval,
        gc_dead_namespaces: parsed
            .gc_dead_namespaces
            .unwrap_or(DEFAULT_GC_DEAD_NAMESPACES),
    }
}

struct EnvSourceParser<S>(S);

impl<S> EnvSourceParser<S> {
    fn new(source: S) -> Self {
        Self(source)
    }
}

impl<S: EnvSource> EnvSourceParser<S> {
    fn get_bool(&self, name: &'static str) -> Option<bool> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected an explicit boolean value; ignoring"
            ));
            return None;
        }

        match value.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "y" | "yes" | "on" => Some(true),
            "f" | "false" | "0" | "n" | "no" | "off" => Some(false),
            _ => {
                warn(format_args!(
                    "dial9: {name}={value:?} is invalid; valid values are t,true,1,y,yes,on,f,false,0,n,no,off; ignoring"
                ));
                None
            }
        }
    }

    fn get_positive_u64(&self, name: &'static str) -> Option<u64> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected a positive integer; ignoring"
            ));
            return None;
        }

        match value.parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                warn(format_args!(
                    "dial9: {name}={value:?} is invalid; expected a positive integer; ignoring"
                ));
                None
            }
        }
    }

    fn get_string(&self, name: &'static str) -> Option<String> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected a non-empty value; ignoring"
            ));
            return None;
        }
        Some(value.to_string())
    }
}

#[cfg(feature = "worker-s3")]
fn default_service_name() -> String {
    if let Ok(path) = std::env::current_exe()
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && !stem.trim().is_empty()
    {
        return stem.to_string();
    }

    "unknown-service".to_string()
}

fn warn(message: fmt::Arguments<'_>) {
    if tracing::dispatcher::has_been_set() {
        tracing::warn!(target: "dial9_telemetry", "{message}");
    } else {
        eprintln!("{message}");
    }
}

fn error(message: fmt::Arguments<'_>) {
    if tracing::dispatcher::has_been_set() {
        tracing::error!(target: "dial9_telemetry", "{message}");
    } else {
        eprintln!("{message}");
    }
}

fn warn_not_unicode(name: &'static str) {
    warn(format_args!("dial9: {name} is not valid Unicode; ignoring"));
}

fn apply_runtime_env<M>(
    mut runtime: TracedRuntimeBuilder<HasTracePath, M>,
    config: RuntimeEnvConfig,
) -> TracedRuntimeBuilder<HasTracePath, M> {
    if let Some(name) = config.runtime_name {
        runtime = runtime.with_runtime_name(name);
    }
    if let Some(enabled) = config.tokio_instrumentation_enabled {
        runtime = runtime.with_tokio_instrumentation(enabled);
    }
    runtime = runtime.with_task_tracking(config.task_tracking_enabled);

    if config.task_dump_enabled {
        let task_dump_config = match config.task_dump_idle_threshold {
            Some(threshold) => crate::telemetry::TaskDumpConfig::builder()
                .idle_threshold(threshold)
                .build(),
            None => crate::telemetry::TaskDumpConfig::default(),
        };
        runtime = runtime.with_task_dumps(task_dump_config);
    }

    if config.process_resource_usage_enabled {
        let process_resource_usage_config = match config.process_resource_usage_sample_interval {
            Some(interval) => crate::telemetry::ProcessResourceUsageConfig::builder()
                .sample_interval(interval)
                .build(),
            None => crate::telemetry::ProcessResourceUsageConfig::default(),
        };
        runtime = runtime.with_process_resource_usage(process_resource_usage_config);
    }

    #[cfg(feature = "linux-socket")]
    if config.socket_accept_queues_enabled == Some(true) {
        let socket_accept_queues_config = match config.socket_accept_queues_sample_interval {
            Some(interval) => crate::telemetry::SocketAcceptQueuesConfig::builder()
                .sample_interval(interval)
                .build(),
            None => crate::telemetry::SocketAcceptQueuesConfig::default(),
        };
        runtime = runtime.with_socket_accept_queues(socket_accept_queues_config);
    }

    #[cfg(not(feature = "linux-socket"))]
    if config.socket_accept_queues_enabled == Some(true) {
        warn(format_args!(
            "dial9: socket accept queues requested but `linux-socket` feature is not enabled; ignoring"
        ));
    }

    #[cfg(feature = "cpu-profiling")]
    {
        use crate::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};

        if config.cpu_profile_enabled {
            let cpu_config = match config.cpu_sample_hz {
                Some(hz) => CpuProfilingConfig::default().frequency_hz(hz),
                None => CpuProfilingConfig::default(),
            };
            runtime = runtime.with_cpu_profiling(cpu_config);
        }
        if config.schedule_profile_enabled {
            runtime = runtime.with_sched_events(SchedEventConfig::default());
        }
    }

    #[cfg(not(feature = "cpu-profiling"))]
    if config.cpu_profile_enabled || config.schedule_profile_enabled {
        warn(format_args!(
            "dial9: CPU/schedule profiling requested but `cpu-profiling` feature is not enabled; ignoring"
        ));
    }

    runtime
}

#[cfg(feature = "worker-s3")]
fn build_s3_config(config: ResolvedS3Config) -> crate::background_task::s3::S3Config {
    crate::background_task::s3::S3Config::builder()
        .bucket(config.bucket)
        .service_name(config.service_name.unwrap_or_else(default_service_name))
        .prefix(config.prefix)
        .build()
}

fn default_tokio_builder() -> tokio::runtime::Builder {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    b.enable_all();
    b
}

impl Dial9Config {
    /// Build a production-oriented config from standard `DIAL9_*` environment variables.
    ///
    /// # Per-process namespace isolation
    ///
    /// On the disk path, segments are written to a per-process subdirectory
    /// `{DIAL9_TRACE_DIR}/{boot_id}/`, where `boot_id` is `{4-alpha}-{pid}`
    /// (e.g. `qmxz-48291`). This keeps processes that share a trace directory
    /// from reading and re-uploading each other's segments. Each process holds
    /// an advisory `flock` on `{boot_id}/.lock` for its lifetime; on startup it
    /// reclaims any sibling namespace whose lock it can acquire (i.e. the owner
    /// has exited). Set `DIAL9_GC_DEAD_NAMESPACES=false` to keep prior runs'
    /// directories instead — handy locally when comparing traces across runs.
    ///
    /// Supported local trace writer variables:
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_ENABLED` | `false` | Master switch for installing telemetry. |
    /// | `DIAL9_TRACE_DIR` | `/tmp/dial9-traces` | Directory for rotated trace segments. |
    /// | `DIAL9_ROTATION_SECS` | `60` | Rotation period in seconds, measured monotonically from writer start. |
    /// | `DIAL9_MAX_DISK_USAGE_MB` | `1024` | Total on-disk trace budget in MiB. |
    /// | `DIAL9_MAX_FILE_SIZE_MB` | `min(100, total / 4)` | Per-file trace segment size in MiB. |
    /// | `DIAL9_GC_DEAD_NAMESPACES` | `true` | Reclaim dead peers' namespace dirs at startup. |
    ///
    /// Supported runtime variables:
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_TASK_TRACKING_ENABLED` | `true` | Track tasks spawned through dial9 handles. |
    /// | `DIAL9_TOKIO_INSTRUMENTATION_ENABLED` | `true` | Install dial9's Tokio runtime hook instrumentation. |
    /// | `DIAL9_RUNTIME_NAME` | unset | Human-readable runtime name in trace metadata. |
    ///
    /// Supported S3 variables (`worker-s3` feature required):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_S3_BUCKET` | unset | Upload sealed trace segments to this bucket. |
    /// | `DIAL9_SERVICE_NAME` | binary name | Service name used in S3 keys and metadata. |
    /// | `DIAL9_S3_PREFIX` | `dial9-traces` | S3 object key prefix. |
    ///
    /// Supported CPU profiling variables (`cpu-profiling` feature required):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_CPU_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable CPU stack sampling. |
    /// | `DIAL9_CPU_SAMPLE_HZ` | `99` | CPU sampling frequency in Hz. |
    /// | `DIAL9_SCHEDULE_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable per-worker scheduler event capture. Requires the [CPU profiling setup](https://github.com/dial9-rs/dial9/blob/HEAD/dial9-tokio-telemetry/README.md#cpu-profiling-linux-only). |
    ///
    /// Supported process resource usage variables:
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_PROCESS_RESOURCE_USAGE_ENABLED` | `true` on Unix, `false` otherwise | Enable process resource usage sampling from `getrusage(RUSAGE_SELF)`. |
    /// | `DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS` | `100` | Sampling interval in milliseconds. |
    ///
    /// Supported socket accept queue variables (`linux-socket` feature required):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED` | `false` | Enable TCP accept queue snapshots from Linux sock_diag. |
    /// | `DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS` | `400` | Sampling interval in milliseconds. |
    ///
    /// Supported task dump variables (capture requires the `taskdump` feature):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_TASK_DUMP_ENABLED` | `false` | Capture async task dumps at idle yield points. |
    /// | `DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS` | `10` | Mean idle duration for task dump sampling. |
    ///
    /// Missing variables use defaults. Blank, invalid, or non-Unicode values
    /// emit a warning and are treated as missing. Some numeric defaults come
    /// from the underlying config builders and are listed here as the current
    /// `from_env()` behavior. The returned config is built with
    /// [`DiskConfigBuilder::build_or_disabled`], so writer setup failures are
    /// logged and downgraded to a plain Tokio runtime.
    pub fn from_env() -> Self {
        Self::from_env_source(&ProcessEnv)
    }

    fn from_env_source(env: &impl EnvSource) -> Self {
        let ResolvedEnvConfig {
            enabled,
            trace_dir,
            rotation_period,
            max_total_size,
            max_file_size,
            tokio_instrumentation_enabled,
            task_tracking_enabled,
            runtime_name,
            s3,
            cpu_profile_enabled,
            cpu_sample_hz,
            schedule_profile_enabled,
            task_dump_enabled,
            task_dump_idle_threshold,
            process_resource_usage_enabled,
            process_resource_usage_sample_interval,
            socket_accept_queues_enabled,
            socket_accept_queues_sample_interval,
            gc_dead_namespaces,
        } = resolve_env_config(parse_env_config(env));

        let runtime_config = RuntimeEnvConfig {
            tokio_instrumentation_enabled,
            task_tracking_enabled,
            runtime_name,
            cpu_profile_enabled,
            cpu_sample_hz,
            schedule_profile_enabled,
            task_dump_enabled,
            task_dump_idle_threshold,
            process_resource_usage_enabled,
            process_resource_usage_sample_interval,
            socket_accept_queues_enabled,
            socket_accept_queues_sample_interval,
        };

        // `from_env` only builds a disk writer for now.
        let builder = Self::builder()
            .on_disk_buffer(trace_dir.join("trace.bin"))
            .enabled(enabled)
            .maybe_max_file_size(max_file_size)
            .max_total_size(max_total_size)
            .gc_dead_namespaces(gc_dead_namespaces)
            .maybe_rotation_period(rotation_period);

        #[cfg(feature = "worker-s3")]
        let builder = match s3 {
            Some(s3) => builder.with_runtime(move |runtime| {
                apply_runtime_env(
                    runtime.with_s3_uploader(build_s3_config(s3)),
                    runtime_config,
                )
            }),
            None => builder.with_runtime(move |runtime| apply_runtime_env(runtime, runtime_config)),
        };

        #[cfg(not(feature = "worker-s3"))]
        let builder = {
            if s3.is_some() {
                warn(format_args!(
                    "dial9: S3 upload requested but `worker-s3` feature is not enabled; ignoring"
                ));
            }
            builder.with_runtime(move |runtime| apply_runtime_env(runtime, runtime_config))
        };

        builder.build_or_disabled()
    }
}

impl Dial9Config {
    /// Start a configuration chain by picking a writer mode:
    /// [`on_disk_buffer`](Dial9ConfigBuilder::on_disk_buffer) (disk) or
    /// [`in_memory_buffer`](Dial9ConfigBuilder::in_memory_buffer). Each hands back a builder
    /// carrying only the knobs that mode supports, so disk and in-memory
    /// settings can't be mixed. Either builder can be turned off with
    /// `.enabled(false)`.
    pub fn builder() -> Dial9ConfigBuilder {
        Dial9ConfigBuilder
    }
}

/// Writer-mode selector returned by [`Dial9Config::builder`].
///
/// Pick one mode: the choice determines which builder (and which knobs) you
/// get next. The mode mirrors the non-macro path, where you pick
/// [`DiskWriter`] or [`InMemoryWriter`] directly.
#[derive(Debug, Default)]
pub struct Dial9ConfigBuilder;

impl Dial9ConfigBuilder {
    /// Write trace segments to disk under `base_path`.
    ///
    /// `max_total_size` is required, `max_file_size` and
    /// `rotation_period` are optional.
    pub fn on_disk_buffer(self, base_path: impl Into<PathBuf>) -> DiskConfigBuilder {
        Dial9Config::disk(base_path.into())
    }

    /// Keep trace segments in process memory, with no filesystem usage.
    ///
    /// `max_total_size` is required.
    pub fn in_memory_buffer(self) -> MemoryConfigBuilder {
        Dial9Config::memory()
    }
}

// The per-mode builders. These bon start functions are `pub` (so the builder
// types they return are public) but `#[doc(hidden)]`: users reach them through
// the `Dial9ConfigBuilder` selector (`on_disk_buffer()` / `in_memory_buffer()`), not
// `Dial9Config::disk()` / `::memory()` directly.
#[bon::bon]
impl Dial9Config {
    /// Disk-writer builder. Reached via [`Dial9ConfigBuilder::on_disk_buffer`].
    #[doc(hidden)]
    #[builder(builder_type = DiskConfigBuilder, finish_fn = build, state_mod = disk_config_builder)]
    pub fn disk(
        /// Trace output path.
        #[builder(start_fn, into)]
        base_path: PathBuf,
        #[builder(field)] tokio_configurators: Vec<TokioConfigurator>,
        #[builder(field)] runtime_finalizer: Option<RuntimeFinalizer<Disk>>,
        #[builder(field = Some(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT))]
        graceful_shutdown_timeout: Option<Duration>,
        /// Defaults to `true`. When `false`, the writer fields are ignored and
        /// a plain tokio runtime is built without telemetry.
        #[builder(default = true)]
        enabled: bool,
        /// Per-file rotation threshold in bytes. Defaults to
        /// `min(100 MiB, max_total_size / 4)`.
        max_file_size: Option<u64>,
        /// Total disk budget in bytes. Required when enabled.
        max_total_size: Option<u64>,
        /// Rotation period, measured monotonically from writer start.
        rotation_period: Option<Duration>,
        /// Reclaim dead peers' namespace directories at startup. Defaults to
        /// `true`. Set `false` to keep trace files from previous runs around —
        /// useful for local development where you want to compare runs.
        #[builder(default = true)]
        gc_dead_namespaces: bool,
    ) -> Result<Dial9Config, Dial9ConfigBuilderError> {
        if !enabled {
            return Ok(Dial9Config {
                inner: Inner::Disabled {
                    tokio_configurators,
                },
                graceful_shutdown_timeout,
            });
        }
        let max_total_size = max_total_size.ok_or_else(|| {
            Dial9ConfigBuilderError::Validation(ValidationError {
                fields: vec!["max_total_size"],
            })
        })?;

        let namespace =
            crate::background_task::boot_id::setup_namespace(&base_path, gc_dead_namespaces)
                .map_err(Dial9ConfigBuilderError::Io)?;

        let mut writer = DiskWriter::builder()
            .base_path(namespace.trace_path.clone())
            .maybe_max_file_size(max_file_size)
            .max_total_size(max_total_size)
            .maybe_rotation_period(rotation_period)
            .build()
            .map_err(Dial9ConfigBuilderError::Io)?;

        let seed = TracedRuntime::builder().with_trace_path(namespace.trace_path.clone());
        writer.set_namespace(namespace.boot_id, namespace.lock);
        let runtime_builder: RuntimeBuilderFn = match runtime_finalizer {
            Some(finalize) => finalize(seed, writer),
            None => Box::new(move |tk| seed.build_and_start(tk, writer)),
        };

        Ok(Dial9Config {
            inner: Inner::Enabled {
                tokio_configurators,
                runtime_builder,
            },
            graceful_shutdown_timeout,
        })
    }

    /// In-memory-writer builder. Reached via [`Dial9ConfigBuilder::in_memory_buffer`].
    #[doc(hidden)]
    #[builder(builder_type = MemoryConfigBuilder, finish_fn = build, state_mod = memory_config_builder)]
    pub fn memory(
        #[builder(field)] tokio_configurators: Vec<TokioConfigurator>,
        #[builder(field)] runtime_finalizer: Option<RuntimeFinalizer<Memory>>,
        #[builder(field = Some(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT))]
        graceful_shutdown_timeout: Option<Duration>,
        /// Defaults to `true`. When `false`, the writer fields are ignored and
        /// a plain tokio runtime is built without telemetry.
        #[builder(default = true)]
        enabled: bool,
        /// Total in-memory budget in bytes (writer + non-cpu pipeline).
        /// Required when enabled.
        max_total_size: Option<u64>,
        /// Per-segment size. Defaults to a value dial9 picks from the budget.
        max_segment_size: Option<u64>,
        /// Rotation period, measured monotonically from writer start.
        rotation_period: Option<Duration>,
    ) -> Result<Dial9Config, Dial9ConfigBuilderError> {
        if !enabled {
            return Ok(Dial9Config {
                inner: Inner::Disabled {
                    tokio_configurators,
                },
                graceful_shutdown_timeout,
            });
        }
        let max_total_size = max_total_size.ok_or_else(|| {
            Dial9ConfigBuilderError::Validation(ValidationError {
                fields: vec!["max_total_size"],
            })
        })?;

        let writer = InMemoryWriter::builder()
            .max_total_size(max_total_size)
            .maybe_max_segment_size(max_segment_size)
            .maybe_rotation_period(rotation_period)
            .build()
            .map_err(Dial9ConfigBuilderError::Io)?;

        // Seed in `Disk` mode (where the pipeline-setting methods live); the
        // memory mode is reached by `with_custom_pipeline`/`with_s3_uploader`
        // in the closure, or inferred from the `InMemoryWriter` at build when no
        // pipeline is configured (the no-pipeline `build_and_start` infers Mode).
        let seed = TracedRuntime::builder().with_trace_path("mem");
        let runtime_builder: RuntimeBuilderFn = match runtime_finalizer {
            Some(finalize) => finalize(seed, writer),
            None => Box::new(move |tk| seed.build_and_start(tk, writer)),
        };

        Ok(Dial9Config {
            inner: Inner::Enabled {
                tokio_configurators,
                runtime_builder,
            },
            graceful_shutdown_timeout,
        })
    }
}

/// Doc text shared by both modes' `with_tokio`.
macro_rules! with_tokio_doc {
    () => {
        "Queue a configurator for the underlying [`tokio::runtime::Builder`].\n\nThe closure receives a fresh builder by mutable reference: use any tokio knob (`worker_threads`, `thread_name`, `thread_stack_size`, etc.). The builder is pre-seeded with `new_multi_thread()` and `enable_all()`. To switch flavors, replace the whole builder inside the closure.\n\nCan be called multiple times; configurators run in call order. The closure must be `Fn + Send + Sync + 'static` so `build_or_disabled` can preserve it on the disabled-fallback variant.\n\nSetting any of the 8 tokio runtime hooks here is silently overwritten by dial9's hooks; use `with_runtime` + `TracedRuntimeBuilder::with_tokio_hooks` to compose with them instead."
    };
}

/// Doc text shared by both modes' `graceful_shutdown`.
macro_rules! graceful_shutdown_doc {
    () => {
        "Set the graceful-shutdown timeout applied by `#[dial9_tokio_telemetry::main]`.\n\nAfter the async body returns, the macro drops the runtime (so Tokio worker threads exit and flush their thread-local buffers) and then calls [`TelemetryGuard::graceful_shutdown`](crate::telemetry::TelemetryGuard::graceful_shutdown) with this timeout, draining the background worker (symbolize, compress, upload) before the process exits.\n\nDefaults to 1 second. Call [`disable_graceful_shutdown`](Self::disable_graceful_shutdown) to skip the implicit drain. Has no effect on the low-level [`TracedRuntime`](crate::TracedRuntime) API, where you call `graceful_shutdown` yourself."
    };
}

/// Doc text shared by both modes' `disable_graceful_shutdown`.
macro_rules! disable_graceful_shutdown_doc {
    () => {
        "Skip the implicit graceful shutdown performed by `#[dial9_tokio_telemetry::main]`.\n\nWith graceful shutdown disabled the guard's `Drop` still flushes and seals the final segment, but the background worker is not drained (it exits without finishing symbolization/compression/upload of the last segment). The inverse of [`graceful_shutdown`](Self::graceful_shutdown)."
    };
}

impl<S: disk_config_builder::State> DiskConfigBuilder<S> {
    #[doc = with_tokio_doc!()]
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        self.tokio_configurators.push(Arc::new(f));
        self
    }

    #[doc = graceful_shutdown_doc!()]
    pub fn graceful_shutdown(mut self, timeout: Duration) -> Self {
        self.graceful_shutdown_timeout = Some(timeout);
        self
    }

    #[doc = disable_graceful_shutdown_doc!()]
    pub fn disable_graceful_shutdown(mut self) -> Self {
        self.graceful_shutdown_timeout = None;
        self
    }

    /// Set the configurator for the dial9 [`TracedRuntimeBuilder`].
    ///
    /// The closure receives the staged builder by value and must return it.
    /// Use this to access runtime configuration methods like
    /// `with_runtime_name`, `with_task_tracking`, `with_s3_uploader`, or
    /// `with_custom_pipeline`; see [`TracedRuntimeBuilder`] for the full list.
    ///
    /// The closure may transition the builder's pipeline-mode marker
    /// (e.g. by calling `.with_s3_uploader(...)` or
    /// `.with_custom_pipeline(...)`); the resulting mode is preserved
    /// through to runtime construction.
    ///
    /// The configurator is applied during `build()` once `base_path` is
    /// known. When `.enabled(false)` is set the configurator is ignored.
    /// Calling this method more than once replaces the prior closure.
    pub fn with_runtime<F, N>(mut self, f: F) -> Self
    where
        F: FnOnce(
                TracedRuntimeBuilder<HasTracePath, PipelineUnset, Disk>,
            ) -> TracedRuntimeBuilder<HasTracePath, N, Disk>
            + 'static,
        N: Send + 'static,
        TracedRuntimeBuilder<HasTracePath, N, Disk>: BuildAndStartRuntime<Disk>,
    {
        self.runtime_finalizer = Some(finalizer(f));
        self
    }
}

impl<S: memory_config_builder::State> MemoryConfigBuilder<S> {
    #[doc = with_tokio_doc!()]
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        self.tokio_configurators.push(Arc::new(f));
        self
    }

    #[doc = graceful_shutdown_doc!()]
    pub fn graceful_shutdown(mut self, timeout: Duration) -> Self {
        self.graceful_shutdown_timeout = Some(timeout);
        self
    }

    #[doc = disable_graceful_shutdown_doc!()]
    pub fn disable_graceful_shutdown(mut self) -> Self {
        self.graceful_shutdown_timeout = None;
        self
    }

    /// Configure the dial9 [`TracedRuntimeBuilder`] for this in-memory runtime.
    ///
    /// This is where the delivery pipeline is set, e.g.
    /// `with_runtime(|r| r.with_custom_pipeline(|p| p.pipe(my_uploader)))`.
    /// The closure must reach [`Memory`] mode (via `with_custom_pipeline` or
    /// `with_s3_uploader`), so disk-only pipeline steps like `write_back()` are
    /// a compile error. Calling this more than once replaces the prior closure.
    pub fn with_runtime<F, N>(mut self, f: F) -> Self
    where
        F: FnOnce(
                TracedRuntimeBuilder<HasTracePath, PipelineUnset, Disk>,
            ) -> TracedRuntimeBuilder<HasTracePath, N, Memory>
            + 'static,
        N: Send + 'static,
        TracedRuntimeBuilder<HasTracePath, N, Memory>: BuildAndStartRuntime<Memory>,
    {
        self.runtime_finalizer = Some(finalizer(f));
        self
    }
}

impl<S: disk_config_builder::IsComplete> DiskConfigBuilder<S> {
    /// Finish into a [`Dial9Config`] that never reports a build error.
    ///
    /// On any [`Dial9ConfigBuilderError`] (validation failure or writer
    /// I/O probe failure) logs an error and returns a [`Dial9Config`]
    /// in its disabled state with the user's `with_tokio` configurators
    /// preserved. The resulting config builds a plain tokio runtime
    /// when handed to [`crate::TracedRuntime::try_new`].
    ///
    /// # Panics (debug builds only)
    ///
    /// Panics on missing required fields to surface misconfigurations
    /// during development.
    ///
    /// Lenient counterpart to [`build`](Self::build). Use
    /// [`build`](Self::build) instead when you want validation and
    /// writer-I/O failures to surface as
    /// [`Dial9ConfigBuilderError`].
    pub fn build_or_disabled(self) -> Dial9Config {
        let fallback = self.tokio_configurators.clone();
        let graceful_shutdown_timeout = self.graceful_shutdown_timeout;
        downgrade_on_err(self.build(), fallback, graceful_shutdown_timeout)
    }
}

impl<S: memory_config_builder::IsComplete> MemoryConfigBuilder<S> {
    /// Finish into a [`Dial9Config`] that never reports a build error.
    ///
    /// On any [`Dial9ConfigBuilderError`] (validation failure or writer
    /// I/O probe failure) logs an error and returns a [`Dial9Config`]
    /// in its disabled state with the user's `with_tokio` configurators
    /// preserved. The resulting config builds a plain tokio runtime
    /// when handed to [`crate::TracedRuntime::try_new`].
    ///
    /// # Panics (debug builds only)
    ///
    /// Panics on missing required fields to surface misconfigurations
    /// during development.
    ///
    /// Lenient counterpart to [`build`](Self::build). Use
    /// [`build`](Self::build) instead when you want validation and
    /// writer-I/O failures to surface as
    /// [`Dial9ConfigBuilderError`].
    pub fn build_or_disabled(self) -> Dial9Config {
        let fallback = self.tokio_configurators.clone();
        let graceful_shutdown_timeout = self.graceful_shutdown_timeout;
        downgrade_on_err(self.build(), fallback, graceful_shutdown_timeout)
    }
}

fn downgrade_on_err(
    result: Result<Dial9Config, Dial9ConfigBuilderError>,
    fallback: Vec<TokioConfigurator>,
    graceful_shutdown_timeout: Option<Duration>,
) -> Dial9Config {
    match result {
        Ok(cfg) => cfg,
        Err(e) => {
            debug_assert!(
                !matches!(e, Dial9ConfigBuilderError::Validation(_)),
                "dial9 config validation failed: {e}"
            );
            error(format_args!(
                "dial9: telemetry config build failed; falling back to plain tokio runtime: {e}"
            ));
            Dial9Config {
                inner: Inner::Disabled {
                    tokio_configurators: fallback,
                },
                graceful_shutdown_timeout,
            }
        }
    }
}

/// Compile-fail assertions for the no-mix gating.
///
/// `max_file_size` is disk-only, rejected on the memory builder:
///
/// ```compile_fail
/// use dial9_tokio_telemetry::Dial9Config;
/// let _ = Dial9Config::builder().in_memory_buffer().max_file_size(1024);
/// ```
///
/// `write_back()` exists only for the disk writer:
///
/// ```compile_fail
/// use dial9_tokio_telemetry::Dial9Config;
/// let _ = Dial9Config::builder()
///     .in_memory_buffer()
///     .max_total_size(16 * 1024 * 1024)
///     .with_runtime(|r| r.with_custom_pipeline(|p| p.write_back()))
///     .build();
/// ```
#[cfg(doctest)]
struct InMemoryGatingTests;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::TracedRuntime;

    use super::*;

    fn tmp_base_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    /// A path under a directory that does not exist; DiskWriter::build()
    /// will fail to create the trace file there.
    fn unwritable_base_path() -> PathBuf {
        PathBuf::from("/this/dir/does/not/exist/dial9_test_trace.bin")
    }

    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<String, FakeEnvValue>,
    }

    enum FakeEnvValue {
        Unicode(String),
        NonUnicode,
    }

    impl FakeEnv {
        fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.vars
                .insert(name.into(), FakeEnvValue::Unicode(value.into()));
            self
        }

        fn with_non_unicode(mut self, name: impl Into<String>) -> Self {
            self.vars.insert(name.into(), FakeEnvValue::NonUnicode);
            self
        }
    }

    impl EnvSource for FakeEnv {
        fn get(&self, name: &str) -> Result<String, std::env::VarError> {
            match self.vars.get(name) {
                Some(FakeEnvValue::Unicode(value)) => Ok(value.clone()),
                Some(FakeEnvValue::NonUnicode) => Err(std::env::VarError::NotUnicode(
                    OsString::from("not unicode"),
                )),
                None => Err(std::env::VarError::NotPresent),
            }
        }
    }

    #[test]
    fn env_missing_values_are_unset() {
        let parsed = parse_env_config(&FakeEnv::default());

        assert_eq!(parsed.enabled, None);
        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
        assert_eq!(parsed.max_total_size, None);
        assert_eq!(parsed.max_file_size, None);
        assert_eq!(parsed.tokio_instrumentation_enabled, None);
        assert_eq!(parsed.task_tracking_enabled, None);
        assert_eq!(parsed.runtime_name, None);
        assert!(parsed.s3.is_none());
        assert_eq!(parsed.cpu_profile_enabled, None);
        assert_eq!(parsed.cpu_sample_hz, None);
        assert_eq!(parsed.schedule_profile_enabled, None);
        assert_eq!(parsed.task_dump_enabled, None);
        assert_eq!(parsed.task_dump_idle_threshold, None);
        assert_eq!(parsed.process_resource_usage_enabled, None);
        assert_eq!(parsed.process_resource_usage_sample_interval, None);
        assert_eq!(parsed.socket_accept_queues_enabled, None);
        assert_eq!(parsed.socket_accept_queues_sample_interval, None);
        assert_eq!(parsed.gc_dead_namespaces, None);
    }

    #[test]
    fn env_gc_dead_namespaces_parses_and_defaults() {
        let parsed =
            parse_env_config(&FakeEnv::default().with("DIAL9_GC_DEAD_NAMESPACES", "false"));
        assert_eq!(parsed.gc_dead_namespaces, Some(false));

        let resolved = resolve_env_config(parse_env_config(&FakeEnv::default()));
        assert_eq!(resolved.gc_dead_namespaces, DEFAULT_GC_DEAD_NAMESPACES);
    }

    #[test]
    fn env_resolution_applies_only_from_env_owned_defaults() {
        let resolved = resolve_env_config(parse_env_config(&FakeEnv::default()));
        let supported_profiling = cfg!(all(target_os = "linux", feature = "cpu-profiling"));

        assert_eq!(resolved.enabled, DEFAULT_ENABLED);
        assert_eq!(resolved.trace_dir, PathBuf::from(DEFAULT_TRACE_DIR));
        assert_eq!(
            resolved.max_total_size,
            DEFAULT_MAX_DISK_USAGE_MB * BYTES_PER_MIB
        );
        assert_eq!(
            resolved.task_tracking_enabled,
            DEFAULT_TASK_TRACKING_ENABLED
        );
        assert_eq!(resolved.tokio_instrumentation_enabled, None);
        assert_eq!(resolved.cpu_profile_enabled, supported_profiling);
        assert_eq!(resolved.schedule_profile_enabled, supported_profiling);
        assert_eq!(resolved.task_dump_enabled, DEFAULT_TASK_DUMP_ENABLED);
        assert_eq!(
            resolved.process_resource_usage_enabled,
            DEFAULT_PROCESS_RESOURCE_USAGE_ENABLED
        );
        assert_eq!(resolved.socket_accept_queues_enabled, None);

        // Optional config/integrations remain absent unless explicitly requested.
        assert_eq!(resolved.runtime_name, None);
        assert!(resolved.s3.is_none());

        // Delegated defaults remain unset so their underlying config types own them.
        assert_eq!(resolved.max_file_size, None);
        assert_eq!(resolved.rotation_period, None);
        assert_eq!(resolved.cpu_sample_hz, None);
        assert_eq!(resolved.task_dump_idle_threshold, None);
        assert_eq!(resolved.process_resource_usage_sample_interval, None);
        assert_eq!(resolved.socket_accept_queues_sample_interval, None);
    }

    #[test]
    fn env_parses_trimmed_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_ENABLED", " YES ")
                .with("DIAL9_TRACE_DIR", " /var/tmp/dial9 ")
                .with("DIAL9_ROTATION_SECS", "15")
                .with("DIAL9_MAX_DISK_USAGE_MB", "2048"),
        );

        assert_eq!(parsed.enabled, Some(true));
        assert_eq!(parsed.trace_dir, Some(PathBuf::from("/var/tmp/dial9")));
        assert_eq!(parsed.rotation_period, Some(Duration::from_secs(15)));
        assert_eq!(parsed.max_total_size, Some(2048 * 1024 * 1024));
        assert_eq!(parsed.max_file_size, None);
    }

    #[test]
    fn env_parses_runtime_storage_s3_cpu_and_taskdump_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_TOKIO_INSTRUMENTATION_ENABLED", "off")
                .with("DIAL9_TASK_TRACKING_ENABLED", "off")
                .with("DIAL9_RUNTIME_NAME", " api-runtime ")
                .with("DIAL9_MAX_FILE_SIZE_MB", "128")
                .with("DIAL9_S3_BUCKET", " traces-bucket ")
                .with("DIAL9_SERVICE_NAME", " checkout ")
                .with("DIAL9_S3_PREFIX", " prod/traces ")
                .with("DIAL9_CPU_PROFILE_ENABLED", "false")
                .with("DIAL9_CPU_SAMPLE_HZ", "199")
                .with("DIAL9_SCHEDULE_PROFILE_ENABLED", "false")
                .with("DIAL9_TASK_DUMP_ENABLED", "true")
                .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "25")
                .with("DIAL9_PROCESS_RESOURCE_USAGE_ENABLED", "true")
                .with("DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS", "250")
                .with("DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED", "true")
                .with("DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS", "1000"),
        );

        assert_eq!(parsed.tokio_instrumentation_enabled, Some(false));
        assert_eq!(parsed.task_tracking_enabled, Some(false));
        assert_eq!(parsed.runtime_name.as_deref(), Some("api-runtime"));
        assert_eq!(parsed.max_file_size, Some(128 * 1024 * 1024));
        let s3 = parsed.s3.expect("s3 config should be parsed");
        assert_eq!(s3.bucket, "traces-bucket");
        assert_eq!(s3.service_name.as_deref(), Some("checkout"));
        assert_eq!(s3.prefix.as_deref(), Some("prod/traces"));
        assert_eq!(parsed.cpu_profile_enabled, Some(false));
        assert_eq!(parsed.cpu_sample_hz, Some(199));
        assert_eq!(parsed.schedule_profile_enabled, Some(false));
        assert_eq!(parsed.task_dump_enabled, Some(true));
        assert_eq!(
            parsed.task_dump_idle_threshold,
            Some(Duration::from_millis(25))
        );
        assert_eq!(parsed.process_resource_usage_enabled, Some(true));
        assert_eq!(
            parsed.process_resource_usage_sample_interval,
            Some(Duration::from_millis(250))
        );
        assert_eq!(parsed.socket_accept_queues_enabled, Some(true));
        assert_eq!(
            parsed.socket_accept_queues_sample_interval,
            Some(Duration::from_millis(1000))
        );
    }

    #[test]
    fn env_allows_s3_bucket_without_service_name() {
        let parsed = parse_env_config(&FakeEnv::default().with("DIAL9_S3_BUCKET", "b"));

        let s3 = parsed.s3.expect("s3 config should be parsed");
        assert_eq!(s3.bucket, "b");
        assert_eq!(s3.service_name, None);
        assert_eq!(s3.prefix, None);
    }

    #[cfg(feature = "worker-s3")]
    #[test]
    fn env_s3_config_defaults_service_name_and_prefix_when_bucket_is_set() {
        let resolved = resolve_env_config(parse_env_config(
            &FakeEnv::default().with("DIAL9_S3_BUCKET", "b"),
        ));
        let s3 = resolved.s3.expect("s3 config should be resolved");
        assert_eq!(s3.prefix, DEFAULT_S3_PREFIX);

        let config = build_s3_config(s3);

        let metadata: HashMap<_, _> = config.as_metadata().collect();
        assert_eq!(metadata.get("bucket"), Some(&"b"));
        assert!(
            metadata
                .get("service_name")
                .is_some_and(|service_name| !service_name.is_empty())
        );
        assert_eq!(metadata.get("prefix"), Some(&DEFAULT_S3_PREFIX));
    }

    #[test]
    fn env_s3_config_preserves_explicit_prefix() {
        let resolved = resolve_env_config(parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_S3_BUCKET", "b")
                .with("DIAL9_S3_PREFIX", "custom-prefix"),
        ));

        let s3 = resolved.s3.expect("s3 config should be resolved");
        assert_eq!(s3.prefix, "custom-prefix");
    }

    #[test]
    fn env_ignores_blank_or_invalid_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_ENABLED", "maybe")
                .with("DIAL9_TOKIO_INSTRUMENTATION_ENABLED", "maybe")
                .with("DIAL9_TRACE_DIR", "   ")
                .with("DIAL9_ROTATION_SECS", "0")
                .with("DIAL9_MAX_DISK_USAGE_MB", "wat")
                .with("DIAL9_MAX_FILE_SIZE_MB", "0")
                .with("DIAL9_RUNTIME_NAME", "   ")
                .with("DIAL9_S3_BUCKET", "   ")
                .with("DIAL9_CPU_SAMPLE_HZ", "0")
                .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "wat")
                .with("DIAL9_PROCESS_RESOURCE_USAGE_ENABLED", "maybe")
                .with("DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS", "0")
                .with("DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED", "maybe")
                .with("DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS", "0"),
        );

        assert_eq!(parsed.enabled, None);
        assert_eq!(parsed.tokio_instrumentation_enabled, None);
        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
        assert_eq!(parsed.max_total_size, None);
        assert_eq!(parsed.max_file_size, None);
        assert_eq!(parsed.runtime_name, None);
        assert!(parsed.s3.is_none());
        assert_eq!(parsed.cpu_sample_hz, None);
        assert_eq!(parsed.task_dump_idle_threshold, None);
        assert_eq!(parsed.process_resource_usage_enabled, None);
        assert_eq!(parsed.process_resource_usage_sample_interval, None);
        assert_eq!(parsed.socket_accept_queues_enabled, None);
        assert_eq!(parsed.socket_accept_queues_sample_interval, None);
    }

    #[test]
    fn env_treats_non_unicode_values_as_invalid() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with_non_unicode("DIAL9_TRACE_DIR")
                .with_non_unicode("DIAL9_ROTATION_SECS"),
        );

        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
    }

    #[test]
    fn env_config_builds_disabled_by_default() {
        let cfg = Dial9Config::from_env_source(&FakeEnv::default());

        assert!(matches!(cfg.inner, Inner::Disabled { .. }));
    }

    #[test]
    fn env_config_builds_enabled_with_local_trace_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir);

        let cfg = Dial9Config::from_env_source(&env);

        assert!(
            matches!(cfg.inner, Inner::Enabled { .. }),
            "DIAL9_ENABLED + DIAL9_TRACE_DIR should produce an enabled config"
        );
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "config should keep telemetry enabled"
        );
        // With per-process namespace isolation, trace files land in a
        // boot_id subdirectory: {trace_dir}/{boot_id}/trace.0.bin.active
        let has_namespace_dir = std::fs::read_dir(dir.path())
            .expect("trace dir should exist")
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                crate::background_task::boot_id::is_valid_boot_id(&name)
                    && entry.path().is_dir()
                    && std::fs::read_dir(entry.path())
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().starts_with("trace."))
            });
        assert!(
            has_namespace_dir,
            "from_env should wire DIAL9_TRACE_DIR so trace segments land in <dir>/<boot_id>/"
        );
    }

    #[test]
    fn env_config_applies_runtime_name_and_task_dumps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir)
            .with("DIAL9_RUNTIME_NAME", " api-runtime ")
            .with("DIAL9_TASK_DUMP_ENABLED", "true")
            .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "25");

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let runtime_meta: Vec<(String, String)> = shared
            .sources
            .lock()
            .unwrap()
            .iter()
            .flat_map(|s| s.segment_metadata())
            .collect();
        let runtime_keys: Vec<&str> = runtime_meta
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| k.starts_with("runtime."))
            .collect();
        assert_eq!(
            runtime_keys,
            ["runtime.api-runtime"],
            "exactly one runtime, named from env, should surface in segment metadata"
        );
        assert!(shared.task_dumps_enabled.load(Ordering::Relaxed));
        assert_eq!(
            shared.task_dump_idle_threshold_ns.load(Ordering::Relaxed),
            25_000_000
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_config_enables_process_resource_usage_by_default_on_unix() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let trace_dir = dir
            .path()
            .to_str()
            .expect("temporary trace directory path should be valid UTF-8");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir);

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let sources = shared.sources.lock().unwrap();

        assert!(
            sources
                .iter()
                .any(|source| source.name() == "process_resource_usage"),
            "from_env should enable process resource usage by default on Unix"
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_config_can_disable_process_resource_usage() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let trace_dir = dir
            .path()
            .to_str()
            .expect("temporary trace directory path should be valid UTF-8");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir)
            .with("DIAL9_PROCESS_RESOURCE_USAGE_ENABLED", "false");

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let sources = shared.sources.lock().unwrap();

        assert!(
            sources
                .iter()
                .all(|source| source.name() != "process_resource_usage"),
            "explicit env opt-out should disable process resource usage"
        );
    }

    #[cfg(all(target_os = "linux", feature = "linux-socket"))]
    #[test]
    fn env_config_does_not_enable_socket_accept_queues_by_default() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let trace_dir = dir
            .path()
            .to_str()
            .expect("temporary trace directory path should be valid UTF-8");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir);

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let sources = shared.sources.lock().unwrap();

        assert!(
            sources
                .iter()
                .all(|source| source.name() != "socket_accept_queues"),
            "from_env should leave socket accept queues disabled by default"
        );
    }

    #[cfg(all(target_os = "linux", feature = "linux-socket"))]
    #[test]
    fn env_config_can_enable_socket_accept_queues() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let trace_dir = dir
            .path()
            .to_str()
            .expect("temporary trace directory path should be valid UTF-8");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir)
            .with("DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED", "true");

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let sources = shared.sources.lock().unwrap();

        assert!(
            sources
                .iter()
                .any(|source| source.name() == "socket_accept_queues"),
            "explicit env opt-in should enable socket accept queues"
        );
    }

    #[test]
    fn env_config_can_disable_tokio_instrumentation_without_disabling_telemetry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir)
            .with("DIAL9_TOKIO_INSTRUMENTATION_ENABLED", "false");

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(rt.guard().is_enabled(), "telemetry should remain enabled");
        assert!(
            !rt.guard()
                .shared()
                .expect("telemetry should be enabled")
                .sources
                .lock()
                .unwrap()
                .iter()
                .flat_map(|s| s.segment_metadata())
                .any(|(k, _)| k.starts_with("runtime.")),
            "no Tokio runtime metadata should be present when Tokio instrumentation is disabled"
        );
        assert!(
            !rt.block_on(async { crate::telemetry::Dial9Handle::current().is_enabled() }),
            "Dial9Handle::current() should remain inert without Tokio hooks"
        );
    }

    #[test]
    fn on_disk_buffer_accepts_required_fields() {
        let _ = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build()
            .expect("build should succeed");
    }

    #[test]
    fn on_disk_buffer_defaults_max_file_size_when_omitted() {
        let _ = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(64 * BYTES_PER_MIB)
            .build()
            .expect("build should succeed without explicit max_file_size");
    }

    #[test]
    fn on_disk_buffer_strict_build_returns_io_error_for_unwritable_base_path() {
        let result = Dial9Config::builder()
            .on_disk_buffer(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build();
        match result {
            Err(Dial9ConfigBuilderError::Io(_)) => {}
            Ok(_) => panic!("expected Io error, got Ok"),
            Err(other) => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn on_disk_buffer_with_runtime_write_back_pipeline_builds_enabled() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(4 * 1024 * 1024)
            .with_runtime(|r| r.with_custom_pipeline(|p| p.write_back()))
            .build()
            .expect("disk build with write_back pipeline should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "config should keep telemetry enabled"
        );
    }

    // ---------------------------------------------------------------
    // in_memory
    // ---------------------------------------------------------------

    #[test]
    fn in_memory_build_yields_enabled_runtime() {
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(16 * BYTES_PER_MIB)
            .build()
            .expect("in-memory build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "in-memory config must keep telemetry enabled"
        );
    }

    #[test]
    fn in_memory_with_runtime_pipeline_runs_and_builds_enabled() {
        use crate::background_task::{ProcessError, SegmentData, SegmentProcessor};
        use std::future::Future;
        use std::pin::Pin;

        #[derive(Debug, Default)]
        struct NoopProcessor;
        impl SegmentProcessor for NoopProcessor {
            fn name(&self) -> &'static str {
                "Noop"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async move { Ok(data) })
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(16 * BYTES_PER_MIB)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r.with_custom_pipeline(|p| p.pipe(NoopProcessor))
            })
            .build()
            .expect("in-memory build with custom pipeline should succeed");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_runtime configurator must run once during build()"
        );
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "config should keep telemetry enabled"
        );
    }

    #[test]
    fn in_memory_build_returns_io_error_for_undersized_budget() {
        // Budget below the 3 × max_segment_size floor is rejected by the writer.
        let result = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(1)
            .build();
        match result {
            Err(Dial9ConfigBuilderError::Io(_)) => {}
            Ok(_) => panic!("expected Io error, got Ok"),
            Err(other) => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn in_memory_build_or_disabled_downgrades_on_undersized_budget() {
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(1)
            .build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("undersized budget should downgrade");
        assert!(
            !rt.guard().is_enabled(),
            "downgrade path must yield an inert guard"
        );
        assert_eq!(rt.block_on(async { 42u32 }), 42);
    }

    // ---------------------------------------------------------------
    // enabled(false) / validation
    // ---------------------------------------------------------------

    #[test]
    fn enabled_false_yields_inert_guard_and_runs_with_tokio() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .enabled(false)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(1);
            })
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "disabled config must yield an inert guard"
        );
        assert_eq!(rt.block_on(async { 7u32 }), 7);
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "with_tokio configurator must run on the disabled runtime build"
        );
    }

    #[test]
    fn enabled_false_skips_required_field_validation() {
        // No max_total_size, but disabled, so it builds without error.
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .enabled(false)
            .build()
            .expect("disabled build needs no writer fields");
        assert!(matches!(cfg.inner, Inner::Disabled { .. }));
    }

    #[test]
    fn enabled_false_skips_with_runtime_configurator() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let _cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .enabled(false)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("disabled build should succeed");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "with_runtime configurator must not run when enabled(false)"
        );
    }

    #[test]
    fn missing_max_total_size_when_enabled_is_validation_error() {
        match Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .build()
        {
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["max_total_size"]);
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn in_memory_missing_max_total_size_when_enabled_is_validation_error() {
        match Dial9Config::builder().in_memory_buffer().build() {
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["max_total_size"]);
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // build_or_disabled (disk)
    // ---------------------------------------------------------------

    #[test]
    fn build_or_disabled_from_complete_builder_yields_enabled_runtime() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("enabled runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "valid config must keep telemetry enabled"
        );
    }

    #[test]
    fn build_or_disabled_downgrades_on_writer_io_failure() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt =
            TracedRuntime::try_new(cfg).expect("writer I/O failure should downgrade to disabled");
        assert!(
            !rt.guard().is_enabled(),
            "downgrade path must yield an inert guard"
        );
        let v = rt.block_on(async { 42u32 });
        assert_eq!(v, 42);
    }

    #[test]
    fn build_or_disabled_preserves_with_tokio_configurators_on_io_failure() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .on_disk_buffer(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(1);
            })
            .build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("downgrade should produce a runtime");
        assert!(!rt.guard().is_enabled());
        let calls = counter.load(Ordering::SeqCst);
        assert!(
            calls >= 1,
            "with_tokio configurator must run on the disabled fallback runtime build (was {calls})"
        );
    }

    // ---------------------------------------------------------------
    // Strict-path configurators
    // ---------------------------------------------------------------

    #[test]
    fn strict_build_runs_with_tokio_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_tokio configurator must run exactly once on strict success path"
        );
    }

    #[test]
    fn strict_build_runs_with_runtime_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("strict build should succeed");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_runtime configurator must run exactly once during build()"
        );
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
    }

    #[test]
    fn multiple_with_tokio_applied_in_declared_order() {
        let order: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let order_first = Arc::clone(&order);
        let order_second = Arc::clone(&order);
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |_b| {
                order_first.lock().unwrap().push(1);
            })
            .with_tokio(move |_b| {
                order_second.lock().unwrap().push(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let recorded = order.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![1, 2],
            "with_tokio configurators must run in declared order"
        );
    }

    #[test]
    fn dial9_config_builder_error_io_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = Dial9ConfigBuilderError::Io(inner);
        let display = format!("{err}");
        assert!(
            display.contains("rotating writer:"),
            "Display should label the variant, got: {display}"
        );
        assert!(
            display.contains("boom"),
            "Display should include the inner io::Error message, got: {display}"
        );
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "source() must return the inner io::Error");
    }

    // ---------------------------------------------------------------
    // graceful shutdown dial (issue #479)
    // ---------------------------------------------------------------

    #[test]
    fn disk_graceful_shutdown_defaults_to_one_second() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(4 * BYTES_PER_MIB)
            .build()
            .expect("build should succeed");
        assert_eq!(
            cfg.graceful_shutdown_timeout,
            Some(Duration::from_secs(1)),
            "disk config must default the graceful-shutdown timeout to 1s"
        );
    }

    #[test]
    fn memory_graceful_shutdown_defaults_to_one_second() {
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(16 * BYTES_PER_MIB)
            .build()
            .expect("build should succeed");
        assert_eq!(
            cfg.graceful_shutdown_timeout,
            Some(Duration::from_secs(1)),
            "in-memory config must default the graceful-shutdown timeout to 1s"
        );
    }

    #[test]
    fn graceful_shutdown_setter_overrides_default() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(4 * BYTES_PER_MIB)
            .graceful_shutdown(Duration::from_secs(7))
            .build()
            .expect("build should succeed");
        assert_eq!(cfg.graceful_shutdown_timeout, Some(Duration::from_secs(7)));
    }

    #[test]
    fn disable_graceful_shutdown_sets_none() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(4 * BYTES_PER_MIB)
            .disable_graceful_shutdown()
            .build()
            .expect("build should succeed");
        assert_eq!(cfg.graceful_shutdown_timeout, None);
    }

    #[test]
    fn memory_disable_graceful_shutdown_sets_none() {
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(16 * BYTES_PER_MIB)
            .disable_graceful_shutdown()
            .build()
            .expect("build should succeed");
        assert_eq!(cfg.graceful_shutdown_timeout, None);
    }

    #[test]
    fn graceful_shutdown_timeout_preserved_when_disabled_downgrades() {
        // build_or_disabled on a writer-I/O failure must keep the configured
        // graceful-shutdown timeout on the downgraded (disabled) config.
        let cfg = Dial9Config::builder()
            .on_disk_buffer(unwritable_base_path())
            .max_total_size(4 * BYTES_PER_MIB)
            .graceful_shutdown(Duration::from_secs(3))
            .build_or_disabled();
        assert!(matches!(cfg.inner, Inner::Disabled { .. }));
        assert_eq!(cfg.graceful_shutdown_timeout, Some(Duration::from_secs(3)));
    }

    #[test]
    fn graceful_shutdown_timeout_flows_to_traced_runtime() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .max_total_size(4 * BYTES_PER_MIB)
            .graceful_shutdown(Duration::from_millis(250))
            .build()
            .expect("build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert_eq!(
            rt.graceful_shutdown_timeout,
            Some(Duration::from_millis(250)),
            "the configured timeout must flow into the TracedRuntime"
        );
    }

    #[test]
    fn graceful_shutdown_drains_worker_after_block_on() {
        use crate::background_task::{ProcessError, SegmentData, SegmentProcessor};
        use std::future::Future;
        use std::pin::Pin;

        #[derive(Debug)]
        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(data) })
            }
        }

        let processed = Arc::new(AtomicUsize::new(0));
        let processed_for_pipeline = Arc::clone(&processed);
        let cfg = Dial9Config::builder()
            .in_memory_buffer()
            .max_total_size(16 * BYTES_PER_MIB)
            .with_runtime(move |r| {
                let processed = Arc::clone(&processed_for_pipeline);
                r.with_custom_pipeline(move |p| p.pipe(CountingProcessor(processed)))
            })
            .build()
            .expect("in-memory build with custom pipeline should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let output = rt.block_on(async { 99u32 });
        assert_eq!(output, 99, "future output must be returned");
        // The generous default 1s timeout is plenty for the no-op processor; the
        // worker is joined, so by the time this returns the segment is drained.
        rt.graceful_shutdown();
        assert!(
            processed.load(Ordering::SeqCst) >= 1,
            "graceful shutdown must drain the background worker (process the sealed segment)"
        );
    }

    #[test]
    fn graceful_shutdown_on_disabled_runtime_is_noop() {
        let cfg = Dial9Config::builder()
            .on_disk_buffer(tmp_base_path())
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert_eq!(rt.block_on(async { 5u32 }), 5);
        rt.graceful_shutdown();
    }
}
