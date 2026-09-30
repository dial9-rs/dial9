pub mod cli;
pub mod ingest;
pub mod report_serve;
#[cfg(feature = "s3")]
mod s3;
#[cfg(not(feature = "s3"))]
#[path = "s3_disabled.rs"]
mod s3;
mod segment_object_key_codec;
mod segment_object_key_parser;
pub mod server;
pub mod simulator;
mod source_key_scope;
mod source_layout;
pub mod storage;
mod trace_shape;

pub use report_serve::report_serve_router;

// Expose the standard per-request metrics sink so a caller embedding
// `build_app` can attach it — or supply their own metrique sink instead.
// Metrics are a process-global concern the caller owns, like logging.
pub use server::metrics::attach_request_metrics;

use std::path::PathBuf;

pub(crate) fn resolve_dev_ui_dir(dev: bool) -> anyhow::Result<Option<PathBuf>> {
    if !dev {
        return Ok(None);
    }

    // Serve the BUILT UI (ui/dist), not the ui/ sources: the servable set
    // is the vite build output (root assets such as demo-trace.bin and
    // flamegraph.css live in ui/public/ and only appear at the served
    // root via a build). Keep it fresh with `npm run dev:embedded`
    // (vite build --watch) for the edit-refresh loop.
    let candidates = [
        PathBuf::from("ui/dist"),
        PathBuf::from("dial9-viewer/ui/dist"),
    ];
    let Some(dir) = candidates.into_iter().find(|p| p.exists()) else {
        anyhow::bail!(
            "--dev: could not find ui/dist/ directory. Run from the dial9-viewer/ or repo root directory."
        );
    };

    if !dir.join("index.html").exists() {
        tracing::warn!(
            path = %dir.display(),
            "ui/dist has no built UI - run `npm run build` or `npm run dev:embedded` \
             in dial9-viewer/ui first (UI work requires Node, see ui/README.md)"
        );
    }
    tracing::info!(path = %dir.display(), "dev mode: serving UI from disk");
    Ok(Some(dir))
}

#[cfg(feature = "s3")]
pub(crate) use s3::backend_for as s3_backend_for;

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
    /// Optional persistent S3 destination for aggregator part-files. When unset,
    /// S3/BYOC aggregate output uses a process-local temporary directory.
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
pub async fn build_app(config: ViewerConfig) -> anyhow::Result<axum::Router> {
    build_app_from(config, None).await
}

/// [`build_app`] over a storage backend you supply, for an object store the
/// built-in backends do not cover (Google Cloud Storage, Azure Blob,
/// in-house solution, ...). Everything else about the assembled app
/// is identical.
///
/// The backend is the source for browsing, and for demand-driven aggregation
/// when [`ViewerConfig::agg`] is set; `bucket` names the container it lists and
/// `prefix` scopes it. Aggregate *output* is unchanged — the configured
/// `agg_output_bucket`, or a process-local temporary directory — so writes
/// never go back to the source.
///
/// The request paths that are S3 by construction stay off: bring-your-own
/// credentials and the assume-role flow mean nothing for another store.
///
/// `local_dir` and `agg_source_dir` select the built-in local backend, so
/// combining either with a supplied backend is an error rather than a silent
/// precedence rule.
///
/// ```no_run
/// # async fn example(backend: std::sync::Arc<dyn dial9_viewer::storage::StorageBackend>)
/// # -> anyhow::Result<()> {
/// let app = dial9_viewer::build_app_with_backend(
///     dial9_viewer::ViewerConfig {
///         bucket: Some("my-trace-bucket".to_string()),
///         ..Default::default()
///     },
///     backend,
/// )
/// .await?;
/// # let _ = app;
/// # Ok(())
/// # }
/// ```
pub async fn build_app_with_backend(
    config: ViewerConfig,
    backend: std::sync::Arc<dyn storage::StorageBackend>,
) -> anyhow::Result<axum::Router> {
    build_app_from(config, Some(backend)).await
}

