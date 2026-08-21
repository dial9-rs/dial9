//! In-process trace source for exercising the complete S3-browser workflow.
//!
//! Simulator objects use the production S3 key layout and the production
//! [`StorageBackend`] interface. The browser, object streaming, on-demand
//! aggregation, span explorer, and flamegraph paths therefore cannot tell the
//! difference between this source and a real trace bucket.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, ensure};
use dial9_trace_format::decoder::{Decoder, RawEvent};
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::{FieldDef, SchemaEntry};
use dial9_trace_format::types::{FieldType, FieldValue, FieldValueRef};
use time::{Date, Month, OffsetDateTime};

use crate::ingest::aggregate::AggContext;
use crate::server::{AggOutput, AppState};
use crate::storage::{BucketInfo, ListPage, ObjectInfo, StorageBackend, StorageError};
use crate::trace_shape::{self, ShapeEvent, ShapeSchema, ShapeValue, TraceShape};

const DEFAULT_BUCKET: &str = "dial9-simulator";
const DEFAULT_PREFIX: &str = "traces";
const DEFAULT_SERVICE: &str = "simulated-service";
const MAX_SIMULATED_HOSTS: usize = 100_000;
const MAX_DEMO_TRACE_BYTES: u64 = 512 * 1024 * 1024;
const PAYLOAD_CACHE_ENTRIES: usize = 8;

/// Trace content used for each simulated segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SimulatorTraceMode {
    /// Generate a sanitized structural copy of the bundled demo trace.
    #[default]
    Synthetic,
    /// Replay the bundled demo's event data, rebased to each segment's time.
    DemoReplay,
}

/// Stack-symbol naming used by synthetic simulator traces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SimulatorSymbolMode {
    /// Keep the privacy-preserving `s_NNNN` placeholders from trace shaping.
    #[default]
    Anonymous,
    /// Replace symbol placeholders with deterministic Rust-like names.
    Realistic,
}

/// Feature groups retained by [`SimulatorTraceMode::Synthetic`].
///
/// Construct with [`SimulatorFeatures::builder`]. Every feature defaults to
/// enabled, so adding a future group will not break existing callers.
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct SimulatorFeatures {
    #[builder(default = true)]
    cpu: bool,
    #[builder(default = true)]
    scheduling: bool,
    #[builder(default = true)]
    tasks: bool,
    #[builder(default = true)]
    spans: bool,
    #[builder(default = true)]
    memory: bool,
    #[builder(default = true)]
    resources: bool,
    #[builder(default = true)]
    custom_events: bool,
}

