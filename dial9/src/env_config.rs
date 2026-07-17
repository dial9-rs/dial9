//! Environment-driven configuration for the `#[dial9::main]` macro.
//!
//! [`recorder_from_env`] reads the standard `DIAL9_*` variables and assembles a
//! [`TracedRuntimeBuilder`] ready to hand to `#[dial9::main]` (or `.build()` directly).
//! With `DIAL9_ENABLED` off it returns a writer-free disabled recorder that
//! builds a plain Tokio runtime; a writer-setup failure is logged at `error!`
//! and downgraded the same way, so a bad trace config never takes down
//! startup.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dial9_core::buffer::{Disk, DiskBuffer, SegmentWriter};
use dial9_tokio_telemetry::telemetry::{RecorderBuilderTokioExt, TracedRuntimeBuilder};

#[cfg(any(
    feature = "cpu-profiling",
    feature = "process-resource",
    feature = "linux-socket",
    feature = "memory-profiling"
))]
use dial9_tokio_telemetry::telemetry::RecorderPerfExt;

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
const ENV_DIAL9_MEMORY_PROFILE_ENABLED: &str = "DIAL9_MEMORY_PROFILE_ENABLED";
const ENV_DIAL9_MEMORY_SAMPLE_RATE_BYTES: &str = "DIAL9_MEMORY_SAMPLE_RATE_BYTES";
const ENV_DIAL9_MEMORY_TRACK_LIVESET: &str = "DIAL9_MEMORY_TRACK_LIVESET";
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
const DEFAULT_MEMORY_PROFILE_ENABLED: bool = false;
const DEFAULT_TASK_DUMP_ENABLED: bool = false;
const DEFAULT_PROCESS_RESOURCE_USAGE_ENABLED: bool = cfg!(all(unix, feature = "process-resource"));

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
    memory_profile_enabled: Option<bool>,
    memory_sample_rate_bytes: Option<u64>,
    memory_track_liveset: Option<bool>,
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
#[cfg_attr(not(feature = "memory-profiling"), allow(dead_code))]
struct ResolvedMemoryProfilingConfig {
    // None means MemoryProfilingConfig::default() owns the sample rate.
    sample_rate_bytes: Option<u64>,
    // None means MemoryProfilingConfig::default() owns the liveset setting.
    track_liveset: Option<bool>,
}

#[derive(Debug)]
struct ResolvedEnvConfig {
    enabled: bool,
    trace_dir: PathBuf,

    // None means the underlying DiskBuffer builder owns the default.
    rotation_period: Option<Duration>,

    max_total_size: u64,

    // None means the underlying DiskBuffer builder owns the default.
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

