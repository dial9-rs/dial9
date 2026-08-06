use crate::ingest::aggregate::AggContext;
use crate::server::{AggOutput, AppState};
use crate::storage::S3Backend;
use std::path::Path;
use std::sync::Arc;

pub(super) fn validate_config(
    _bucket: Option<&str>,
    _agg: bool,
    _agg_output_bucket: Option<&str>,
    _local_dir: Option<&Path>,
    _agg_source_dir: Option<&Path>,
) -> anyhow::Result<()> {
    Ok(())
}

async fn detect_bucket_region(bucket: &str) -> Option<String> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    crate::server::region_from_head_bucket(&client, bucket).await
}

/// Build an [`S3Backend`] for `bucket`, pinned to the bucket's region when it
/// can be detected (so cross-region buckets work), else the default chain.
pub(crate) async fn backend_for(bucket: &str) -> S3Backend {
    if let Some(region) = detect_bucket_region(bucket).await {
        tracing::info!(%region, %bucket, "detected bucket region");
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .load()
            .await;
        S3Backend::from_client(aws_sdk_s3::Client::new(&config))
    } else {
        tracing::warn!(%bucket, "could not detect bucket region, using default");
        S3Backend::from_env().await
    }
}

/// Build one aggregate-output destination shared by the configured aggregation
/// context and all BYOC requests. An explicit output bucket retains the
/// persistent S3 behavior; otherwise output uses a process-local temporary
/// directory that is removed when the server drops it.
pub(super) async fn aggregate_output(
    out_bucket: Option<&str>,
    output_prefix: &str,
) -> anyhow::Result<AggOutput> {
    if let Some(out_bucket) = out_bucket {
        let backend = Arc::new(backend_for(out_bucket).await);
        Ok(AggOutput::s3(out_bucket, backend).with_prefix(output_prefix))
    } else {
        Ok(AggOutput::temporary().with_prefix(output_prefix))
    }
}

/// Build the S3 demand-driven aggregation context. The served prefix scopes
/// the raw-segment listing, while output always uses the server-owned backend.
pub(super) async fn aggregate_context(
    bucket: Option<&str>,
    prefix: Option<&str>,
    agg_output: &AggOutput,
    segment_duration_secs: i64,
) -> anyhow::Result<Option<AggContext>> {
    let Some(src_bucket) = bucket else {
        anyhow::bail!("--agg requires --bucket (the S3 source of raw traces)");
    };
    let source = Arc::new(backend_for(src_bucket).await);
    tracing::info!(
        source_bucket = %src_bucket,
        output = %agg_output.location(),
        output_prefix = %agg_output.prefix(),
        "demand-driven aggregation enabled (S3 source)"
    );
    Ok(Some(AggContext {
        source,
        output: agg_output.backend(),
        output_bucket: agg_output.output_bucket_for(src_bucket),
        source_bucket: src_bucket.to_string(),
        source_is_local: false,
        output_prefix: agg_output.prefix().to_string(),
        // The served `prefix` (if any) scopes the raw-segment listing.
        source_prefixes: vec![prefix.unwrap_or_default().to_string()],
        segment_duration_secs,
    }))
}

/// Build the default S3-backed application state when no local source or
/// explicit aggregation context was selected.
pub(super) async fn app_state(
    bucket: Option<&str>,
    prefix: Option<&str>,
) -> anyhow::Result<(AppState, bool)> {
    let backend = match bucket {
        Some(bucket) => backend_for(bucket).await,
        None => S3Backend::from_env().await,
    };
    Ok((
        AppState::new(
            Arc::new(backend),
            bucket.map(str::to_string),
            prefix.map(str::to_string),
        ),
        true,
    ))
}

/// For an S3 source, offer the assume-role path using the server's ambient
/// identity. A local-directory source has no S3 and gets no assumer.
pub(super) async fn with_role_assumer(mut state: AppState, source_is_s3: bool) -> AppState {
    if source_is_s3 {
        let assumer = crate::server::credentials::StsRoleAssumer::from_env().await;
        state = state.with_role_assumer(Arc::new(assumer));
    }
    state
}