impl Default for SimulatorFeatures {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// Configuration for [`build_simulator_app`].
///
/// All fields are private and defaulted. Use [`SimulatorConfig::builder`].
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct SimulatorConfig {
    #[builder(default)]
    trace_mode: SimulatorTraceMode,
    /// Number of host rows generated in the browser (default 3).
    #[builder(default = 3)]
    hosts: usize,
    /// Wall-clock coverage and key spacing of each object (default 60 seconds).
    #[builder(default = Duration::from_secs(60))]
    segment_duration: Duration,
    /// Structural template repetitions inside each synthetic object (default 1).
    ///
    /// Demo replay always uses one complete demo trace per object.
    #[builder(default = 1)]
    repetitions_per_segment: u32,
    #[builder(default)]
    features: SimulatorFeatures,
    /// Stack-symbol naming for synthetic traces.
    #[builder(default)]
    symbol_mode: SimulatorSymbolMode,
    #[builder(default = DEFAULT_BUCKET.to_string())]
    bucket: String,
    #[builder(default = DEFAULT_PREFIX.to_string())]
    prefix: String,
    #[builder(default = DEFAULT_SERVICE.to_string())]
    service: String,
    /// Serve the built UI from `ui/dist` instead of embedded assets.
    #[builder(default = false)]
    dev: bool,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// Build a viewer backed by generated S3-shaped trace objects.
///
/// The returned router includes demand-driven aggregation with a process-local
/// temporary output store. Source objects are read-only and generated lazily;
/// no AWS credentials, bucket, or persistent local directory is required.
pub async fn build_simulator_app(config: SimulatorConfig) -> anyhow::Result<axum::Router> {
    let dev = config.dev;
    let backend = tokio::task::spawn_blocking(move || {
        let demo = load_bundled_demo_trace()?;
        SimulatorBackend::new(config, &demo)
    })
    .await
    .context("simulator setup task panicked")??;

    let bucket = backend.bucket.clone();
    let prefix = backend.prefix.clone();
    let segment_duration_secs = backend.segment_duration_secs;
    let host_count = backend.host_count;
    let mode = backend.mode;
    let source: Arc<dyn StorageBackend> = Arc::new(backend);

    let output = AggOutput::temporary();
    let agg = AggContext {
        source: Arc::clone(&source),
        output: output.backend(),
        source_bucket: bucket.clone(),
        source_is_local: false,
        output_bucket: output.output_bucket_for(&bucket),
        output_prefix: output.prefix().to_string(),
        source_prefixes: vec![prefix.clone()],
        segment_duration_secs,
    };

    tracing::info!(
        %bucket,
        %prefix,
        ?mode,
        hosts = host_count,
        segment_duration_secs,
        "virtual simulator source ready"
    );

    let mut state = AppState::new(source, Some(bucket), Some(prefix))
        .with_time_partitioned_source()
        .with_agg(agg)
        .with_agg_output(output)
        .with_agg_segment_secs(segment_duration_secs)
        .with_bucket_filter("");
    if let Some(dir) = crate::resolve_dev_ui_dir(dev)? {
        state = state.with_dev_ui_dir(dir);
    }
    Ok(crate::server::router(state))
}

struct SimulatorBackend {
    bucket: String,
    prefix: String,
    service: String,
    segment_duration_secs: i64,
    host_count: usize,
    mode: SimulatorTraceMode,
    listed_size: i64,
    payloads: Arc<PayloadFactory>,
    cache: Arc<Mutex<PayloadCache>>,
}

#[derive(Clone, PartialEq, Eq)]
struct PayloadCoordinates {
    epoch_secs: i64,
    boot_id: String,
}

impl SimulatorBackend {
    fn new(mut config: SimulatorConfig, demo_trace: &[u8]) -> anyhow::Result<Self> {
        validate_config(&config)?;
        config.prefix = config.prefix.trim_matches('/').to_string();

        let segment_duration_secs = i64::try_from(config.segment_duration.as_secs())
            .context("simulator segment duration does not fit in i64 seconds")?;
        let payloads = Arc::new(PayloadFactory::new(&config, demo_trace)?);
        let segment_duration_ns = u64::try_from(config.segment_duration.as_nanos())
            .context("simulator segment duration does not fit in u64 nanoseconds")?;
        ensure!(
            payloads.duration_ns() <= segment_duration_ns,
            "simulator trace content spans {:.3}s but each segment covers only {}s; \
             increase --simulator-segment-secs or reduce --simulator-repetitions",
            payloads.duration_ns() as f64 / 1_000_000_000.0,
            segment_duration_secs
        );

        Ok(Self {
            bucket: config.bucket,
            prefix: config.prefix,
            service: config.service,
            segment_duration_secs,
            host_count: config.hosts,
            mode: config.trace_mode,
            listed_size: payloads.listed_size(),
            payloads,
            cache: Arc::new(Mutex::new(PayloadCache::default())),
        })
    }

    fn ensure_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        if bucket == self.bucket {
            Ok(())
        } else {
            Err(StorageError::NotFound(format!("bucket {bucket}")))
        }
    }

    fn list_virtual_objects(
        &self,
        prefix: &str,
        cap: Option<usize>,
    ) -> Result<ListPage, StorageError> {
        let (start, end) = match self.catalog_interval(prefix)? {
            CatalogInterval::Empty => {
                return Ok(ListPage {
                    objects: Vec::new(),
                    truncated: false,
                });
            }
            CatalogInterval::Unbounded => {
                return Err(StorageError::Other(
                    "virtual simulator listings require a date or time prefix".to_string(),
                ));
            }
            CatalogInterval::Finite { start, end } => (start.max(0), end),
        };
        if end <= start {
            return Ok(ListPage {
                objects: Vec::new(),
                truncated: false,
            });
        }

        let limit = cap.unwrap_or(usize::MAX);
        let mut objects = Vec::new();
        let first_minute = start - start.rem_euclid(60);
        let mut minute_start = first_minute;
        while minute_start < end {
            let minute_end = minute_start.saturating_add(60).min(end);
            let candidate_start = minute_start.max(start);
            let mut epoch_secs = align_up(candidate_start, self.segment_duration_secs)
                .ok_or_else(|| StorageError::Other("simulator timestamp overflow".to_string()))?;
            let mut minute_objects = Vec::new();
            while epoch_secs < minute_end {
                for host_index in 0..self.host_count {
                    minute_objects.push(self.object_info(host_index, epoch_secs)?);
                }
                epoch_secs = epoch_secs
                    .checked_add(self.segment_duration_secs)
                    .ok_or_else(|| {
                        StorageError::Other("simulator timestamp overflow".to_string())
                    })?;
            }
            minute_objects.sort_unstable_by(|a, b| a.key.cmp(&b.key));
            for object in minute_objects {
                if !object.key.starts_with(prefix) {
                    continue;
                }
                if objects.len() == limit {
                    return Ok(ListPage {
                        objects,
                        truncated: true,
                    });
                }
                objects.push(object);
            }
            minute_start = minute_start.checked_add(60).ok_or_else(|| {
                StorageError::Other("simulator listing range overflow".to_string())
            })?;
        }
        Ok(ListPage {
            objects,
            truncated: false,
        })
    }

    fn object_info(&self, host_index: usize, epoch_secs: i64) -> Result<ObjectInfo, StorageError> {
        let host = simulator_host(host_index);
        let boot_id = simulator_boot_id(host_index);
        let key = simulator_key(
            &self.prefix,
            &self.service,
            &host,
            &boot_id,
            epoch_secs,
            self.segment_duration_secs,
        )
        .map_err(|error| StorageError::Other(format!("format simulator key: {error:#}")))?;
        let modified_epoch = epoch_secs
            .checked_add(self.segment_duration_secs)
            .ok_or_else(|| StorageError::Other("simulator timestamp overflow".to_string()))?;
        let last_modified = format_epoch(modified_epoch).map_err(|error| {
            StorageError::Other(format!("format simulator timestamp: {error:#}"))
        })?;
        Ok(ObjectInfo {
            key,
            size: self.listed_size,
            last_modified: Some(last_modified),
        })
    }

    fn catalog_interval(&self, requested_prefix: &str) -> Result<CatalogInterval, StorageError> {
        let tail = if self.prefix.is_empty() {
            requested_prefix
        } else if requested_prefix == self.prefix
            || requested_prefix == format!("{}/", self.prefix)
            || format!("{}/", self.prefix).starts_with(requested_prefix)
        {
            return Ok(CatalogInterval::Unbounded);
        } else if let Some(tail) = requested_prefix.strip_prefix(&format!("{}/", self.prefix)) {
            tail
        } else {
            return Ok(CatalogInterval::Empty);
        };
        if tail.is_empty() {
            return Ok(CatalogInterval::Unbounded);
        }

        let mut parts = tail.split('/');
        if parts.next() != Some("version=1") {
            return Ok(CatalogInterval::Empty);
        }
        let Some(date) = parts
            .next()
            .and_then(|part| part.strip_prefix("date="))
            .and_then(parse_catalog_date)
        else {
            return Ok(CatalogInterval::Empty);
        };
        let day_start = date.midnight().assume_utc().unix_timestamp();
        let Some(_service) = parts.next().and_then(|part| part.strip_prefix("service=")) else {
            return Ok(CatalogInterval::Finite {
                start: day_start,
                end: day_start + 86_400,
            });
        };
        let Some(time_prefix) = parts.next().and_then(|part| part.strip_prefix("time=")) else {
            return Ok(CatalogInterval::Finite {
                start: day_start,
                end: day_start + 86_400,
            });
        };
        if time_prefix.is_empty() {
            return Ok(CatalogInterval::Finite {
                start: day_start,
                end: day_start + 86_400,
            });
        }
        let Some((offset, width)) = parse_time_prefix(time_prefix) else {
            return Ok(CatalogInterval::Empty);
        };
        Ok(CatalogInterval::Finite {
            start: day_start + offset,
            end: day_start + offset + width,
        })
    }

    fn parse_key(&self, key: &str) -> Result<PayloadCoordinates, StorageError> {
        let parsed = crate::segment_object_key_parser::parse_segment_object_key(key);
        if parsed.layout != crate::segment_object_key_parser::SegmentObjectKeyLayout::Version1
            || parsed.service.as_deref() != Some(self.service.as_str())
        {
            return Err(StorageError::NotFound(key.to_string()));
        }
        let host = parsed
            .instance
            .as_deref()
            .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        let host_index = parse_simulator_host(host, self.host_count)
            .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        let boot_id = parsed
            .boot_id
            .as_deref()
            .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        if boot_id != simulator_boot_id(host_index) {
            return Err(StorageError::NotFound(key.to_string()));
        }
        let epoch_secs = parsed
            .epoch_secs
            .filter(|epoch| *epoch >= 0 && epoch.rem_euclid(self.segment_duration_secs) == 0)
            .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        let expected_sequence = virtual_segment_sequence(epoch_secs, self.segment_duration_secs)
            .ok_or_else(|| StorageError::NotFound(key.to_string()))?;
        if parsed.segment_index.map(u64::from) != Some(expected_sequence) {
            return Err(StorageError::NotFound(key.to_string()));
        }
        let canonical = simulator_key(
            &self.prefix,
            &self.service,
            host,
            boot_id,
            epoch_secs,
            self.segment_duration_secs,
        )
        .map_err(|_| StorageError::NotFound(key.to_string()))?;
        if canonical != key {
            return Err(StorageError::NotFound(key.to_string()));
        }
        Ok(PayloadCoordinates {
            epoch_secs,
            boot_id: boot_id.to_string(),
        })
    }
}

enum CatalogInterval {
    Empty,
    Unbounded,
    Finite { start: i64, end: i64 },
}

impl StorageBackend for SimulatorBackend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<BucketInfo>, StorageError>> + Send + '_>> {
        let bucket = self.bucket.clone();
        Box::pin(async move { Ok(vec![BucketInfo::new(bucket, None)]) })
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> Pin<Box<dyn Future<Output = Result<ListPage, StorageError>> + Send + '_>> {
        let result = self
            .ensure_bucket(bucket)
            .and_then(|()| self.list_virtual_objects(prefix, Some(cap)));
        Box::pin(async move { result })
    }

    fn list_objects_all(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let result = self
            .ensure_bucket(bucket)
            .and_then(|()| self.list_virtual_objects(prefix, None))
            .map(|page| page.objects);
        Box::pin(async move { result })
    }

    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let result = self.ensure_bucket(bucket).and_then(|()| {
            let catalog_root = if self.prefix.is_empty() {
                "version=1/".to_string()
            } else {
                format!("{}/version=1/", self.prefix)
            };
            if !catalog_root.is_empty()
                && catalog_root.starts_with(prefix)
                && prefix != catalog_root
            {
                let rest = &catalog_root[prefix.len()..];
                let slash = rest.find('/').unwrap_or(rest.len().saturating_sub(1));
                return Ok(vec![format!("{prefix}{}", &rest[..=slash])]);
            }
            let page = self.list_virtual_objects(prefix, None)?;
            let mut prefixes = BTreeSet::new();
            for object in page.objects {
                let Some(rest) = object.key.strip_prefix(prefix) else {
                    continue;
                };
                let Some(slash) = rest.find('/') else {
                    continue;
                };
                prefixes.insert(format!("{prefix}{}", &rest[..=slash]));
            }
            Ok(prefixes.into_iter().collect())
        });
        Box::pin(async move { result })
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let result = self
            .ensure_bucket(bucket)
            .and_then(|()| self.parse_key(key));
        let payloads = Arc::clone(&self.payloads);
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let coordinates = result?;
            if let Some(bytes) = cache
                .lock()
                .map_err(|_| StorageError::Other("simulator payload cache poisoned".into()))?
                .get(&coordinates)
            {
                return Ok(bytes.as_ref().clone());
            }

            let render_coordinates = coordinates.clone();
            let rendered = tokio::task::spawn_blocking(move || {
                payloads.render(render_coordinates.epoch_secs, &render_coordinates.boot_id)
            })
            .await
            .map_err(|e| StorageError::Other(format!("simulator render task panicked: {e}")))?
            .map_err(|e| StorageError::Other(format!("generate simulator trace: {e:#}")))?;
            let rendered = Arc::new(rendered);
            cache
                .lock()
                .map_err(|_| StorageError::Other("simulator payload cache poisoned".into()))?
                .insert(coordinates, Arc::clone(&rendered));
            Ok(rendered.as_ref().clone())
        })
    }

    fn put_object(
        &self,
        _bucket: &str,
        _key: &str,
        _data: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + '_>> {
        Box::pin(async {
            Err(StorageError::Other(
                "the simulator trace source is read-only".to_string(),
            ))
        })
    }
}