    // Optional source: Some(_) installs memory profiling; None leaves it disabled.
    memory_profiling: Option<ResolvedMemoryProfilingConfig>,

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
    #[cfg_attr(not(feature = "process-resource"), allow(dead_code))]
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
        memory_profile_enabled: env.get_bool(ENV_DIAL9_MEMORY_PROFILE_ENABLED),
        memory_sample_rate_bytes: env.get_positive_u64(ENV_DIAL9_MEMORY_SAMPLE_RATE_BYTES),
        memory_track_liveset: env.get_bool(ENV_DIAL9_MEMORY_TRACK_LIVESET),
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
    let memory_profiling = parsed
        .memory_profile_enabled
        .unwrap_or(DEFAULT_MEMORY_PROFILE_ENABLED)
        .then_some(ResolvedMemoryProfilingConfig {
            sample_rate_bytes: parsed.memory_sample_rate_bytes,
            track_liveset: parsed.memory_track_liveset,
        });

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
        memory_profiling,
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

#[cfg(feature = "memory-profiling")]
fn build_memory_profiling_config(
    config: ResolvedMemoryProfilingConfig,
) -> dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig {
    dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig::builder()
        .maybe_sample_rate_bytes(config.sample_rate_bytes)
        .maybe_track_liveset(config.track_liveset)
        .build()
}

#[cfg(feature = "worker-s3")]
fn build_s3_config(
    config: ResolvedS3Config,
) -> dial9_tokio_telemetry::background_task::s3::S3Config {
    dial9_tokio_telemetry::background_task::s3::S3Config::builder()
        .bucket(config.bucket)
        .service_name(config.service_name.unwrap_or_else(default_service_name))
        .prefix(config.prefix)
        .build()
}

/// Build a production-oriented [`TracedRuntimeBuilder`] from standard `DIAL9_*`
/// environment variables.
///
/// Hand it straight to the macro: `#[dial9::main(config = dial9::recorder_from_env)]`.
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
/// directories instead. Handy locally when comparing traces across runs.
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
/// Supported memory profiling variables (`memory-profiling` feature required;
/// applications must still install the `Dial9Allocator` from the `memory` module
/// as their `#[global_allocator]`):
///
/// | Variable | Default | Meaning |
/// | --- | --- | --- |
/// | `DIAL9_MEMORY_PROFILE_ENABLED` | `false` | Enable memory allocation sampling. |
/// | `DIAL9_MEMORY_SAMPLE_RATE_BYTES` | `524288` | Mean bytes between sampled allocations. |
/// | `DIAL9_MEMORY_TRACK_LIVESET` | `false` | Track frees for leak detection. |
///
/// Supported process resource usage variables (`process-resource` feature required):
///
/// | Variable | Default | Meaning |
/// | --- | --- | --- |
/// | `DIAL9_PROCESS_RESOURCE_USAGE_ENABLED` | `true` on Unix with `process-resource`, `false` otherwise | Enable process resource usage sampling from `getrusage(RUSAGE_SELF)`. |
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
/// emit a warning and are treated as missing. With `DIAL9_ENABLED` off, or
/// when the trace writer cannot be created, the returned recorder builds a
/// plain Tokio runtime (the failure is logged at `error!`).
pub fn recorder_from_env() -> TracedRuntimeBuilder<Disk> {
    recorder_from_env_source(&ProcessEnv)
}

fn recorder_from_env_source(env: &impl EnvSource) -> TracedRuntimeBuilder<Disk> {
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
        memory_profiling,
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

    #[cfg(feature = "memory-profiling")]
    let memory_profiling_config = memory_profiling.map(build_memory_profiling_config);
    #[cfg(not(feature = "memory-profiling"))]
    if memory_profiling.is_some() {
        warn(format_args!(
            "dial9: memory profiling requested but `memory-profiling` feature is not enabled; ignoring"
        ));
    }

    // Disabled: return a writer-free recorder so nothing touches the disk.
    if !enabled {
        return TracedRuntimeBuilder::disabled();
    }

    // Build the disk writer; on failure downgrade to a disabled recorder so a
    // bad trace config runs a plain runtime instead of aborting startup.
    let writer = match build_env_disk_writer(
        &trace_dir,
        gc_dead_namespaces,
        max_file_size,
        max_total_size,
        rotation_period,
    ) {
        Ok(writer) => writer,
        Err(e) => {
            error(format_args!(
                "dial9: telemetry writer setup failed; falling back to plain tokio runtime: {e}"
            ));
            return TracedRuntimeBuilder::disabled();
        }
    };

    // Sources that live on the flush-thread recorder are plugged before
    // `with_tokio`; the Tokio-only knobs (and S3 / memory profiling) chain onto
    // the resulting `TracedRuntimeBuilder`.
    let core = apply_core_sources(dial9_core::recorder::recorder(writer), &runtime_config);
    let traced = apply_runtime_env(core.with_tokio(|_| {}), runtime_config, s3);

    #[cfg(feature = "memory-profiling")]
    let traced = match memory_profiling_config {
        Some(config) => traced.with_memory_profiling(config),
        None => traced,
    };

    traced
}

fn build_env_disk_writer(
    trace_dir: &Path,
    gc_dead_namespaces: bool,
    max_file_size: Option<u64>,
    max_total_size: u64,
    rotation_period: Option<Duration>,
) -> std::io::Result<SegmentWriter<Disk>> {
    // The namespace resolves the segment directory to `{trace_dir}/{boot_id}/`,
    // which is what the writer's `base_path` wants.
    let namespace = dial9_core::boot_id::setup_namespace(trace_dir, gc_dead_namespaces)?;
    let mut writer = DiskBuffer::builder()
        .base_path(&namespace.dir)
        .maybe_max_file_size(max_file_size)
        .max_total_size(max_total_size)
        .maybe_rotation_period(rotation_period)
        .build()?;
    writer.set_namespace(namespace.boot_id, namespace.lock);
    Ok(writer)
}

/// Register the flush-thread `Source`s (cpu / sched / rusage / socket) selected
/// by env config onto the core recorder builder, before `with_tokio`.
fn apply_core_sources(
    #[allow(unused_mut)] mut core: dial9_core::recorder::RecorderBuilder<Disk>,
    config: &RuntimeEnvConfig,
) -> dial9_core::recorder::RecorderBuilder<Disk> {
    #[cfg(feature = "cpu-profiling")]
    {
        use dial9_perf_self_profile::{CpuProfilingConfig, SchedEventConfig};

        if config.cpu_profile_enabled {
            let cpu_config = match config.cpu_sample_hz {
                Some(hz) => CpuProfilingConfig::default().frequency_hz(hz),
                None => CpuProfilingConfig::default(),
            };
            core = core.with_cpu_profiling(cpu_config);
        }
        if config.schedule_profile_enabled {
            core = core.with_sched_events(SchedEventConfig::default());
        }
    }
    #[cfg(not(feature = "cpu-profiling"))]
    if config.cpu_profile_enabled || config.schedule_profile_enabled {
        warn(format_args!(
            "dial9: CPU/schedule profiling requested but `cpu-profiling` feature is not enabled; ignoring"
        ));
    }

    #[cfg(feature = "process-resource")]
    if config.process_resource_usage_enabled {
        let process_resource_usage_config = match config.process_resource_usage_sample_interval {
            Some(interval) => dial9_perf_self_profile::ProcessResourceUsageConfig::builder()
                .sample_interval(interval)
                .build(),
            None => dial9_perf_self_profile::ProcessResourceUsageConfig::default(),
        };
        core = core.with_process_resource_usage(process_resource_usage_config);
    }
    #[cfg(not(feature = "process-resource"))]
    if config.process_resource_usage_enabled {
        warn(format_args!(
            "dial9: process resource usage requested but `process-resource` feature is not enabled; ignoring"
        ));
    }

    #[cfg(feature = "linux-socket")]
    if config.socket_accept_queues_enabled == Some(true) {
        let socket_accept_queues_config = match config.socket_accept_queues_sample_interval {
            Some(interval) => dial9_perf_self_profile::SocketAcceptQueuesConfig::builder()
                .sample_interval(interval)
                .build(),
            None => dial9_perf_self_profile::SocketAcceptQueuesConfig::default(),
        };
        core = core.with_socket_accept_queues(socket_accept_queues_config);
    }
    #[cfg(not(feature = "linux-socket"))]
    if config.socket_accept_queues_enabled == Some(true) {
        warn(format_args!(
            "dial9: socket accept queues requested but `linux-socket` feature is not enabled; ignoring"
        ));
    }

    core
}

/// Apply the Tokio-only env knobs (plus S3 upload) onto the `TracedRuntimeBuilder`.
fn apply_runtime_env(
    mut traced: TracedRuntimeBuilder<Disk>,
    config: RuntimeEnvConfig,
    s3: Option<ResolvedS3Config>,
) -> TracedRuntimeBuilder<Disk> {
    if let Some(name) = config.runtime_name {
        traced = traced.with_runtime_name(name);
    }
    if let Some(enabled) = config.tokio_instrumentation_enabled {
        traced = traced.with_tokio_instrumentation(enabled);
    }
    traced = traced.with_task_tracking(config.task_tracking_enabled);

    if config.task_dump_enabled {
        let task_dump_config = match config.task_dump_idle_threshold {
            Some(threshold) => dial9_tokio_telemetry::telemetry::TaskDumpConfig::builder()
                .idle_threshold(threshold)
                .build(),
            None => dial9_tokio_telemetry::telemetry::TaskDumpConfig::default(),
        };
        traced = traced.with_task_dumps(task_dump_config);
    }

    #[cfg(feature = "worker-s3")]
    if let Some(s3) = s3 {
        traced = traced.with_s3_uploader(build_s3_config(s3));
    }
    #[cfg(not(feature = "worker-s3"))]
    if s3.is_some() {
        warn(format_args!(
            "dial9: S3 upload requested but `worker-s3` feature is not enabled; ignoring"
        ));
    }

    traced
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use crate::TracedRuntime;

    use super::*;

    /// A trace dir under a regular file, so creating the namespace directory
    /// fails. Stays unwritable even for root (a nonexistent dir wouldn't, since
    /// root just creates it), which the I/O-downgrade test relies on.
    fn unwritable_trace_dir() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker file");
        // Leak the TempDir so the blocker file outlives the test.
        std::mem::forget(dir);
        blocker.join("traces")
    }

