use crate::ingest::aggregate::AggContext;
use crate::server::{AggOutput, AppState};
use std::path::Path;

pub(super) fn validate_config(
    bucket: Option<&str>,
    agg: bool,
    agg_output_bucket: Option<&str>,
    local_dir: Option<&Path>,
    agg_source_dir: Option<&Path>,
) -> anyhow::Result<()> {
    if bucket.is_some() {
        anyhow::bail!(
            "this dial9-viewer build has no S3 support; `bucket` (`--bucket`) requires \
             the `s3` feature; use `local_dir` (`--local-dir`) for a local source"
        );
    }
    if agg {
        anyhow::bail!(
            "this dial9-viewer build has no S3 support; `agg` (`--agg`) requires the \
             `s3` feature; use `agg_source_dir` (`--agg-source-dir`) for local aggregation"
        );
    }
    if agg_output_bucket.is_some() {
        anyhow::bail!(
            "this dial9-viewer build has no S3 support; `agg_output_bucket` \
             (`--agg-output-bucket`) requires the `s3` feature; use `agg_output_dir` \
             (`--agg-output-dir`) or leave it unset for temporary local output"
        );
    }
    if local_dir.is_none() && agg_source_dir.is_none() {
        anyhow::bail!(
            "this dial9-viewer build has no S3 support; set `local_dir` (`--local-dir`) \
             or `agg_source_dir` (`--agg-source-dir`)"
        );
    }
    Ok(())
}

pub(super) async fn aggregate_output(
    out_bucket: Option<&str>,
    output_prefix: &str,
) -> anyhow::Result<AggOutput> {
    if out_bucket.is_some() {
        anyhow::bail!(
            "this dial9-viewer build has no S3 support; enable the `s3` feature or \
             use a local aggregate output"
        );
    }
    Ok(AggOutput::temporary().with_prefix(output_prefix))
}

pub(super) async fn aggregate_context(
    _bucket: Option<&str>,
    _prefix: Option<&str>,
    _agg_output: &AggOutput,
    _segment_duration_secs: i64,
) -> anyhow::Result<Option<AggContext>> {
    anyhow::bail!(
        "this dial9-viewer build has no S3 support; enable the `s3` feature or \
         use `agg_source_dir`"
    )
}

pub(super) async fn app_state(
    _bucket: Option<&str>,
    _prefix: Option<&str>,
) -> anyhow::Result<(AppState, bool)> {
    anyhow::bail!("this dial9-viewer build has no S3 support; set `local_dir` or `agg_source_dir`")
}

pub(super) async fn with_role_assumer(state: AppState, _source_is_s3: bool) -> AppState {
    state
}