#[derive(Default)]
struct PayloadCache {
    entries: VecDeque<(PayloadCoordinates, Arc<Vec<u8>>)>,
}

impl PayloadCache {
    fn get(&mut self, coordinates: &PayloadCoordinates) -> Option<Arc<Vec<u8>>> {
        let index = self
            .entries
            .iter()
            .position(|(key, _)| key == coordinates)?;
        let entry = self.entries.remove(index)?;
        let result = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, coordinates: PayloadCoordinates, bytes: Arc<Vec<u8>>) {
        if let Some(index) = self.entries.iter().position(|(key, _)| key == &coordinates) {
            self.entries.remove(index);
        }
        self.entries.push_back((coordinates, bytes));
        while self.entries.len() > PAYLOAD_CACHE_ENTRIES {
            self.entries.pop_front();
        }
    }
}

enum PayloadTemplate {
    Replay {
        raw: Arc<Vec<u8>>,
        first_timestamp_ns: u64,
        first_wall_ns: u64,
        duration_ns: u64,
    },
    Synthetic {
        shape: Arc<TraceShape>,
        repetitions: u32,
        duration_ns: u64,
    },
}

struct PayloadFactory {
    template: PayloadTemplate,
    listed_size: i64,
}

impl PayloadFactory {
    fn new(config: &SimulatorConfig, demo_trace: &[u8]) -> anyhow::Result<Self> {
        let repetitions = match config.trace_mode {
            SimulatorTraceMode::Synthetic => u64::from(config.repetitions_per_segment),
            SimulatorTraceMode::DemoReplay => 1,
        };
        let listed_size = u64::try_from(demo_trace.len())
            .context("bundled demo trace size does not fit in u64")?
            .checked_mul(repetitions)
            .context("simulator listed size overflow")
            .and_then(|size| i64::try_from(size).context("simulator listed size exceeds i64"))?;
        let raw = decode_trace_file(demo_trace)?;
        let (first_timestamp_ns, last_timestamp_ns) = timestamp_bounds(&raw)?;
        let template = match config.trace_mode {
            SimulatorTraceMode::DemoReplay => {
                ensure!(
                    config.repetitions_per_segment == 1,
                    "--simulator-repetitions applies only to synthetic mode; \
                     use more segments to replay the demo trace repeatedly"
                );
                ensure!(
                    first_timestamp_ns > 0,
                    "the bundled demo trace has an invalid timestamp range"
                );
                let (clock_timestamp_ns, clock_realtime_ns) = first_clock_sync(&raw)?;
                let first_wall_ns =
                    rebase_value(clock_realtime_ns, clock_timestamp_ns, first_timestamp_ns)
                        .context("the bundled demo trace clock cannot cover its first event")?;
                PayloadTemplate::Replay {
                    raw: Arc::new(raw),
                    first_timestamp_ns,
                    first_wall_ns,
                    duration_ns: last_timestamp_ns.saturating_sub(first_timestamp_ns),
                }
            }
            SimulatorTraceMode::Synthetic => {
                let mut shape =
                    trace_shape::extract_shape(&raw).context("extract bundled demo trace shape")?;
                let dropped = trace_shape::rebase_timeline_from_first_nonzero_event(&mut shape)?;
                if dropped > 0 {
                    tracing::warn!(
                        events_dropped = dropped,
                        "simulator synthetic template omitted zero-timestamp demo events"
                    );
                }
                apply_symbol_mode(&mut shape, config.symbol_mode)?;
                trace_shape::retain_events(&mut shape, |schema, event| {
                    feature_enabled(config.features, schema, event)
                })?;
                let template_duration = shape.summary.duration_ns.max(10_000);
                let repetitions = u64::from(config.repetitions_per_segment);
                let duration_ns = template_duration
                    .checked_mul(repetitions)
                    .and_then(|duration| {
                        duration.checked_add(10_000u64.saturating_mul(repetitions - 1))
                    })
                    .context("synthetic simulator duration overflow")?;
                PayloadTemplate::Synthetic {
                    shape: Arc::new(shape),
                    repetitions: config.repetitions_per_segment,
                    duration_ns,
                }
            }
        };
        Ok(Self {
            template,
            listed_size,
        })
    }

