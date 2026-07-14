pub mod cli;
pub mod ingest;
pub mod report_serve;
pub mod server;
pub mod storage;

pub use report_serve::report_serve_router;

// Expose the standard per-request metrics sink so a caller embedding
// `build_app` can attach it — or supply their own metrique sink instead.
// Metrics are a process-global concern the caller owns, like logging.
pub use server::metrics::attach_request_metrics;

use std::path::PathBuf;

async fn detect_bucket_region(bucket: &str) -> Option<String> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    server::region_from_head_bucket(&client, bucket).await
}

/// Configuration for [`build_app`]. Construct it directly in code, or map it
/// from CLI args (see [`cli::run`]).
///
/// Deliberately excluded, because they are the caller's concern rather than
/// part of app assembly:
///   - the listen **port** — binding is the caller's job (see [`build_app`]);
///   - **logging** and the per-request **metrics** format — see [`init_tracing`]
///     and [`attach_request_metrics`], which the caller drives (often from a
///     `--local`/deployed flag).
#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub local_dir: Option<PathBuf>,
    pub dev: bool,
    /// Enable demand-driven aggregation against the S3 `bucket`/`prefix` source.
    pub agg: bool,
    /// When set, enable demand-driven aggregation reading raw segments from
    /// this local directory (local equivalent of `agg`).
    pub agg_source_dir: Option<PathBuf>,
    /// Where the on-demand aggregator writes its Parquet output (local).
    /// Defaults to `<agg_source_dir>/flamegraph-data`.
    pub agg_output_dir: Option<PathBuf>,
    /// Output S3 bucket for aggregator part-files. Defaults to the source bucket.
    pub agg_output_bucket: Option<String>,
    /// Output S3 key prefix for aggregator part-files.
    pub agg_output_prefix: String,
    /// Raw-trace segment duration (seconds) for the scope time-filter pad.
    pub agg_segment_secs: i64,
    /// Enable the temporary trace-upload feature (`POST /api/upload`). Off by
    /// default; there is no auth, so only enable on a trusted network.
    pub enable_upload: bool,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            bucket: None,
            prefix: None,
            local_dir: None,
            dev: false,
            agg: false,
            agg_source_dir: None,
            agg_output_dir: None,
            agg_output_bucket: None,
            agg_output_prefix: "flamegraph-data".to_string(),
            agg_segment_secs: crate::ingest::aggregate::DEFAULT_SEGMENT_DURATION_SECS,
            enable_upload: false,
        }
    }
}

/// Build an [`S3Backend`] for `bucket`, pinned to the bucket's region when it
/// can be detected (so cross-region buckets work), else the default chain.
pub(crate) async fn s3_backend_for(bucket: &str) -> storage::S3Backend {
    if let Some(region) = detect_bucket_region(bucket).await {
        tracing::info!(%region, %bucket, "detected bucket region");
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .load()
            .await;
        storage::S3Backend::from_client(aws_sdk_s3::Client::new(&config))
    } else {
        tracing::warn!(%bucket, "could not detect bucket region, using default");
        storage::S3Backend::from_env().await
    }
}

/// Initialize the process-global tracing subscriber: JSON logs by default (so
/// they render cleanly in CloudWatch), human-readable under `local`. Call once,
/// before serving. Logging is the binary's concern, so it is separate from
/// [`build_app`].
pub fn init_tracing(local: bool) {
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "dial9_viewer=info".parse().unwrap())
    };
    if local {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter())
            .init();
    }
}

