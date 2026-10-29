//! Bounded lowest-rank sketches over immutable completed-window reports.

use super::Config;
use crate::backend::{ListRequest, SamplingStore, StoreError, StoreErrorKind};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};

pub(super) const SPACE: u128 = 1u128 << 64;

/// Null encodes the entire 64-bit space, without losing precision in JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(super) struct Cutoff(pub(super) Option<u64>);
impl Cutoff {
    pub(super) fn value(&self) -> u128 {
        self.0.map_or(SPACE, u128::from)
    }
    pub(super) fn probability(&self) -> f64 {
        self.value() as f64 / SPACE as f64
    }
    pub(super) fn threshold(value: u128) -> Self {
        Self((value < SPACE).then_some(value as u64))
    }
    pub(super) fn from_probability(probability: f64) -> Self {
        Self::threshold((probability.clamp(0.0, 1.0) * SPACE as f64) as u128)
    }
    pub(super) fn includes(&self, rank: u64) -> bool {
        u128::from(rank) < self.value()
    }
}
fn invalid(message: &str) -> StoreError {
    StoreError::new(StoreErrorKind::invalid_response(), message)
}

pub(super) fn report_key(
    config: &Config,
    channel: &str,
    window: u64,
    rank: u64,
    cutoff: &Cutoff,
) -> String {
    format!(
        "{}{rank:016x}.{}.{:017x}",
        config.report_prefix(channel, window),
        config.invocation_id,
        cutoff.value(),
    )
}
struct Report {
    rank: u64,
    cutoff: u128,
    identity: String,
}
fn parse(key: &str, prefix: &str, profiler: bool) -> Result<Report, StoreError> {
    let suffix = key
        .strip_prefix(prefix)
        .ok_or_else(|| invalid("report outside requested prefix"))?;
    let mut fields = suffix.split('.');
    let rank_hex = fields
        .next()
        .ok_or_else(|| invalid("missing report rank"))?;
    let identity = fields
        .next()
        .ok_or_else(|| invalid("missing run identity"))?;
    let cutoff_hex = fields
        .next()
        .ok_or_else(|| invalid("missing chosen cutoff"))?;
    let rank = u64::from_str_radix(rank_hex, 16).map_err(|_| invalid("invalid report rank"))?;
    let cutoff =
        u128::from_str_radix(cutoff_hex, 16).map_err(|_| invalid("invalid chosen cutoff"))?;
    let id = identity
        .parse::<ulid::Ulid>()
        .map_err(|_| invalid("invalid run identity"))?;
    if fields.next().is_some()
        || rank_hex != format!("{rank:016x}")
        || identity != id.to_string()
        || cutoff_hex != format!("{cutoff:017x}")
        || cutoff == 0
        || cutoff > SPACE
        || u128::from(rank) >= cutoff
        || (profiler && cutoff != SPACE)
    {
        return Err(invalid("invalid report encoding or eligibility"));
    }
    Ok(Report {
        rank,
        cutoff,
        identity: identity.into(),
    })
}

#[derive(Debug)]
pub(super) struct Sample {
    pub(super) estimate: f64,
    pub(super) retained: usize,
}

/// A short truncated page cannot establish a boundary or exhaustion. There are
/// no replacement reads for malformed objects, duplicate identities, or ties.
pub(super) async fn observe(
    store: &dyn SamplingStore,
    config: &Config,
    channel: &str,
    window: u64,
    keep: usize,
    timeout: Duration,
) -> Result<Sample, StoreError> {
    let record_budget = keep
        .checked_add(1)
        .ok_or_else(|| invalid("record budget overflow"))?;
    let page_budget = record_budget.div_ceil(1000);
    let prefix = config.report_prefix(channel, window);
    let mut reports = Vec::with_capacity(record_budget);
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut identities = HashSet::new();
    let mut last_key: Option<String> = None;
    let mut exhausted = false;
    for _ in 0..page_budget {
        let size = (record_budget - reports.len()).min(1000);
        let page = super::engine::bounded(
            timeout,
            store.list_sorted_page(
                ListRequest::builder()
                    .prefix(&prefix)
                    .page_size(size)
                    .maybe_continuation(cursor)
                    .build(),
            ),
        )
        .await?;
        if page.objects().len() > size {
            return Err(invalid("LIST exceeded requested page bound"));
        }
        for object in page.objects() {
            if last_key
                .as_ref()
                .is_some_and(|last| last.as_str() >= object.key())
            {
                return Err(invalid("LIST keys are not globally strictly ordered"));
            }
            let report = parse(object.key(), &prefix, channel == "profilers")?;
            if !identities.insert(report.identity.clone()) {
                return Err(invalid("duplicate run identity in census window"));
            }
            last_key = Some(object.key().into());
            reports.push(report);
        }
        cursor = page.continuation().cloned();
        exhausted = cursor.is_none();
        if let Some(token) = &cursor
            && !cursors.insert(token.as_str().to_owned())
        {
            return Err(invalid("repeated LIST continuation cursor"));
        }
        if exhausted || reports.len() == record_budget {
            break;
        }
    }
    let boundary = if reports.len() == record_budget {
        u128::from(reports[keep].rank)
    } else if exhausted {
        SPACE
    } else {
        return Err(invalid(
            "LIST page cap reached without boundary or verified exhaustion",
        ));
    };
    if boundary == 0 {
        return Err(invalid("zero rank boundary has no usable retained sample"));
    }
    let mut estimate = 0.0;
    let mut retained = 0;
    for report in reports
        .iter()
        .filter(|report| u128::from(report.rank) < boundary)
    {
        estimate += SPACE as f64 / report.cutoff.min(boundary) as f64;
        retained += 1;
    }
    if !estimate.is_finite() {
        return Err(invalid("census estimate overflow"));
    }
    Ok(Sample { estimate, retained })
}