    fn duration_ns(&self) -> u64 {
        match &self.template {
            PayloadTemplate::Replay { duration_ns, .. }
            | PayloadTemplate::Synthetic { duration_ns, .. } => *duration_ns,
        }
    }

    fn listed_size(&self) -> i64 {
        self.listed_size
    }

    fn render(&self, epoch_secs: i64, boot_id: &str) -> anyhow::Result<Vec<u8>> {
        let wall_start_ns = u64::try_from(epoch_secs)
            .context("simulator timestamp predates the Unix epoch")?
            .checked_mul(1_000_000_000)
            .context("simulator wall-clock timestamp overflow")?;
        let raw = match &self.template {
            PayloadTemplate::Replay {
                raw,
                first_timestamp_ns,
                first_wall_ns,
                ..
            } => rebase_replay(
                raw,
                *first_timestamp_ns,
                *first_wall_ns,
                wall_start_ns,
                boot_id,
            )?,
            PayloadTemplate::Synthetic {
                shape, repetitions, ..
            } => {
                let generated = trace_shape::generate_trace_with_bases(
                    shape,
                    *repetitions,
                    wall_start_ns,
                    wall_start_ns,
                )?;
                rewrite_segment_boot_id(&generated, boot_id)?
            }
        };
        gzip(&raw)
    }
}

fn validate_config(config: &SimulatorConfig) -> anyhow::Result<()> {
    ensure!(config.hosts > 0, "simulator hosts must be at least 1");
    ensure!(
        config.hosts <= MAX_SIMULATED_HOSTS,
        "simulator host count {} exceeds the {MAX_SIMULATED_HOSTS} limit",
        config.hosts
    );
    ensure!(
        config.repetitions_per_segment > 0,
        "simulator repetitions must be at least 1"
    );
    ensure!(
        !config.segment_duration.is_zero() && config.segment_duration.subsec_nanos() == 0,
        "simulator segment duration must be a positive whole number of seconds"
    );
    validate_path_value("bucket", &config.bucket, false, false)?;
    validate_path_value("prefix", &config.prefix, true, true)?;
    validate_path_value("service", &config.service, false, false)?;
    Ok(())
}

fn validate_path_value(
    name: &str,
    value: &str,
    allow_slash: bool,
    allow_empty: bool,
) -> anyhow::Result<()> {
    ensure!(
        allow_empty || !value.trim().is_empty(),
        "simulator {name} must not be empty"
    );
    ensure!(
        !value.chars().any(char::is_control),
        "simulator {name} must not contain control characters"
    );
    ensure!(
        allow_slash || !value.contains('/'),
        "simulator {name} must be one path segment"
    );
    ensure!(
        !value.split('/').any(|part| part == ".."),
        "simulator {name} must not contain '..' path segments"
    );
    Ok(())
}

fn feature_enabled(features: SimulatorFeatures, schema: &ShapeSchema, event: &ShapeEvent) -> bool {
    match schema.name.as_str() {
        "ClockSyncEvent" | "SegmentMetadataEvent" => true,
        "CpuSampleEvent" => {
            let source = field_u64(schema, event, "source");
            if source == Some(1) {
                features.scheduling
            } else {
                features.cpu
            }
        }
        "WorkerParkEvent" | "WorkerUnparkEvent" => features.scheduling,
        // `RuntimeMetricsEvent` supersedes `QueueSampleEvent`; both are gated by
        // `tasks` so shaping either producer generation drops the queue/alive-task
        // series together instead of letting the newer one through as a custom event.
        "PollStartEvent"
        | "PollEndEvent"
        | "QueueSampleEvent"
        | "RuntimeMetricsEvent"
        | "TaskSpawnEvent"
        | "TaskTerminateEvent"
        | "WakeEventEvent"
        | "TaskDumpEvent" => features.tasks,
        "SpanCloseEvent" => features.spans,
        "AllocEvent" | "FreeEvent" | "MemoryProfileOverflowEvent" => features.memory,
        "ProcessResourceUsageEvent" | "TcpAcceptQueueEvent" => features.resources,
        "SymbolTableEntry" => {
            features.cpu
                || features.scheduling
                || features.tasks
                || features.spans
                || features.memory
        }
        name if name.starts_with("SpanEnter:") || name.starts_with("SpanExit:") => features.spans,
        _ => features.custom_events,
    }
}

fn field_u64(schema: &ShapeSchema, event: &ShapeEvent, name: &str) -> Option<u64> {
    let index = schema.fields.iter().position(|field| field.name == name)?;
    match event.values.get(index)? {
        ShapeValue::U(value) => Some(*value),
        _ => None,
    }
}

fn apply_symbol_mode(
    shape: &mut TraceShape,
    symbol_mode: SimulatorSymbolMode,
) -> anyhow::Result<()> {
    if symbol_mode == SimulatorSymbolMode::Anonymous {
        return Ok(());
    }

    let Some(schema_index) = shape
        .schemas
        .iter()
        .position(|schema| schema.name == "SymbolTableEntry")
    else {
        return Ok(());
    };
    let schema = &shape.schemas[schema_index];
    let symbol_name_index = schema
        .fields
        .iter()
        .position(|field| field.name == "symbol_name")
        .context("SymbolTableEntry has no symbol_name field")?;
    let source_file_index = schema
        .fields
        .iter()
        .position(|field| field.name == "source_file");

    let mut ordinal = 0usize;
    for event in &mut shape.events {
        if event.schema_index as usize != schema_index {
            continue;
        }
        let (symbol_name, source_file) = realistic_symbol(ordinal);
        replace_shape_string(
            event
                .values
                .get_mut(symbol_name_index)
                .context("SymbolTableEntry is missing symbol_name")?,
            symbol_name,
        )?;
        if let Some(index) = source_file_index {
            replace_shape_string(
                event
                    .values
                    .get_mut(index)
                    .context("SymbolTableEntry is missing source_file")?,
                source_file,
            )?;
        }
        ordinal += 1;
    }
    Ok(())
}

fn replace_shape_string(value: &mut ShapeValue, replacement: String) -> anyhow::Result<()> {
    match value {
        ShapeValue::S(value) | ShapeValue::PS(value) => {
            *value = replacement;
            Ok(())
        }
        ShapeValue::None => Ok(()),
        _ => anyhow::bail!("synthetic symbol field is not a string"),
    }
}