/// Assemble the fully-configured viewer application — routes, storage backend,
/// and optional demand-driven aggregation — and return it as an [`axum::Router`],
/// which is also a `tower::Service`.
///
/// Binding is the caller's responsibility: add routes with [`axum::Router::merge`],
/// wrap middleware such as auth with [`axum::Router::layer`], pick a
/// server/listener/TLS, then serve. Two other process-global concerns are the
/// caller's as well, so they compose freely and can be swapped:
///   - **logging** — call [`init_tracing`] once beforehand;
///   - **per-request metrics** — attach a sink with [`attach_request_metrics`]
///     (the standard EMF/local sink) or supply your own metrique sink, and hold
///     the returned handle for the life of the server.
pub async fn build_app(
    ViewerConfig {
        bucket,
        prefix,
        local_dir,
        dev,
        agg,
        agg_source_dir,
        agg_output_dir,
        agg_output_bucket,
        agg_output_prefix,
        agg_segment_secs,
        enable_upload,
    }: ViewerConfig,
) -> anyhow::Result<axum::Router> {
    // Build the demand-driven aggregation context if requested. Two sources:
    //   - `agg_source_dir` (local): source + output are LocalBackends.
    //   - `agg` + `bucket` (S3): source is the served bucket/prefix; output is a
    //     (possibly different) bucket. Both go through region-aware S3 clients.
    use crate::ingest::aggregate::AggContext;
    let agg_output_prefix_for_state = agg_output_prefix.clone();
    let agg = if let Some(src_dir) = &agg_source_dir {
        let src_dir = std::fs::canonicalize(src_dir)?;
        let out_dir = agg_output_dir.unwrap_or_else(|| src_dir.join("flamegraph-data"));
        std::fs::create_dir_all(&out_dir)?;
        let out_dir = std::fs::canonicalize(&out_dir)?;
        tracing::info!(
            source = %src_dir.display(),
            output = %out_dir.display(),
            "demand-driven aggregation enabled (local)"
        );
        Some(AggContext {
            source: std::sync::Arc::new(storage::LocalBackend::new(&src_dir)),
            output: std::sync::Arc::new(storage::LocalBackend::new(&out_dir)),
            source_bucket: "local".to_string(),
            source_is_local: true,
            output_bucket: "local".to_string(),
            output_prefix: ".".to_string(),
            source_prefixes: vec![String::new()],
            segment_duration_secs: agg_segment_secs,
        })
    } else if agg {
        let Some(src_bucket) = bucket.clone() else {
            anyhow::bail!("--agg requires --bucket (the S3 source of raw traces)");
        };
        let out_bucket = agg_output_bucket
            .clone()
            .unwrap_or_else(|| src_bucket.clone());
        let source = std::sync::Arc::new(s3_backend_for(&src_bucket).await);
        // Output may be a different bucket/account/region → its own client.
        let output = std::sync::Arc::new(s3_backend_for(&out_bucket).await);
        tracing::info!(
            source_bucket = %src_bucket,
            output_bucket = %out_bucket,
            output_prefix = %agg_output_prefix,
            "demand-driven aggregation enabled (S3)"
        );
        Some(AggContext {
            source,
            output,
            source_bucket: src_bucket,
            source_is_local: false,
            output_bucket: out_bucket,
            output_prefix: agg_output_prefix,
            // The served `prefix` (if any) scopes the raw-segment listing.
            source_prefixes: vec![prefix.clone().unwrap_or_default()],
            segment_duration_secs: agg_segment_secs,
        })
    } else {
        None
    };

    let dev_ui_dir = if dev {
        let candidates = [PathBuf::from("ui"), PathBuf::from("dial9-viewer/ui")];
        let dir = candidates.into_iter().find(|p| p.exists());
        match dir {
            Some(d) => {
                tracing::info!(path = %d.display(), "dev mode: serving UI from disk");
                Some(d)
            }
            None => {
                anyhow::bail!(
                    "--dev: could not find ui/ directory. Run from the dial9-viewer/ or repo root directory."
                );
            }
        }
    } else {
        None
    };

    // Build the base state per backend. `source_is_s3` is true for every S3
    // backend; it is false only in local-dir mode (and local-source
    // aggregation), where the data is local. It drives BYO credentials, the
    // creds panel, and on-demand aggregation (see `AppState::allow_byo_creds`).
    let (mut app_state, source_is_s3) = if let Some(agg) = &agg {
        // Demand-driven mode: browse endpoints read the raw segments from the
        // same source backend, and `/api/flamegraph` runs the refinement loop.
        // The browse default bucket is the agg source bucket ("local" for a
        // local source, the real bucket for S3). An S3 source supports BYO
        // credentials; a local-directory source does not.
        let source_is_s3 = !agg.source_is_local;
        let state = server::AppState::new(
            std::sync::Arc::clone(&agg.source),
            Some(agg.source_bucket.clone()),
            prefix.clone(),
        )
        .with_agg(agg.clone());
        (state, source_is_s3)
    } else if let Some(dir) = &local_dir {
        let dir = std::fs::canonicalize(dir)?;
        tracing::info!(path = %dir.display(), "serving traces from local directory");
        let backend = storage::LocalBackend::new(&dir);
        let state = server::AppState::new(
            std::sync::Arc::new(backend),
            Some("local".into()),
            prefix.clone(),
        );
        (state, false)
    } else if let Some(bucket_name) = &bucket {
        if let Some(region) = detect_bucket_region(bucket_name).await {
            tracing::info!(%region, bucket = %bucket_name, "detected bucket region");
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(region))
                .load()
                .await;
            let client = aws_sdk_s3::Client::new(&config);
            let backend = storage::S3Backend::from_client(client);
            let state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            (state, true)
        } else {
            tracing::warn!(bucket = %bucket_name, "could not detect bucket region, using default");
            let backend = storage::S3Backend::from_env().await;
            let state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            (state, true)
        }
    } else {
        let backend = storage::S3Backend::from_env().await;
        let state =
            server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
        (state, true)
    };

    // When an output bucket is configured, build its region-aware backend once
    // (ambient identity — the operator owns this bucket) and hand both to the
    // state. The `/api/flamegraph` BYOC path writes aggregated part-files here
    // instead of back into the (often read-only) source bucket. Without this,
    // aggregation against a read-only source fails with S3 AccessDenied on the
    // first PutObject.
    let agg_output_backend: Option<std::sync::Arc<dyn storage::StorageBackend>> =
        match &agg_output_bucket {
            Some(out_bucket) => {
                tracing::info!(
                    %out_bucket,
                    "aggregation output bucket configured (writes go here, not the source)"
                );
                Some(std::sync::Arc::new(s3_backend_for(out_bucket).await))
            }
            None => None,
        };

    app_state = app_state
        .with_byo_creds(source_is_s3)
        .with_agg_output_prefix(agg_output_prefix_for_state)
        .with_agg_output_bucket(agg_output_bucket, agg_output_backend)
        .with_agg_segment_secs(agg_segment_secs);
    // For an S3 source, also offer the assume-role path: a request may name a
    // role ARN and the viewer assumes it with its own (ambient) identity via
    // STS. Same gate as BYOC — both require an S3 source; this additionally
    // relies on the server having an ambient identity allowed to assume the
    // target role. A local-dir source has no S3 and gets neither.
    if source_is_s3 {
        let assumer = server::credentials::StsRoleAssumer::from_env().await;
        app_state = app_state.with_role_assumer(std::sync::Arc::new(assumer));
    }
    if let Some(d) = dev_ui_dir {
        app_state = app_state.with_dev_ui_dir(d);
    }
    if enable_upload {
        tracing::info!(
            "trace-upload feature enabled (POST /api/upload); no auth — trusted network only"
        );
        app_state = app_state.with_uploads(server::UploadLimits::default());
    }

    let app = server::router(app_state);
    Ok(app)
}

/// Wait for a shutdown signal (Ctrl-C), then resolve. Pass this to
/// `axum::serve(...).with_graceful_shutdown(...)` when binding a router from
/// [`build_app`].
pub async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    tracing::info!("shutting down");
}