async fn build_app_from(
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
    supplied_backend: Option<std::sync::Arc<dyn storage::StorageBackend>>,
) -> anyhow::Result<axum::Router> {
    match &supplied_backend {
        // A supplied backend answers for the source, so the built-in source
        // selection — and the S3-feature checks that police it — do not apply.
        Some(_) => {
            if local_dir.is_some() || agg_source_dir.is_some() {
                anyhow::bail!(
                    "a supplied storage backend and `local_dir`/`agg_source_dir` both name \
                     the source; pass one or the other"
                );
            }
        }
        None => s3::validate_config(
            bucket.as_deref(),
            agg,
            agg_output_bucket.as_deref(),
            local_dir.as_deref(),
            agg_source_dir.as_deref(),
        )?,
    }

    // Build one aggregate-output destination shared by the configured
    // aggregation context and all BYOC requests. An explicit output bucket
    // retains the persistent S3 behavior; otherwise output uses a process-local
    // temporary directory that is removed when the server drops it.
    use crate::ingest::aggregate::AggContext;
    let agg_output = s3::aggregate_output(agg_output_bucket.as_deref(), &agg_output_prefix).await?;
    tracing::info!(
        output = %agg_output.location(),
        "aggregate output destination (writes go here, never the source)"
    );

    // Build the demand-driven aggregation context if requested. Three sources:
    //   - a supplied backend: source is that backend over `bucket`/`prefix`.
    //   - `agg_source_dir` (local): source + output are LocalBackends.
    //   - `agg` + `bucket` (S3): source is the served bucket/prefix; output is
    //     the configured S3 bucket or a process-local temporary directory.
    let agg = if let Some(backend) = &supplied_backend {
        if agg {
            let Some(source_bucket) = bucket.as_deref() else {
                anyhow::bail!("`agg` over a supplied storage backend requires `bucket`");
            };
            tracing::info!(
                %source_bucket,
                output = %agg_output.location(),
                "demand-driven aggregation enabled (supplied backend)"
            );
            Some(AggContext {
                source: std::sync::Arc::clone(backend),
                output: agg_output.backend(),
                output_bucket: agg_output.output_bucket_for(source_bucket),
                source_bucket: source_bucket.to_string(),
                // Not the local filesystem, so segment keys stay fully
                // qualified, as they are for S3 and the simulator.
                source_is_local: false,
                output_prefix: agg_output.prefix().to_string(),
                // The served `prefix` (if any) scopes the raw-segment listing.
                source_prefixes: vec![prefix.clone().unwrap_or_default()],
                segment_duration_secs: agg_segment_secs,
            })
        } else {
            None
        }
    } else if let Some(src_dir) = &agg_source_dir {
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
        s3::aggregate_context(
            bucket.as_deref(),
            prefix.as_deref(),
            &agg_output,
            agg_segment_secs,
        )
        .await?
    } else {
        None
    };

    let dev_ui_dir = resolve_dev_ui_dir(dev)?;

    // Build the base state per backend. `source_is_s3` is true for every S3
    // backend; it is false only in local-dir mode (and local-source
    // aggregation), where the data is local. It drives BYO credentials, the
    // creds panel, and on-demand aggregation (see `AppState::allow_byo_creds`).
    let (mut app_state, source_is_s3) = if let Some(agg) = &agg {
        // Demand-driven mode: browse endpoints read the raw segments from the
        // same source backend, and `/api/flamegraph` runs the refinement loop.
        // The browse default bucket is the agg source bucket ("local" for a
        // local source, the real bucket for S3). An S3 source supports BYO
        // credentials; a local-directory source does not, and neither does a
        // supplied backend — its store is not S3 whatever it addresses.
        let source_is_s3 = !agg.source_is_local && supplied_backend.is_none();
        let state = server::AppState::new(
            std::sync::Arc::clone(&agg.source),
            Some(agg.source_bucket.clone()),
            prefix.clone(),
        )
        .with_agg(agg.clone());
        (state, source_is_s3)
    } else if let Some(backend) = &supplied_backend {
        tracing::info!(?bucket, "serving traces from a supplied storage backend");
        let state = server::AppState::new(
            std::sync::Arc::clone(backend),
            bucket.clone(),
            prefix.clone(),
        );
        (state, false)
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
    } else {
        s3::app_state(bucket.as_deref(), prefix.as_deref()).await?
    };

    // Hand the same output destination to BYOC aggregation. With an explicit
    // bucket this is the region-aware S3 client built above; otherwise it is
    // the process-local temporary directory. In neither mode do aggregate
    // writes use the source backend or the caller's read credentials.
    app_state = app_state
        .with_byo_creds(source_is_s3)
        .with_agg_output(agg_output)
        .with_agg_segment_secs(agg_segment_secs);
    // For an S3 source, also offer the assume-role path: a request may name a
    // role ARN and the viewer assumes it with its own (ambient) identity via
    // STS. Same gate as BYOC — both require an S3 source; this additionally
    // relies on the server having an ambient identity allowed to assume the
    // target role. A local-dir source has no S3 and gets neither.
    app_state = s3::with_role_assumer(app_state, source_is_s3).await;
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

#[cfg(test)]
mod supplied_backend_tests {
    use super::{ViewerConfig, build_app_with_backend, storage};
    use std::sync::Arc;

    fn backend(dir: &std::path::Path) -> Arc<dyn storage::StorageBackend> {
        Arc::new(storage::LocalBackend::new(dir))
    }

    /// The supplied backend is the source, so neither a bucket nor a local
    /// directory is needed — and on a build without the `s3` feature, the
    /// source checks that would otherwise reject this must not run.
    #[tokio::test]
    async fn builds_without_bucket_or_local_dir() {
        let traces = tempfile::tempdir().unwrap();
        let app = build_app_with_backend(ViewerConfig::default(), backend(traces.path())).await;
        assert!(app.is_ok(), "{:?}", app.err());
    }

    #[tokio::test]
    async fn builds_with_aggregation_over_the_supplied_backend() {
        let traces = tempfile::tempdir().unwrap();
        let app = build_app_with_backend(
            ViewerConfig {
                bucket: Some("traces".to_string()),
                agg: true,
                ..ViewerConfig::default()
            },
            backend(traces.path()),
        )
        .await;
        assert!(app.is_ok(), "{:?}", app.err());
    }

    #[tokio::test]
    async fn aggregation_needs_a_bucket_to_list() {
        let traces = tempfile::tempdir().unwrap();
        let error = build_app_with_backend(
            ViewerConfig {
                agg: true,
                ..ViewerConfig::default()
            },
            backend(traces.path()),
        )
        .await
        .expect_err("aggregation without a bucket should not build");
        assert!(error.to_string().contains("requires `bucket`"), "{error}");
    }

    /// Two sources with no ordering between them is a caller mistake worth
    /// reporting rather than resolving.
    #[tokio::test]
    async fn rejects_a_second_source() {
        let traces = tempfile::tempdir().unwrap();
        let error = build_app_with_backend(
            ViewerConfig {
                local_dir: Some(traces.path().to_path_buf()),
                ..ViewerConfig::default()
            },
            backend(traces.path()),
        )
        .await
        .expect_err("a supplied backend plus local_dir should not build");
        assert!(
            error.to_string().contains("pass one or the other"),
            "{error}"
        );
    }
}

#[cfg(all(test, not(feature = "s3")))]
mod no_s3_tests {
    use super::{ViewerConfig, build_app};

    #[tokio::test]
    async fn local_app_builds_without_s3() {
        let traces = tempfile::tempdir().unwrap();
        let app = build_app(ViewerConfig {
            local_dir: Some(traces.path().to_path_buf()),
            ..ViewerConfig::default()
        })
        .await;
        assert!(app.is_ok(), "{app:?}");
    }

    #[tokio::test]
    async fn bucket_configuration_is_rejected_without_s3() {
        let result = build_app(ViewerConfig {
            bucket: Some("trace-bucket".to_string()),
            ..ViewerConfig::default()
        })
        .await;
        let error = result.expect_err("S3 configuration must be rejected");
        let message = error.to_string();
        assert!(message.contains("`bucket` (`--bucket`)"), "{message}");
        assert!(message.contains("`local_dir` (`--local-dir`)"), "{message}");
    }

    #[tokio::test]
    async fn s3_aggregation_is_rejected_without_s3() {
        let traces = tempfile::tempdir().unwrap();
        let result = build_app(ViewerConfig {
            local_dir: Some(traces.path().to_path_buf()),
            agg: true,
            ..ViewerConfig::default()
        })
        .await;
        let error = result.expect_err("S3 aggregation must be rejected");
        let message = error.to_string();
        assert!(message.contains("`agg` (`--agg`)"), "{message}");
        assert!(
            message.contains("`agg_source_dir` (`--agg-source-dir`)"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn s3_aggregate_output_is_rejected_without_s3() {
        let traces = tempfile::tempdir().unwrap();
        let result = build_app(ViewerConfig {
            local_dir: Some(traces.path().to_path_buf()),
            agg_output_bucket: Some("aggregate-bucket".to_string()),
            ..ViewerConfig::default()
        })
        .await;
        let error = result.expect_err("S3 aggregate output must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("`agg_output_bucket` (`--agg-output-bucket`)"),
            "{message}"
        );
        assert!(
            message.contains("`agg_output_dir` (`--agg-output-dir`)"),
            "{message}"
        );
    }
}