fn realistic_symbol(mut ordinal: usize) -> (String, String) {
    const CRATES: &[&str] = &[
        "telemetry",
        "runtime",
        "storage",
        "network",
        "protocol",
        "service",
        "metrics",
        "scheduler",
        "tracing",
        "database",
        "cache",
        "gateway",
    ];
    const MODULES: &[&str] = &[
        "collector",
        "worker",
        "buffer",
        "request",
        "response",
        "segment",
        "connection",
        "stream",
        "task",
        "queue",
        "profile",
        "event",
        "batch",
        "client",
        "server",
        "codec",
        "index",
        "state",
    ];
    const VERBS: &[&str] = &[
        "poll", "read", "write", "flush", "dispatch", "decode", "encode", "collect", "record",
        "resolve", "schedule", "process", "handle", "update", "load", "store", "acquire",
        "release", "prepare", "commit",
    ];
    const NOUNS: &[&str] = &[
        "trace", "sample", "frame", "span", "metric", "packet", "message", "record", "cursor",
        "future", "resource", "snapshot",
    ];

    let crate_name = CRATES[ordinal % CRATES.len()];
    ordinal /= CRATES.len();
    let module = MODULES[ordinal % MODULES.len()];
    ordinal /= MODULES.len();
    let verb = VERBS[ordinal % VERBS.len()];
    ordinal /= VERBS.len();
    let noun = NOUNS[ordinal % NOUNS.len()];
    (
        format!("{crate_name}::{module}::{verb}_{noun}"),
        format!("src/{module}/{noun}.rs"),
    )
}

fn rebase_replay(
    raw: &[u8],
    first_timestamp_ns: u64,
    first_wall_ns: u64,
    wall_start_ns: u64,
    boot_id: &str,
) -> anyhow::Result<Vec<u8>> {
    reencode_trace(
        raw,
        boot_id,
        Some(TimelineRebase {
            first_timestamp_ns,
            first_wall_ns,
            wall_start_ns,
        }),
    )
}

fn rewrite_segment_boot_id(raw: &[u8], boot_id: &str) -> anyhow::Result<Vec<u8>> {
    reencode_trace(raw, boot_id, None)
}

#[derive(Clone, Copy)]
struct TimelineRebase {
    first_timestamp_ns: u64,
    first_wall_ns: u64,
    wall_start_ns: u64,
}

fn reencode_trace(
    raw: &[u8],
    boot_id: &str,
    timeline: Option<TimelineRebase>,
) -> anyhow::Result<Vec<u8>> {
    let mut decoder = Decoder::new(raw).context("invalid bundled demo trace")?;
    let mut encoder = Encoder::new();
    let mut schemas = HashMap::<String, (SchemaEntry, Schema)>::new();

    decoder
        .try_for_each_event(|event| {
            let timestamp_ns = event.timestamp_ns;
            let timestamp_ns = if let Some(timeline) = timeline {
                rebase_value(
                    timestamp_ns,
                    timeline.first_timestamp_ns,
                    timeline.wall_start_ns,
                )
                .context("demo replay timestamp shift overflow")?
            } else {
                timestamp_ns
            };

            let entry = normalized_replay_schema(event.schema);
            let schema = if let Some((existing, schema)) = schemas.get(event.name) {
                ensure!(
                    existing == &entry,
                    "demo replay schema '{}' changed definition after a stream reset",
                    event.name
                );
                schema.clone()
            } else {
                let schema = Schema::from_entry(entry.clone());
                schemas.insert(event.name.to_string(), (entry, schema.clone()));
                schema
            };

            let mut values = Vec::with_capacity(event.fields.len());
            for (field, value) in event.schema.fields().iter().zip(event.fields.iter()) {
                let mut value = if event.name == "SegmentMetadataEvent" && field.name() == "entries"
                {
                    rewrite_boot_id_value(value, boot_id)?
                } else {
                    replay_value(value, &event, &mut encoder)?
                };
                if let Some(timeline) = timeline
                    && event.name == "ClockSyncEvent"
                    && field.name() == "realtime_ns"
                {
                    let FieldValue::Varint(realtime_ns) = value else {
                        anyhow::bail!("demo ClockSyncEvent.realtime_ns is not a varint");
                    };
                    value = FieldValue::Varint(
                        rebase_value(realtime_ns, timeline.first_wall_ns, timeline.wall_start_ns)
                            .context("demo replay realtime shift overflow")?,
                    );
                } else if let Some(timeline) = timeline
                    && field.name() == "alloc_timestamp_ns"
                {
                    value = match value {
                        FieldValue::Varint(timestamp_ns) => FieldValue::Varint(
                            rebase_value(
                                timestamp_ns,
                                timeline.first_timestamp_ns,
                                timeline.wall_start_ns,
                            )
                            .context("demo replay timestamp reference shift overflow")?,
                        ),
                        FieldValue::None => FieldValue::None,
                        _ => anyhow::bail!(
                            "demo replay alloc_timestamp_ns is neither a varint nor absent"
                        ),
                    };
                }
                values.push(value);
            }
            encoder
                .write_event(&schema, timestamp_ns, &values)
                .with_context(|| format!("re-encode demo event '{}'", event.name))
        })
        .map_err(|error| match error {
            dial9_trace_format::decoder::TryForEachError::Decode(error) => {
                anyhow::anyhow!(
                    "decode bundled demo trace at {}: {}",
                    error.pos,
                    error.message
                )
            }
            dial9_trace_format::decoder::TryForEachError::User(error) => error,
        })?;
    ensure!(
        decoder.position() == decoder.data_len(),
        "bundled demo trace has trailing undecodable bytes"
    );
    Ok(encoder.finish())
}

fn rewrite_boot_id_value(value: &FieldValueRef<'_>, boot_id: &str) -> anyhow::Result<FieldValue> {
    let FieldValueRef::StringMap(entries) = value else {
        anyhow::bail!("SegmentMetadataEvent.entries is not a string map");
    };
    let mut found = false;
    let mut rewritten = entries
        .iter()
        .map(|(key, value)| {
            let value = if key == "boot_id" {
                found = true;
                boot_id
            } else {
                value
            };
            (key.as_bytes().to_vec(), value.as_bytes().to_vec())
        })
        .collect::<Vec<_>>();
    if !found {
        rewritten.push((b"boot_id".to_vec(), boot_id.as_bytes().to_vec()));
    }
    Ok(FieldValue::StringMap(rewritten))
}

fn normalized_replay_schema(schema: &SchemaEntry) -> SchemaEntry {
    SchemaEntry::with_annotations(
        schema.name(),
        schema.fields().iter().map(|field| {
            FieldDef::new(
                field.name(),
                normalize_replay_field_type(field.field_type()),
            )
        }),
        schema.annotations().iter().cloned(),
    )
}

fn normalize_replay_field_type(field_type: FieldType) -> FieldType {
    match field_type {
        FieldType::U8 | FieldType::U16 | FieldType::U32 => FieldType::Varint,
        FieldType::OptionalU8 | FieldType::OptionalU16 | FieldType::OptionalU32 => {
            FieldType::OptionalVarint
        }
        other => other,
    }
}