    /// Names of the sources installed on the runtime's recorder, reached through
    /// the public `guard().recorder()` accessor.
    fn source_names(rt: &TracedRuntime) -> Vec<String> {
        rt.recorder()
            .expect("enabled recorder")
            .shared()
            .expect("enabled recorder")
            .with_sources_mut(|sources| sources.iter().map(|s| s.name().to_string()).collect())
            .expect("sources lock")
    }

    /// Segment metadata contributed by the runtime's sources.
    fn segment_metadata(rt: &TracedRuntime) -> Vec<(String, String)> {
        rt.recorder()
            .expect("enabled recorder")
            .shared()
            .expect("enabled recorder")
            .with_sources_mut(|sources| {
                let mut out = Vec::new();
                for source in sources.iter_mut() {
                    source.segment_metadata(&mut out);
                }
                out
            })
            .expect("sources lock")
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

    fn enabled_env(dir: &tempfile::TempDir) -> FakeEnv {
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        FakeEnv::default()
            .with(ENV_DIAL9_ENABLED, "true")
            .with(ENV_DIAL9_TRACE_DIR, trace_dir)
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
        assert_eq!(parsed.memory_profile_enabled, None);
        assert_eq!(parsed.memory_sample_rate_bytes, None);
        assert_eq!(parsed.memory_track_liveset, None);
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
        assert!(resolved.memory_profiling.is_none());
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
                .with("DIAL9_MEMORY_PROFILE_ENABLED", "true")
                .with("DIAL9_MEMORY_SAMPLE_RATE_BYTES", "4096")
                .with("DIAL9_MEMORY_TRACK_LIVESET", "true")
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
        assert_eq!(parsed.memory_profile_enabled, Some(true));
        assert_eq!(parsed.memory_sample_rate_bytes, Some(4096));
        assert_eq!(parsed.memory_track_liveset, Some(true));
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
    fn env_memory_profiling_resolves_only_when_enabled() {
        let resolved = resolve_env_config(parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_MEMORY_SAMPLE_RATE_BYTES", "4096")
                .with("DIAL9_MEMORY_TRACK_LIVESET", "true"),
        ));
        assert!(
            resolved.memory_profiling.is_none(),
            "memory profiling tuning alone should not enable the source"
        );

        let resolved = resolve_env_config(parse_env_config(
            &FakeEnv::default().with("DIAL9_MEMORY_PROFILE_ENABLED", "true"),
        ));
        let memory = resolved
            .memory_profiling
            .expect("memory profiling should be resolved when explicitly enabled");
        assert_eq!(memory.sample_rate_bytes, None);
        assert_eq!(memory.track_liveset, None);

        let resolved = resolve_env_config(parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_MEMORY_PROFILE_ENABLED", "true")
                .with("DIAL9_MEMORY_SAMPLE_RATE_BYTES", "4096")
                .with("DIAL9_MEMORY_TRACK_LIVESET", "true"),
        ));
        let memory = resolved
            .memory_profiling
            .expect("memory profiling should be resolved when explicitly enabled");
        assert_eq!(memory.sample_rate_bytes, Some(4096));
        assert_eq!(memory.track_liveset, Some(true));
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
                .with("DIAL9_MEMORY_PROFILE_ENABLED", "maybe")
                .with("DIAL9_MEMORY_SAMPLE_RATE_BYTES", "0")
                .with("DIAL9_MEMORY_TRACK_LIVESET", "maybe")
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
        assert_eq!(parsed.memory_profile_enabled, None);
        assert_eq!(parsed.memory_sample_rate_bytes, None);
        assert_eq!(parsed.memory_track_liveset, None);
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
    fn env_recorder_builds_disabled_by_default() {
        let rt = recorder_from_env_source(&FakeEnv::default())
            .build()
            .expect("disabled runtime should build");
        assert!(
            !rt.is_enabled(),
            "an env with DIAL9_ENABLED unset must yield a disabled runtime"
        );
        assert_eq!(rt.block_on(async { 5u32 }), 5);
    }

    #[test]
    fn env_recorder_downgrades_to_disabled_on_writer_io_failure() {
        let trace_dir = unwritable_trace_dir();
        let env = FakeEnv::default().with(ENV_DIAL9_ENABLED, "true").with(
            ENV_DIAL9_TRACE_DIR,
            trace_dir.to_str().expect("utf8 trace dir"),
        );

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("writer I/O failure should downgrade to a plain runtime");
        assert!(
            !rt.is_enabled(),
            "writer setup failure must downgrade to a disabled runtime"
        );
        assert_eq!(rt.block_on(async { 42u32 }), 42);
    }

    #[test]
    fn env_recorder_builds_enabled_with_local_trace_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = enabled_env(&dir);

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(
            rt.is_enabled(),
            "DIAL9_ENABLED + DIAL9_TRACE_DIR should keep telemetry enabled"
        );
        // With per-process namespace isolation, trace files land in a
        // boot_id subdirectory: {trace_dir}/{boot_id}/trace.0.bin.active
        let has_namespace_dir = std::fs::read_dir(dir.path())
            .expect("trace dir should exist")
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                dial9_core::boot_id::is_valid_boot_id(&name)
                    && entry.path().is_dir()
                    && std::fs::read_dir(entry.path())
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().starts_with("trace."))
            });
        assert!(
            has_namespace_dir,
            "recorder_from_env should wire DIAL9_TRACE_DIR so trace segments land in <dir>/<boot_id>/"
        );
    }

    #[test]
    fn env_recorder_applies_runtime_name_and_task_dumps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = enabled_env(&dir)
            .with("DIAL9_RUNTIME_NAME", " api-runtime ")
            .with("DIAL9_TASK_DUMP_ENABLED", "true")
            .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "25");

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        let runtime_meta = segment_metadata(&rt);
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
        assert_eq!(
            rt.taskdump_config().map(|c| c.idle_threshold()),
            Some(Duration::from_millis(25)),
            "env config should configure task dumps"
        );
    }

    #[cfg(all(unix, feature = "process-resource"))]
    #[test]
    fn env_recorder_enables_process_resource_usage_by_default_on_unix() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let env = enabled_env(&dir);

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(
            source_names(&rt)
                .iter()
                .any(|name| name == "process_resource_usage"),
            "recorder_from_env should enable process resource usage by default on Unix"
        );
    }

    #[cfg(all(unix, feature = "process-resource"))]
    #[test]
    fn env_recorder_can_disable_process_resource_usage() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let env = enabled_env(&dir).with("DIAL9_PROCESS_RESOURCE_USAGE_ENABLED", "false");

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(
            source_names(&rt)
                .iter()
                .all(|name| name != "process_resource_usage"),
            "explicit env opt-out should disable process resource usage"
        );
    }

    #[cfg(all(target_os = "linux", feature = "linux-socket"))]
    #[test]
    fn env_recorder_does_not_with_socket_accept_queues_by_default() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let env = enabled_env(&dir);

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(
            source_names(&rt)
                .iter()
                .all(|name| name != "socket_accept_queues"),
            "recorder_from_env should leave socket accept queues disabled by default"
        );
    }

    #[cfg(all(target_os = "linux", feature = "linux-socket"))]
    #[test]
    fn env_recorder_can_with_socket_accept_queues() {
        let dir = tempfile::tempdir().expect("temporary trace directory should be created");
        let env = enabled_env(&dir).with("DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED", "true");

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(
            source_names(&rt)
                .iter()
                .any(|name| name == "socket_accept_queues"),
            "explicit env opt-in should enable socket accept queues"
        );
    }

    #[test]
    fn env_recorder_can_disable_tokio_instrumentation_without_disabling_telemetry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = enabled_env(&dir).with("DIAL9_TOKIO_INSTRUMENTATION_ENABLED", "false");

        let rt = recorder_from_env_source(&env)
            .build()
            .expect("runtime should build");
        assert!(rt.is_enabled(), "telemetry should remain enabled");
        let runtime_meta = segment_metadata(&rt);
        assert!(
            !runtime_meta.iter().any(|(k, _)| k.starts_with("runtime.")),
            "no Tokio runtime metadata should be present when Tokio instrumentation is disabled"
        );
        assert!(
            !rt.block_on(async { crate::core::current_handle().is_enabled() }),
            "Dial9Handle::current() should remain inert without Tokio hooks"
        );
    }
}