fn replay_value(
    value: &FieldValueRef<'_>,
    event: &RawEvent<'_, '_>,
    encoder: &mut Encoder,
) -> anyhow::Result<FieldValue> {
    Ok(match value {
        FieldValueRef::PooledString(id) => {
            let string = event
                .string_pool
                .get(*id)
                .with_context(|| format!("demo replay references missing string pool id {id:?}"))?;
            FieldValue::PooledString(encoder.intern_string(string)?)
        }
        FieldValueRef::PooledStackFrames(id) => {
            let frames = event
                .stack_pool
                .get(*id)
                .with_context(|| format!("demo replay references missing stack pool id {id:?}"))?;
            FieldValue::PooledStackFrames(encoder.intern_stack_frames(frames)?)
        }
        FieldValueRef::List(list) => FieldValue::List(
            list.iter()
                .map(|value| replay_value(value, event, encoder))
                .collect::<anyhow::Result<_>>()?,
        ),
        FieldValueRef::Map(map) => FieldValue::Map(
            map.iter()
                .map(|(key, value)| {
                    Ok((
                        replay_value(key, event, encoder)?,
                        replay_value(value, event, encoder)?,
                    ))
                })
                .collect::<anyhow::Result<_>>()?,
        ),
        _ => value.to_owned(),
    })
}

fn rebase_value(value: u64, source_base: u64, target_base: u64) -> Option<u64> {
    if value >= source_base {
        target_base.checked_add(value - source_base)
    } else {
        target_base.checked_sub(source_base - value)
    }
}

fn first_clock_sync(raw: &[u8]) -> anyhow::Result<(u64, u64)> {
    let mut decoder = Decoder::new(raw).context("invalid bundled demo trace")?;
    let mut clock = None;
    decoder
        .try_for_each_event(|event| {
            if clock.is_some() || event.name != "ClockSyncEvent" {
                return Ok::<_, anyhow::Error>(());
            }
            let timestamp_ns = event.timestamp_ns;
            let realtime_ns = event
                .field_names()
                .zip(event.fields.iter())
                .find(|(name, _)| *name == "realtime_ns")
                .and_then(|(_, value)| match value {
                    FieldValueRef::Varint(value) => Some(*value),
                    _ => None,
                })
                .context("demo ClockSyncEvent has no varint realtime_ns")?;
            if timestamp_ns > 0 && realtime_ns > 0 {
                clock = Some((timestamp_ns, realtime_ns));
            }
            Ok(())
        })
        .map_err(|error| match error {
            dial9_trace_format::decoder::TryForEachError::Decode(error) => {
                anyhow::anyhow!(
                    "decode bundled demo trace at {}: {}",
                    error.pos,
                    error.message
                )
            }
            dial9_trace_format::decoder::TryForEachError::User(error) => error,
        })?;
    clock.context("bundled demo trace has no valid ClockSyncEvent")
}

fn timestamp_bounds(raw: &[u8]) -> anyhow::Result<(u64, u64)> {
    let mut decoder = Decoder::new(raw).context("invalid bundled demo trace")?;
    let mut min = None;
    let mut max = None;
    decoder
        .try_for_each_event(|event| {
            let timestamp = event.timestamp_ns;
            if timestamp == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            min = Some(min.map_or(timestamp, |value: u64| value.min(timestamp)));
            max = Some(max.map_or(timestamp, |value: u64| value.max(timestamp)));
            Ok::<_, anyhow::Error>(())
        })
        .map_err(|error| match error {
            dial9_trace_format::decoder::TryForEachError::Decode(error) => {
                anyhow::anyhow!(
                    "decode bundled demo trace at {}: {}",
                    error.pos,
                    error.message
                )
            }
            dial9_trace_format::decoder::TryForEachError::User(error) => error,
        })?;
    ensure!(
        decoder.position() == decoder.data_len(),
        "bundled demo trace has trailing undecodable bytes"
    );
    Ok((
        min.context("bundled demo trace has no events")?,
        max.context("bundled demo trace has no events")?,
    ))
}

fn simulator_key(
    prefix: &str,
    service: &str,
    host: &str,
    boot_id: &str,
    epoch_secs: i64,
    segment_duration_secs: i64,
) -> anyhow::Result<String> {
    let timestamp = OffsetDateTime::from_unix_timestamp(epoch_secs)
        .context("simulator timestamp out of range")?;
    let date = format!(
        "{:04}-{:02}-{:02}",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day()
    );
    let minute = format!("{:02}{:02}", timestamp.hour(), timestamp.minute());
    let sequence = virtual_segment_sequence(epoch_secs, segment_duration_secs)
        .context("simulator segment sequence out of range")?;
    let filename = format!("{epoch_secs}-{sequence}.bin.gz");
    Ok(
        crate::segment_object_key_codec::format_v1_segment_object_key(
            (!prefix.is_empty()).then_some(prefix),
            &date,
            service,
            &minute,
            host,
            boot_id,
            &filename,
        ),
    )
}

fn simulator_host(host_index: usize) -> String {
    format!("host-{:03}", host_index + 1)
}

fn parse_simulator_host(host: &str, host_count: usize) -> Option<usize> {
    let number = host.strip_prefix("host-")?.parse::<usize>().ok()?;
    let host_index = number.checked_sub(1)?;
    (host_index < host_count && simulator_host(host_index) == host).then_some(host_index)
}

fn simulator_boot_id(host_index: usize) -> String {
    format!("simu-{:06}", host_index + 1)
}

fn virtual_segment_sequence(epoch_secs: i64, segment_duration_secs: i64) -> Option<u64> {
    u64::try_from(epoch_secs.checked_div(segment_duration_secs)?).ok()
}

fn align_up(value: i64, alignment: i64) -> Option<i64> {
    let remainder = value.rem_euclid(alignment);
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

fn parse_catalog_date(value: &str) -> Option<Date> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u8>().ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

fn parse_time_prefix(value: &str) -> Option<(i64, i64)> {
    if !(1..=4).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (hour, minute, width) = match value.len() {
        1 => {
            let hour_tens = value.parse::<u8>().ok()?;
            if hour_tens > 2 {
                return None;
            }
            let start_hour = hour_tens * 10;
            let hours = 10u8.min(24 - start_hour);
            (start_hour, 0, i64::from(hours) * 3_600)
        }
        2 => (value.parse::<u8>().ok()?, 0, 3_600),
        3 => {
            let hour = value[..2].parse::<u8>().ok()?;
            let minute_tens = value[2..].parse::<u8>().ok()?;
            if minute_tens > 5 {
                return None;
            }
            (hour, minute_tens * 10, 600)
        }
        4 => (
            value[..2].parse::<u8>().ok()?,
            value[2..].parse::<u8>().ok()?,
            60,
        ),
        _ => unreachable!("length checked above"),
    };
    if hour >= 24 || minute >= 60 {
        return None;
    }
    Some((i64::from(hour) * 3_600 + i64::from(minute) * 60, width))
}

fn format_epoch(epoch_secs: i64) -> anyhow::Result<String> {
    let timestamp = OffsetDateTime::from_unix_timestamp(epoch_secs)
        .context("simulator timestamp out of range")?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    ))
}

fn gzip(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(raw).context("compress simulator trace")?;
    encoder.finish().context("finish simulator trace gzip")
}

fn decode_trace_file(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        ensure!(
            bytes.len() as u64 <= MAX_DEMO_TRACE_BYTES,
            "bundled demo trace exceeds the decompressed size limit"
        );
        return Ok(bytes.to_vec());
    }

    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut bounded = decoder.take(MAX_DEMO_TRACE_BYTES + 1);
    let mut raw = Vec::new();
    bounded
        .read_to_end(&mut raw)
        .context("decompress bundled demo trace")?;
    ensure!(
        raw.len() as u64 <= MAX_DEMO_TRACE_BYTES,
        "bundled demo trace exceeds the {MAX_DEMO_TRACE_BYTES} byte decompressed limit"
    );
    Ok(raw)
}

fn load_bundled_demo_trace() -> anyhow::Result<Vec<u8>> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        manifest_dir.join("ui/public/demo-trace.bin"),
        manifest_dir.join("ui/dist/demo-trace.bin"),
    ] {
        match std::fs::read(&path) {
            Ok(bytes) => return Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        }
    }
    crate::server::embedded_ui_asset("demo-trace.bin").context(
        "the bundled demo trace is unavailable; build the viewer UI before using simulator mode",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct ClockSync {
        timestamp_ns: u64,
        realtime_ns: u64,
    }

    #[derive(Deserialize)]
    struct SegmentMetadata {
        entries: HashMap<String, String>,
    }

    fn only_cpu() -> SimulatorFeatures {
        SimulatorFeatures::builder()
            .cpu(true)
            .scheduling(false)
            .tasks(false)
            .spans(false)
            .memory(false)
            .resources(false)
            .custom_events(false)
            .build()
    }

    fn trace_boot_id(raw: &[u8]) -> String {
        let mut decoder = Decoder::new(raw).unwrap();
        let mut boot_id = None;
        decoder
            .for_each_event(|event| {
                if event.name == "SegmentMetadataEvent"
                    && let Ok(metadata) = event.deserialize::<SegmentMetadata>()
                    && let Some(value) = metadata.entries.get("boot_id")
                {
                    boot_id = Some(value.clone());
                }
            })
            .unwrap();
        boot_id.expect("simulator trace should contain boot_id metadata")
    }

    #[tokio::test]
    async fn replay_backend_lists_virtual_fleet_without_rendering_and_rebases_on_fetch() {
        let demo = load_bundled_demo_trace().unwrap();
        let config = SimulatorConfig::builder()
            .trace_mode(SimulatorTraceMode::DemoReplay)
            .hosts(2)
            .build();
        let backend = SimulatorBackend::new(config, &demo).unwrap();

        assert_eq!(
            backend.list_prefixes(DEFAULT_BUCKET, "").await.unwrap(),
            vec!["traces/"]
        );
        let page = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2026-07-28/service=simulated-service/time=0000/",
                10,
            )
            .await
            .unwrap();
        assert!(!page.truncated);
        assert_eq!(page.objects.len(), 2);
        assert!(page.objects.iter().all(|object| {
            object.key.starts_with(
                "traces/version=1/date=2026-07-28/service=simulated-service/time=0000/",
            )
        }));
        assert!(
            backend.cache.lock().unwrap().entries.is_empty(),
            "listing must not render or cache trace payloads"
        );

        let first = &page.objects[0];
        let first_epoch = backend.parse_key(&first.key).unwrap().epoch_secs;
        let gz = backend
            .get_object(DEFAULT_BUCKET, &first.key)
            .await
            .unwrap();
        assert_eq!(backend.cache.lock().unwrap().entries.len(), 1);
        let raw = decode_trace_file(&gz).unwrap();
        let mut decoder = Decoder::new(&raw).unwrap();
        let mut first_clock = None;
        let mut min_cpu_timestamp = u64::MAX;
        let mut cpu_samples = 0;
        let mut span_events = 0;
        decoder
            .for_each_event(|event| {
                if event.name == "ClockSyncEvent" && first_clock.is_none() {
                    first_clock = event.deserialize::<ClockSync>().ok();
                }
                if event.name == "CpuSampleEvent" {
                    min_cpu_timestamp = min_cpu_timestamp.min(event.timestamp_ns);
                    cpu_samples += 1;
                }
                if event.name.starts_with("SpanEnter:") {
                    span_events += 1;
                }
            })
            .unwrap();
        let clock = first_clock.unwrap();
        let expected_start = u64::try_from(first_epoch).unwrap() * 1_000_000_000;
        assert!((expected_start..expected_start + 60_000_000_000).contains(&min_cpu_timestamp));
        assert_eq!(
            clock.realtime_ns, clock.timestamp_ns,
            "rebased monotonic and realtime clocks should share the segment epoch"
        );
        assert!(cpu_samples > 0);
        assert!(span_events > 0);
        assert_eq!(trace_boot_id(&raw), "simu-000001");

        let second_host = page
            .objects
            .iter()
            .find(|object| object.key.contains("/instance=host-002/"))
            .unwrap();
        assert!(second_host.key.contains("/boot=simu-000002/"));
        let second_gz = backend
            .get_object(DEFAULT_BUCKET, &second_host.key)
            .await
            .unwrap();
        let second_raw = decode_trace_file(&second_gz).unwrap();
        assert_eq!(trace_boot_id(&second_raw), "simu-000002");
        assert_ne!(
            raw, second_raw,
            "hosts at the same epoch need distinct identity metadata"
        );
    }

    #[tokio::test]
    async fn catalog_is_unbounded_deterministic_and_rejects_noncanonical_keys() {
        let demo = load_bundled_demo_trace().unwrap();
        assert!(
            SimulatorBackend::new(
                SimulatorConfig::builder()
                    .hosts(MAX_SIMULATED_HOSTS + 1)
                    .build(),
                &demo,
            )
            .is_err()
        );
        let backend =
            SimulatorBackend::new(SimulatorConfig::builder().hosts(2).build(), &demo).unwrap();

        let old = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2001-02-03/service=simulated-service/time=0405/",
                10,
            )
            .await
            .unwrap();
        let future = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2037-08-09/service=simulated-service/time=1011/",
                10,
            )
            .await
            .unwrap();
        let repeated = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2037-08-09/service=simulated-service/time=1011/",
                10,
            )
            .await
            .unwrap();
        assert_eq!(old.objects.len(), 2);
        assert_eq!(future.objects.len(), 2);
        assert_eq!(
            future
                .objects
                .iter()
                .map(|object| &object.key)
                .collect::<Vec<_>>(),
            repeated
                .objects
                .iter()
                .map(|object| &object.key)
                .collect::<Vec<_>>()
        );
        assert!(backend.cache.lock().unwrap().entries.is_empty());

        let capped = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2037-08-09/service=simulated-service/time=10",
                1,
            )
            .await
            .unwrap();
        assert_eq!(capped.objects.len(), 1);
        assert!(capped.truncated);

        let key = &future.objects[0].key;
        let forged = key.replace("/boot=simu-000001/", "/boot=simu-999999/");
        assert!(matches!(
            backend.get_object(DEFAULT_BUCKET, &forged).await,
            Err(StorageError::NotFound(_))
        ));
        let (parent, file) = key.rsplit_once('/').unwrap();
        let epoch = file.split('-').next().unwrap();
        let forged = format!("{parent}/{epoch}-0.bin.gz");
        assert!(matches!(
            backend.get_object(DEFAULT_BUCKET, &forged).await,
            Err(StorageError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn synthetic_features_filter_scheduler_tasks_and_spans() {
        let demo = load_bundled_demo_trace().unwrap();
        let config = SimulatorConfig::builder()
            .features(only_cpu())
            .hosts(1)
            .build();
        let backend = SimulatorBackend::new(config, &demo).unwrap();
        let page = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2026-07-28/service=simulated-service/time=0000/",
                1,
            )
            .await
            .unwrap();
        let gz = backend
            .get_object(DEFAULT_BUCKET, &page.objects[0].key)
            .await
            .unwrap();
        let raw = decode_trace_file(&gz).unwrap();
        assert_eq!(trace_boot_id(&raw), "simu-000001");
        let mut decoder = Decoder::new(&raw).unwrap();
        let mut cpu = 0;
        let mut forbidden = Vec::new();
        decoder
            .for_each_event(|event| match event.name {
                "CpuSampleEvent" => {
                    let source = event
                        .field_names()
                        .zip(event.fields.iter())
                        .find(|(name, _)| *name == "source")
                        .and_then(|(_, value)| match value {
                            dial9_trace_format::types::FieldValueRef::Varint(value) => Some(*value),
                            _ => None,
                        });
                    if source == Some(1) {
                        forbidden.push("scheduler sample".to_string());
                    } else {
                        cpu += 1;
                    }
                }
                "WorkerParkEvent"
                | "WorkerUnparkEvent"
                | "PollStartEvent"
                | "PollEndEvent"
                | "SpanCloseEvent"
                | "QueueSampleEvent"
                | "RuntimeMetricsEvent" => forbidden.push(event.name.to_string()),
                name if name.starts_with("SpanEnter:") || name.starts_with("SpanExit:") => {
                    forbidden.push(name.to_string())
                }
                _ => {}
            })
            .unwrap();
        assert!(cpu > 0);
        assert!(forbidden.is_empty(), "unexpected events: {forbidden:?}");
    }

    #[tokio::test]
    async fn synthetic_runtime_metrics_are_gated_by_tasks_not_custom_events() {
        // `RuntimeMetricsEvent` must be filtered by the `tasks` feature, exactly
        // like the `QueueSampleEvent` it supersedes. This config is the one that
        // discriminates: with `custom_events` ON but `tasks` OFF, a
        // `RuntimeMetricsEvent` not named in `feature_enabled` would fall through
        // to the catch-all `custom_events` arm and survive. Both queue-series
        // events must be dropped together.
        let demo = load_bundled_demo_trace().unwrap();
        let features = SimulatorFeatures::builder()
            .cpu(true)
            .scheduling(true)
            .tasks(false)
            .spans(true)
            .memory(true)
            .resources(true)
            .custom_events(true)
            .build();
        let config = SimulatorConfig::builder()
            .features(features)
            .hosts(1)
            .build();
        let backend = SimulatorBackend::new(config, &demo).unwrap();
        let page = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2026-07-28/service=simulated-service/time=0000/",
                1,
            )
            .await
            .unwrap();
        let gz = backend
            .get_object(DEFAULT_BUCKET, &page.objects[0].key)
            .await
            .unwrap();
        let raw = decode_trace_file(&gz).unwrap();
        let mut decoder = Decoder::new(&raw).unwrap();
        let mut queue_series_events = Vec::new();
        decoder
            .for_each_event(|event| {
                if matches!(event.name, "RuntimeMetricsEvent" | "QueueSampleEvent") {
                    queue_series_events.push(event.name.to_string());
                }
            })
            .unwrap();
        assert!(
            queue_series_events.is_empty(),
            "tasks-disabled shaping leaked queue-series events: {queue_series_events:?}"
        );
    }

    #[tokio::test]
    async fn synthetic_realistic_symbols_resolve_cpu_stack_addresses() {
        let demo = load_bundled_demo_trace().unwrap();
        let config = SimulatorConfig::builder()
            .features(only_cpu())
            .symbol_mode(SimulatorSymbolMode::Realistic)
            .segment_duration(Duration::from_secs(15))
            .hosts(1)
            .build();
        let backend = SimulatorBackend::new(config, &demo).unwrap();
        let page = backend
            .list_objects(
                DEFAULT_BUCKET,
                "traces/version=1/date=2026-07-28/service=simulated-service/time=0000/",
                1,
            )
            .await
            .unwrap();
        let gz = backend
            .get_object(DEFAULT_BUCKET, &page.objects[0].key)
            .await
            .unwrap();
        let raw = decode_trace_file(&gz).unwrap();

        let mut symbols = std::collections::BTreeMap::new();
        let mut symbol_names = Vec::new();
        let mut cpu_frames = Vec::new();
        let mut decoder = Decoder::new(&raw).unwrap();
        decoder
            .for_each_event(|event| {
                if event.name == "SymbolTableEntry" {
                    let mut base = None;
                    let mut size = None;
                    let mut name = None;
                    for (field, value) in event.field_names().zip(event.fields.iter()) {
                        match (field, value) {
                            ("addr" | "base_addr", FieldValueRef::Varint(value)) => {
                                base = Some(*value);
                            }
                            ("size", FieldValueRef::Varint(value)) => size = Some(*value),
                            ("symbol_name", FieldValueRef::PooledString(id)) => {
                                name = event.string_pool.get(*id).map(str::to_string);
                            }
                            _ => {}
                        }
                    }
                    if let (Some(base), Some(size), Some(name)) = (base, size, name) {
                        symbols.insert(base, size.max(1));
                        symbol_names.push(name);
                    }
                } else if event.name == "CpuSampleEvent"
                    && let Some((_, callchain)) = event
                        .field_names()
                        .zip(event.fields.iter())
                        .find(|(field, _)| *field == "callchain")
                {
                    match callchain {
                        FieldValueRef::StackFrames(frames) => {
                            cpu_frames.extend(frames.iter());
                        }
                        FieldValueRef::PooledStackFrames(id) => {
                            if let Some(frames) = event.stack_pool.get(*id) {
                                cpu_frames.extend(frames.iter().copied());
                            }
                        }
                        _ => {}
                    }
                }
            })
            .unwrap();

        assert!(
            symbol_names.len() > 100,
            "expected a populated symbol table"
        );
        assert!(
            symbol_names
                .iter()
                .all(|name| name.contains("::") && !name.starts_with("s_")),
            "unexpected synthetic symbol names"
        );
        assert!(
            cpu_frames.iter().any(|frame| {
                symbols
                    .range(..=frame)
                    .next_back()
                    .is_some_and(|(base, size)| *frame < base.saturating_add(*size))
            }),
            "CPU callchains should resolve against the synthetic symbol table"
        );
    }
}
