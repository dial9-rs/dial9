//! Server-side, selected-type tail comparison. Reuses credential-scoped folds;
//! sends summaries and bounded examples, never the full observation dataset.
use std::collections::{BTreeMap, BTreeSet};

use axum::{Json, body::Bytes, extract::State, http::StatusCode};
use axum_extra::extract::Query;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::{
    AttrFilter, Exemplar, RowComposition, SpanStatsInput, SpanStatsParams, SpanStatsRow,
    SpansBatchReader, parse_attr_filters, parse_span_type_uid, raw_trace_bytes,
};
use crate::ingest::{aggregate, decode, refine};
use crate::server::{AppState, credentials::MaybeCreds};

const MAX_FILES: usize = 24;
const MAX_RECORDS: usize = 1_000_000;
const MAX_FIELDS: usize = 32;
// Discovery needs room for elapsed aliases and constants before the visible
// field limit is applied. Both discovery and output remain bounded.
const MAX_NUMERIC_CANDIDATES: usize = MAX_FIELDS * 4;
const MAX_GROUP_VALUES: usize = 12;
const HISTOGRAM_LOG_BINS: usize = 36;
const DENSITY_VALUE_BINS: usize = 32;
const ALIAS_MIN_COHORT_OBSERVATIONS: usize = 20;
const PROPERTY_MIN_SAMPLES: usize = 100;
const PROPERTY_MIN_TAIL_SAMPLES: usize = 2_000;
const PROPERTY_MIN_COVERAGE: f64 = 0.5;
const PROPERTY_MIN_CDF_GAP: f64 = 0.2;
const PROPERTY_MIN_TAIL_RATIO: f64 = 1.5;
const PROPERTY_SAMPLING_BUDGET: f64 = 0.01;
const MAX_EXAMPLE_ATTRIBUTES: usize = 64;
const MAX_EXAMPLE_ATTRIBUTE_BYTES: usize = 8 * 1024;
const MAX_REPORT_ATTRIBUTE_BYTES: usize = 16 * 1024 * 1024;
// Keep permits in blocking workers as well: dropping a timed-out HTTP future
// must not release capacity while its CPU work is still running.
static ANALYSIS_LIMIT: Semaphore = Semaphore::const_new(2);

#[derive(Deserialize)]
pub(crate) struct TailOptions {
    slow_percentile: Option<f64>,
    group_key: Option<String>,
    group_value: Option<String>,
}

struct TailParams {
    scope: SpanStatsParams,
    options: TailOptions,
}

struct Selection {
    uid: [u8; 16],
    filters: Vec<AttrFilter>,
    percentile: f64,
    group: Option<(String, String)>,
    min_ms: Option<f64>,
    max_ms: Option<f64>,
}

impl TailParams {
    fn selection(&self) -> Result<Selection, String> {
        let uid = parse_span_type_uid(self.scope.span_type_uid.as_deref())?
            .ok_or("span_type_uid is required")?;
        let percentile = self.options.slow_percentile.unwrap_or(99.0);
        if !percentile.is_finite() || !(50.0..=99.99).contains(&percentile) {
            return Err("slow_percentile must be between 50 and 99.99".into());
        }
        if self
            .scope
            .start_ns
            .zip(self.scope.end_ns)
            .is_some_and(|(a, b)| a > b)
        {
            return Err("inverted occurrence time range".into());
        }
        if self.scope.min_span_ns.is_some_and(|n| n < 0)
            || self.scope.max_span_ns.is_some_and(|n| n < 0)
            || self
                .scope
                .min_span_ns
                .zip(self.scope.max_span_ns)
                .is_some_and(|(a, b)| a > b)
        {
            return Err("invalid span duration bounds".into());
        }
        let group = match (&self.options.group_key, &self.options.group_value) {
            (None, None) => None,
            (Some(key), Some(value)) if !key.is_empty() => Some((key.clone(), value.clone())),
            _ => return Err("group_key and group_value must be supplied together".into()),
        };
        Ok(Selection {
            uid,
            filters: parse_attr_filters(&self.scope.attr)?,
            percentile,
            group,
            min_ms: self.scope.min_span_ns.map(|n| n as f64 / 1e6),
            max_ms: self.scope.max_span_ns.map(|n| n as f64 / 1e6),
        })
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Example {
    elapsed_ms: f64,
    inside_ms: Option<f64>,
    start_ns: String,
    end_ns: String,
    source_url: String,
    request_id: Option<String>,
    attributes: Option<Vec<(String, String)>>,
    attributes_omitted: Option<usize>,
    #[serde(skip)]
    source: usize,
    #[serde(skip)]
    span_uid: String,
}

struct Observation {
    example: Example,
    composition: Option<RowComposition>,
    detailed: bool,
    source: usize,
    numeric: BTreeMap<String, f64>,
    groups: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Signal {
    key: String,
    unit: Option<String>,
    typical: Option<f64>,
    slow: Option<f64>,
    typical_count: usize,
    slow_count: usize,
    typical_coverage: f64,
    slow_coverage: f64,
    separation: Option<f64>,
    correlation: Option<f64>,
    correlation_count: usize,
    duplicates_latency: bool,
    files_compared: usize,
    files_agree: usize,
    points: Vec<(f64, f64, Example)>,
    value_axis: ValueAxis,
    // Populated after the final visible field set establishes shared axes.
    density: Option<Density>,
    tail_difference: Option<NumericTailDifference>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NumericTailDifference {
    cdf_gap: f64,
    sampling_margin: f64,
    lower_bound: f64,
    comparisons: usize,
    sampling_budget: f64,
    typical_median: f64,
    slow_median: f64,
    typical_p99: f64,
    slow_p99: f64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ValueAxis {
    unit: Option<String>,
    multiplier: f64,
    minimum: f64,
    maximum: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Density {
    elapsed_edges_ms: Vec<f64>,
    value_edges: Vec<f64>,
    // Row-major: counts[value_bin * elapsed_bin_count + elapsed_bin].
    counts: Vec<usize>,
    count: usize,
    max_count: usize,
}

#[derive(Serialize)]
struct Group {
    key: String,
    values: Vec<String>,
    typical: BTreeMap<String, f64>,
    slow: BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct MixChange {
    key: String,
    value: String,
    typical: f64,
    slow: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoryLatency {
    value: String,
    count: usize,
    shares: Vec<f64>,
    bin_counts: Vec<usize>,
    median_ms: f64,
    p99_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PropertyValueComparison {
    value: String,
    count: usize,
    p99_ms: f64,
    p99_window: Option<QuantileWindow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuantileWindow {
    lower_ms: f64,
    upper_ms: f64,
    lower_rank: usize,
    upper_rank: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PropertyRelevance {
    reason: &'static str,
    values: [PropertyValueComparison; 2],
    cdf_gap: f64,
    cdf_margin: f64,
    tail_ratio_lower_bound: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoricalHistogram {
    key: String,
    count: usize,
    missing_count: usize,
    bin_edges_ms: Vec<f64>,
    max_share: f64,
    values: Vec<CategoryLatency>,
    relevance: Option<PropertyRelevance>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PropertyScreen {
    examined: usize,
    shown: usize,
    limited_coverage: usize,
    limited_observations: usize,
    similar_distributions: usize,
    pairs_compared: usize,
    minimum_samples: usize,
    minimum_tail_samples: usize,
    minimum_coverage: f64,
    minimum_cdf_gap: f64,
    minimum_tail_ratio: f64,
    sampling_budget: f64,
    interpretation: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompositionCategory {
    key: &'static str,
    share: f64,
    mean_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CohortComposition {
    count: usize,
    detailed_count: usize,
    detailed_coverage: f64,
    categories: Vec<CompositionCategory>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    count: usize,
    typical_count: usize,
    slow_count: usize,
    boundary_ms: f64,
    median_ms: f64,
    typical_elapsed_ms: Option<f64>,
    slow_elapsed_ms: Option<f64>,
    typical_inside_ms: Option<f64>,
    slow_inside_ms: Option<f64>,
    typical_outside_ms: Option<f64>,
    slow_outside_ms: Option<f64>,
    typical_attributed: usize,
    slow_attributed: usize,
    typical_composition: Option<CohortComposition>,
    slow_composition: Option<CohortComposition>,
    signals: Vec<Signal>,
    groups: Vec<Group>,
    histograms: Vec<CategoricalHistogram>,
    property_screen: PropertyScreen,
    numeric_fields_omitted: usize,
    mix_changes: Vec<MixChange>,
    examples: Vec<Example>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TailResponse {
    report: Report,
    files_used: usize,
    files_matched: usize,
    truncated: bool,
    fields_truncated: bool,
}

#[derive(Default)]
struct Collector {
    rows: Vec<Observation>,
    // Err marks incompatible producer units; these fields are excluded.
    fields: BTreeMap<String, Result<Option<String>, ()>>,
    groups: BTreeMap<String, Option<BTreeSet<String>>>,
    truncated: bool,
    fields_truncated: bool,
}

fn numeric(value: &str) -> Option<f64> {
    if value.is_empty() || value.trim() != value {
        return None;
    }
    let n: f64 = value.parse().ok()?;
    (n.is_finite() && (n.fract() != 0.0 || n.abs() <= 9_007_199_254_740_991.0)).then_some(n)
}

/// Exact cohort CDF difference with the same descriptive sampling margin as
/// property screening. Independence is assumed; this is not a p-value or cause.
/// Both cohorts must be sorted in ascending value order.
fn numeric_tail_difference(
    typical: &[f64],
    slow: &[f64],
    comparisons: usize,
) -> Option<NumericTailDifference> {
    if typical.is_empty() || slow.is_empty() {
        return None;
    }
    let log_budget = (2.0 * comparisons.max(1) as f64 / PROPERTY_SAMPLING_BUDGET).ln();
    let sampling_margin = (log_budget / (2.0 * typical.len() as f64)).sqrt()
        + (log_budget / (2.0 * slow.len() as f64)).sqrt();
    let cdf_gap = cdf_distance(typical, slow);
    let quantile = |values: &[f64], p: f64| {
        values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)]
    };
    Some(NumericTailDifference {
        cdf_gap,
        sampling_margin,
        lower_bound: (cdf_gap - sampling_margin).max(0.0),
        comparisons,
        sampling_budget: PROPERTY_SAMPLING_BUDGET,
        typical_median: quantile(typical, 0.5),
        slow_median: quantile(slow, 0.5),
        typical_p99: quantile(typical, 0.99),
        slow_p99: quantile(slow, 0.99),
    })
}

struct ExampleMetadata {
    references: usize,
    seen: bool,
    attributes: Option<Vec<(String, String)>>,
    omitted: Option<usize>,
    extra_bytes: usize,
}

type MetadataIdentities = BTreeMap<(i64, i64), BTreeMap<String, ExampleMetadata>>;

/// Stores metadata only for bounded trace examples. Original strings are
/// hydrated from source rows after analysis, never retained on every observation.
struct MetadataHydrator {
    sources: BTreeMap<usize, MetadataIdentities>,
    bytes_used: usize,
}

impl MetadataHydrator {
    fn new(report: &Report) -> Self {
        let mut sources: BTreeMap<usize, MetadataIdentities> = BTreeMap::new();
        let mut references = 0;
        for example in report.examples.iter().chain(
            report
                .signals
                .iter()
                .flat_map(|s| s.points.iter().map(|p| &p.2)),
        ) {
            let coordinates = example
                .start_ns
                .parse::<i64>()
                .ok()
                .zip(example.end_ns.parse::<i64>().ok());
            let Some(coordinates) = coordinates else {
                continue;
            };
            let entry = sources
                .entry(example.source)
                .or_default()
                .entry(coordinates)
                .or_default()
                .entry(example.span_uid.clone())
                .or_insert_with(|| ExampleMetadata {
                    references: 0,
                    seen: false,
                    attributes: None,
                    omitted: None,
                    extra_bytes: 0,
                });
            entry.references += 1;
            references += 1;
        }
        Self {
            sources,
            // Reserve the [] overhead for every emitted copy before filling.
            bytes_used: references * 2,
        }
    }

    fn record(
        &mut self,
        source: usize,
        span_uid: &str,
        start_ns: i64,
        end_ns: i64,
        attributes: &[(String, String)],
    ) -> Result<(), serde_json::Error> {
        let entry = self
            .sources
            .get_mut(&source)
            .and_then(|rows| rows.get_mut(&(start_ns, end_ns)))
            .and_then(|rows| rows.get_mut(span_uid));
        let Some(entry) = entry else { return Ok(()) };
        if entry.seen {
            // Identical coordinates are not sufficient to choose between
            // duplicate source identities, even if one was read first.
            self.bytes_used -= entry.extra_bytes * entry.references;
            entry.extra_bytes = 0;
            entry.attributes = None;
            entry.omitted = None;
            return Ok(());
        }
        entry.seen = true;
        let extra_budget = (MAX_REPORT_ATTRIBUTE_BYTES - self.bytes_used) / entry.references;
        let budget = MAX_EXAMPLE_ATTRIBUTE_BYTES.min(extra_budget + 2);
        let mut captured = Vec::new();
        let mut bytes = 2;
        for (key, value) in attributes {
            if captured.len() == MAX_EXAMPLE_ATTRIBUTES
                || key.len().saturating_add(value.len()) > budget
            {
                continue;
            }
            let pair_bytes = serde_json::to_vec(&(key, value))?.len();
            let additional = pair_bytes + usize::from(!captured.is_empty());
            if bytes + additional <= budget {
                captured.push((key.clone(), value.clone()));
                bytes += additional;
            }
        }
        entry.omitted = Some(attributes.len() - captured.len());
        entry.attributes = Some(captured);
        entry.extra_bytes = bytes - 2;
        self.bytes_used += entry.extra_bytes * entry.references;
        Ok(())
    }

    fn finish(self, report: &mut Report) {
        for example in report.examples.iter_mut().chain(
            report
                .signals
                .iter_mut()
                .flat_map(|s| s.points.iter_mut().map(|p| &mut p.2)),
        ) {
            let coordinates = example
                .start_ns
                .parse::<i64>()
                .ok()
                .zip(example.end_ns.parse::<i64>().ok());
            let entry = coordinates
                .and_then(|key| self.sources.get(&example.source)?.get(&key))
                .and_then(|rows| rows.get(&example.span_uid));
            if let Some(entry) = entry {
                example.attributes.clone_from(&entry.attributes);
                example.attributes_omitted = entry.omitted;
            }
        }
    }
}

impl Collector {
    fn push(&mut self, row: SpanStatsRow, selection: &Selection, source: usize, url: &str) {
        if row.span_type_uid != selection.uid
            || row.elapsed_ns < 0
            || !selection.filters.iter().all(|f| {
                row.attributes
                    .iter()
                    .any(|(k, v)| k == &f.key && v == &f.value)
            })
        {
            return;
        }
        if self.rows.len() == MAX_RECORDS {
            self.truncated = true;
            return;
        }
        let elapsed_ms = row.elapsed_ns as f64 / 1e6;
        let inside_ms = row
            .observed_active_wall_ns
            .filter(|n| row.details_complete == Some(true) && *n >= 0 && *n <= row.elapsed_ns)
            .map(|n| n as f64 / 1e6);
        let mut numbers = BTreeMap::new();
        let mut groups = BTreeMap::new();
        let mut units = BTreeMap::new();
        let mut conflicting_units = BTreeSet::new();
        for (key, unit) in row.attribute_units {
            if units
                .insert(key.clone(), unit.clone())
                .is_some_and(|old| old != unit)
            {
                conflicting_units.insert(key);
            }
        }
        let request_id = row
            .attributes
            .iter()
            .find(|(k, v)| k == "RequestId" && v.len() <= 512)
            .map(|(_, v)| v.clone());
        for (key, value) in row.attributes {
            if key.len() > 256 || value.len() > 512 || key.starts_with("dial9.") {
                continue;
            }
            if let Some(n) = numeric(&value) {
                if !self.fields.contains_key(&key) && self.fields.len() >= MAX_NUMERIC_CANDIDATES {
                    self.fields_truncated = true;
                    continue;
                }
                let unit = units.get(&key).cloned();
                let known = self
                    .fields
                    .entry(key.clone())
                    .or_insert_with(|| Ok(unit.clone()));
                if conflicting_units.contains(&key) || known.as_ref().is_ok_and(|old| old != &unit)
                {
                    *known = Err(());
                }
                numbers.insert(key, n);
            } else {
                if !self.groups.contains_key(&key) && self.groups.len() >= MAX_FIELDS {
                    self.fields_truncated = true;
                    continue;
                }
                let values = self
                    .groups
                    .entry(key.clone())
                    .or_insert_with(|| Some(BTreeSet::new()));
                if let Some(values) = values {
                    values.insert(value.clone());
                    if values.len() > MAX_GROUP_VALUES {
                        self.groups.insert(key, None);
                    } else {
                        groups.insert(key, value);
                    }
                }
            }
        }
        let exemplar = row
            .exemplar
            .filter(|e| !e.source_key.is_empty() && e.end_ns >= e.start_ns);
        let (start_ns, end_ns, source_url, span_uid) = match exemplar {
            Some(e) => (
                e.start_ns.to_string(),
                e.end_ns.to_string(),
                url.to_string(),
                e.span_uid,
            ),
            None => (String::new(), String::new(), String::new(), String::new()),
        };
        self.rows.push(Observation {
            example: Example {
                elapsed_ms,
                inside_ms,
                start_ns,
                end_ns,
                source_url,
                request_id,
                attributes: None,
                attributes_omitted: None,
                source,
                span_uid,
            },
            source,
            composition: row.composition,
            detailed: row.details_complete == Some(true),
            numeric: numbers,
            groups,
        });
    }

    fn report(&self, selection: &Selection) -> Result<Report, String> {
        if let Some((key, _)) = &selection.group
            && matches!(self.groups.get(key), Some(None))
        {
            return Err(format!(
                "Grouping by {key} is unavailable: more than {MAX_GROUP_VALUES} distinct values."
            ));
        }
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .filter(|r| {
                selection
                    .group
                    .as_ref()
                    .is_none_or(|(k, v)| r.groups.get(k) == Some(v))
            })
            .collect();
        rows.sort_by(|a, b| a.example.elapsed_ms.total_cmp(&b.example.elapsed_ms));
        if rows.is_empty() {
            return Err("No matching spans in this comparison.".into());
        }
        let quantile = |p: f64| {
            rows[((rows.len() as f64 * p).ceil() as usize).saturating_sub(1)]
                .example
                .elapsed_ms
        };
        let median_ms = quantile(0.5);
        // Approximate aliases must also agree in the source/group's fixed p99
        // tail, independent of the selected percentile or brushed band.
        let p99_ms = quantile(0.99);
        let tail_total = rows
            .iter()
            .filter(|row| row.example.elapsed_ms >= p99_ms)
            .count();
        let boundary_ms = selection.min_ms.unwrap_or_else(|| {
            if selection.max_ms.is_some() {
                0.0
            } else {
                quantile(selection.percentile / 100.0)
            }
        });
        let typical: Vec<_> = rows
            .iter()
            .copied()
            .filter(|r| r.example.elapsed_ms <= median_ms)
            .collect();
        let slow: Vec<_> = rows
            .iter()
            .copied()
            .filter(|r| {
                r.example.elapsed_ms >= boundary_ms
                    && selection.max_ms.is_none_or(|n| r.example.elapsed_ms <= n)
            })
            .collect();
        let mut fields = Vec::new();
        for (key, unit) in &self.fields {
            let Ok(unit) = unit else { continue };
            let multiplier = time_multiplier(unit.as_deref()).unwrap_or(1.0);
            let mut count = 0;
            let mut matches = 0;
            let mut tail_count = 0;
            let mut tail_matches = 0;
            let mut selected_count = 0;
            let mut selected_matches = 0;
            let mut minimum = f64::INFINITY;
            let mut maximum = f64::NEG_INFINITY;
            for row in &rows {
                if let Some(value) = row.numeric.get(key) {
                    count += 1;
                    let value = value * multiplier;
                    minimum = minimum.min(value);
                    maximum = maximum.max(value);
                    let matches_elapsed = (value - row.example.elapsed_ms).abs()
                        <= 1e-6_f64.max(row.example.elapsed_ms * 0.002);
                    matches += usize::from(matches_elapsed);
                    if row.example.elapsed_ms >= p99_ms {
                        tail_count += 1;
                        tail_matches += usize::from(matches_elapsed);
                    }
                    if row.example.elapsed_ms >= boundary_ms
                        && selection.max_ms.is_none_or(|n| row.example.elapsed_ms <= n)
                    {
                        selected_count += 1;
                        selected_matches += usize::from(matches_elapsed);
                    }
                }
            }
            // A true alias can have missing values. Permit ordinary jitter
            // only when p99 and selected cohorts entirely agree, each with
            // 20 recorded values or complete coverage of its nonempty cohort.
            // Rare divergences in either cohort remain visible.
            let verified = |recorded: usize, total: usize| {
                recorded > 0 && (recorded >= ALIAS_MIN_COHORT_OBSERVATIONS || recorded == total)
            };
            let repeats_elapsed = matches == count
                || (fraction(matches, count) > 0.98
                    && verified(tail_count, tail_total)
                    && tail_matches == tail_count
                    && verified(selected_count, slow.len())
                    && selected_matches == selected_count);
            if count == 0
                || minimum == maximum
                || (time_multiplier(unit.as_deref()).is_some() && repeats_elapsed)
            {
                continue;
            }
            let displayed_unit = if time_multiplier(unit.as_deref()).is_some() {
                Some("ms".to_string())
            } else {
                unit.clone()
            };
            fields.push((key, unit, multiplier, displayed_unit, minimum, maximum));
        }
        let elapsed_edges_ms = elapsed_bin_edges(&rows);
        let comparisons = fields.len();
        let mut signals = Vec::new();
        for (key, unit, multiplier, displayed_unit, minimum, maximum) in fields {
            let all: Vec<_> = rows
                .iter()
                .filter_map(|r| r.numeric.get(key).map(|n| (*r, *n)))
                .collect();
            let mut a: Vec<_> = typical
                .iter()
                .filter_map(|r| r.numeric.get(key).copied())
                .collect();
            let mut b: Vec<_> = slow
                .iter()
                .filter_map(|r| r.numeric.get(key).copied())
                .collect();
            let difference = separation(&a, &b);
            a.sort_by(f64::total_cmp);
            b.sort_by(f64::total_cmp);
            let tail_difference = numeric_tail_difference(&a, &b, comparisons);
            let mut files: BTreeMap<usize, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
            for row in &typical {
                if let Some(n) = row.numeric.get(key) {
                    files.entry(row.source).or_default().0.push(*n);
                }
            }
            for row in &slow {
                if let Some(n) = row.numeric.get(key) {
                    files.entry(row.source).or_default().1.push(*n);
                }
            }
            let differences: Vec<_> = files
                .values()
                .filter(|(a, b)| a.len() >= 5 && b.len() >= 5)
                .filter_map(|(a, b)| separation(a, b))
                .collect();
            let sampled: Vec<_> = sample_indices(all.len(), 2048)
                .into_iter()
                .map(|i| (all[i].0.example.elapsed_ms, all[i].1))
                .collect();
            let value_axis = ValueAxis {
                unit: displayed_unit,
                multiplier,
                minimum: minimum.min(0.0),
                maximum: maximum.max(0.0),
            };
            signals.push(Signal {
                key: key.clone(),
                unit: unit.clone(),
                typical: mean(a.iter().copied()),
                slow: mean(b.iter().copied()),
                typical_count: a.len(),
                slow_count: b.len(),
                typical_coverage: fraction(a.len(), typical.len()),
                slow_coverage: fraction(b.len(), slow.len()),
                separation: difference,
                correlation: spearman(&sampled),
                correlation_count: sampled.len(),
                // Retained as an additive wire-compatibility field. Aliases
                // are removed before constructing any visible signal.
                duplicates_latency: false,
                files_compared: differences.len(),
                files_agree: differences
                    .iter()
                    .filter(|d| {
                        difference.is_some_and(|overall| {
                            overall != 0.0 && **d != 0.0 && d.signum() == overall.signum()
                        })
                    })
                    .count(),
                points: Vec::new(),
                value_axis,
                density: None,
                tail_difference,
            });
        }
        let quality = |s: &Signal| {
            if s.typical_count.min(s.slow_count) < 20 {
                0
            } else {
                1
            }
        };
        signals.sort_by(|a, b| {
            quality(b)
                .cmp(&quality(a))
                .then_with(|| match (&a.tail_difference, &b.tail_difference) {
                    (Some(a), Some(b)) => b.lower_bound.total_cmp(&a.lower_bound),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.key.cmp(&b.key))
        });
        let numeric_fields_omitted = signals.len().saturating_sub(MAX_FIELDS);
        signals.truncate(MAX_FIELDS);
        let mut axes: BTreeMap<(Option<String>, String), (f64, f64)> = BTreeMap::new();
        let axis_key = |signal: &Signal| {
            // Unknown producer units cannot be assumed compatible.
            (
                signal.value_axis.unit.clone(),
                if signal.value_axis.unit.is_none() {
                    signal.key.clone()
                } else {
                    String::new()
                },
            )
        };
        for signal in &signals {
            let limits = axes.entry(axis_key(signal)).or_insert((0.0, 0.0));
            limits.0 = limits.0.min(signal.value_axis.minimum);
            limits.1 = limits.1.max(signal.value_axis.maximum);
        }
        for signal in &mut signals {
            let (minimum, maximum) = axes[&axis_key(signal)];
            signal.value_axis.minimum = minimum;
            signal.value_axis.maximum = maximum;
            let all: Vec<_> = rows
                .iter()
                .filter_map(|row| row.numeric.get(&signal.key).map(|n| (*row, *n)))
                .collect();
            let slow_indices: Vec<_> = all
                .iter()
                .enumerate()
                .filter(|(_, (r, _))| {
                    r.example.elapsed_ms >= boundary_ms
                        && selection.max_ms.is_none_or(|n| r.example.elapsed_ms <= n)
                })
                .map(|(i, _)| i)
                .collect();
            let mut plot_indices: BTreeSet<_> =
                sample_indices(all.len(), 250).into_iter().collect();
            plot_indices.extend(
                sample_indices(slow_indices.len(), 80)
                    .into_iter()
                    .map(|i| slow_indices[i]),
            );
            signal.density = Some(numeric_density(
                &all,
                &elapsed_edges_ms,
                &signal.value_axis,
                &mut plot_indices,
            ));
            signal.points = plot_indices
                .into_iter()
                .filter_map(|i| {
                    let (row, value) = all[i];
                    (!row.example.start_ns.is_empty())
                        .then(|| (row.example.elapsed_ms, value, row.example.clone()))
                })
                .collect();
        }
        let prevalence = |cohort: &[&Observation], key: &str, values: &BTreeSet<String>| {
            values
                .iter()
                .map(|v| {
                    (
                        v.clone(),
                        fraction(
                            cohort
                                .iter()
                                .filter(|r| r.groups.get(key) == Some(v))
                                .count(),
                            cohort.len(),
                        ),
                    )
                })
                .collect()
        };
        let groups: Vec<Group> = self
            .groups
            .iter()
            .filter_map(|(key, values)| {
                values.as_ref().filter(|v| v.len() > 1).map(|values| Group {
                    key: key.clone(),
                    values: values.iter().cloned().collect(),
                    typical: prevalence(&typical, key, values),
                    slow: prevalence(&slow, key, values),
                })
            })
            .collect();
        let mut mix_changes: Vec<_> = groups
            .iter()
            .flat_map(|group| {
                group.values.iter().filter_map(|value| {
                    let typical = group.typical[value];
                    let slow = group.slow[value];
                    ((slow - typical).abs() >= 0.15).then(|| MixChange {
                        key: group.key.clone(),
                        value: value.clone(),
                        typical,
                        slow,
                    })
                })
            })
            .collect();
        mix_changes.sort_by(|a, b| {
            (b.slow - b.typical)
                .abs()
                .total_cmp(&(a.slow - a.typical).abs())
        });
        mix_changes.truncate(2);
        let known = |cohort: &[&Observation]| {
            cohort
                .iter()
                .filter(|r| r.example.inside_ms.is_some())
                .count()
        };
        let (histograms, property_screen) = meaningful_histograms(&rows, &self.groups);
        Ok(Report {
            count: rows.len(),
            typical_count: typical.len(),
            slow_count: slow.len(),
            boundary_ms,
            median_ms,
            typical_elapsed_ms: mean(typical.iter().map(|r| r.example.elapsed_ms)),
            slow_elapsed_ms: mean(slow.iter().map(|r| r.example.elapsed_ms)),
            typical_inside_ms: mean(typical.iter().filter_map(|r| r.example.inside_ms)),
            slow_inside_ms: mean(slow.iter().filter_map(|r| r.example.inside_ms)),
            typical_outside_ms: mean(
                typical
                    .iter()
                    .filter_map(|r| r.example.inside_ms.map(|n| r.example.elapsed_ms - n)),
            ),
            slow_outside_ms: mean(
                slow.iter()
                    .filter_map(|r| r.example.inside_ms.map(|n| r.example.elapsed_ms - n)),
            ),
            typical_attributed: known(&typical),
            slow_attributed: known(&slow),
            typical_composition: cohort_composition(&typical)?,
            slow_composition: cohort_composition(&slow)?,
            signals,
            groups,
            histograms,
            property_screen,
            numeric_fields_omitted,
            mix_changes,
            examples: slow
                .iter()
                .rev()
                .filter(|r| !r.example.start_ns.is_empty())
                .take(8)
                .map(|r| r.example.clone())
                .collect(),
        })
    }

    fn finish(
        self,
        selection: &Selection,
        files_used: usize,
        files_matched: usize,
    ) -> Result<TailResponse, String> {
        let report = self.report(selection)?;
        // Consuming the collector releases observations before metadata hydration.
        Ok(TailResponse {
            fields_truncated: self.fields_truncated || report.numeric_fields_omitted > 0,
            truncated: self.truncated,
            report,
            files_used,
            files_matched,
        })
    }
}

fn time_multiplier(unit: Option<&str>) -> Option<f64> {
    match unit {
        Some("ns") => Some(1e-6),
        Some("us" | "µs" | "μs") => Some(1e-3),
        Some("ms") => Some(1.0),
        Some("s") => Some(1000.0),
        _ => None,
    }
}

/// Shared logarithmic elapsed bins include zero and the maximum observation.
fn elapsed_bin_edges(rows: &[&Observation]) -> Vec<f64> {
    let Some(minimum) = rows
        .iter()
        .map(|r| r.example.elapsed_ms)
        .filter(|n| *n > 0.0)
        .min_by(f64::total_cmp)
    else {
        // A nanosecond is the trace's smallest measurable elapsed duration.
        return vec![0.0, 1e-6];
    };
    // Callers sort the full scoped population, including missing field values.
    let maximum = rows
        .last()
        .expect("a positive observation requires nonempty rows")
        .example
        .elapsed_ms;
    let rounded_low = 10.0_f64.powf(minimum.log10().floor());
    let low = if rounded_low > 0.0 {
        rounded_low.min(minimum)
    } else {
        minimum
    };
    let log_low = low.ln();
    let log_span = maximum.ln() - log_low;
    let mut edges = vec![0.0, low];
    edges.extend(
        (1..HISTOGRAM_LOG_BINS)
            .map(|i| (log_low + log_span * (i as f64 / HISTOGRAM_LOG_BINS as f64)).exp())
            .filter(|edge| *edge > low && *edge < maximum),
    );
    // Keep the observed endpoint exact despite floating-point interpolation.
    edges.push(maximum);
    edges.dedup();
    edges
}

fn bin_index(edges: &[f64], value: f64) -> usize {
    edges
        .partition_point(|edge| *edge <= value)
        .saturating_sub(1)
        .min(edges.len() - 2)
}

fn numeric_density(
    observations: &[(&Observation, f64)],
    elapsed_edges_ms: &[f64],
    axis: &ValueAxis,
    plot_indices: &mut BTreeSet<usize>,
) -> Density {
    let mut value_edges: Vec<_> = (0..=DENSITY_VALUE_BINS)
        .map(|i| {
            axis.minimum + (axis.maximum - axis.minimum) * (i as f64 / DENSITY_VALUE_BINS as f64)
        })
        .collect();
    // Very small values may have fewer than 32 representable intervals.
    value_edges.dedup();
    let elapsed_bins = elapsed_edges_ms.len() - 1;
    let mut counts = vec![0; elapsed_bins * (value_edges.len() - 1)];
    let cell_index = |row: &Observation, value: f64| {
        let x = bin_index(elapsed_edges_ms, row.example.elapsed_ms);
        let y = bin_index(&value_edges, value * axis.multiplier);
        y * elapsed_bins + x
    };
    let mut represented = vec![false; counts.len()];
    for &index in plot_indices.iter() {
        let (row, value) = observations[index];
        if !row.example.start_ns.is_empty() {
            represented[cell_index(row, value)] = true;
        }
    }
    for (index, (row, value)) in observations.iter().enumerate() {
        let cell = cell_index(row, *value);
        counts[cell] += 1;
        // Count-only legacy rows remain in the density. Supplement the rank
        // samples only when a cell has an actual source span to inspect.
        if !represented[cell] && !row.example.start_ns.is_empty() {
            represented[cell] = true;
            plot_indices.insert(index);
        }
    }
    let max_count = counts.iter().copied().max().unwrap_or(0);
    Density {
        elapsed_edges_ms: elapsed_edges_ms.to_vec(),
        value_edges,
        counts,
        count: observations.len(),
        max_count,
    }
}

/// Per-value normalization preserves shape when category frequencies differ.
fn categorical_histograms(
    rows: &[&Observation],
    groups: &BTreeMap<String, Option<BTreeSet<String>>>,
) -> Vec<CategoricalHistogram> {
    let edges = elapsed_bin_edges(rows);
    let bins = edges.len() - 1;
    groups
        .iter()
        .filter_map(|(key, discovered)| {
            let discovered = discovered.as_ref()?;
            if discovered.len() < 2 {
                return None;
            }
            let mut values = Vec::new();
            let mut count = 0;
            let mut max_share = 0.0_f64;
            let mut by_value: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
            for row in rows {
                if let Some(value) = row.groups.get(key) {
                    by_value
                        .entry(value)
                        .or_default()
                        .push(row.example.elapsed_ms);
                }
            }
            for (value, latencies) in by_value {
                // rows are already sorted by elapsed time for exact quantiles.
                let mut counts = vec![0; bins];
                for latency in &latencies {
                    counts[bin_index(&edges, *latency)] += 1;
                }
                let shares: Vec<_> = counts
                    .iter()
                    .map(|n| fraction(*n, latencies.len()))
                    .collect();
                max_share = shares.iter().copied().fold(max_share, f64::max);
                let quantile = |p: f64| {
                    latencies[((latencies.len() as f64 * p).ceil() as usize).saturating_sub(1)]
                };
                count += latencies.len();
                values.push(CategoryLatency {
                    value: value.to_string(),
                    count: latencies.len(),
                    median_ms: quantile(0.5),
                    p99_ms: quantile(0.99),
                    shares,
                    bin_counts: counts,
                });
            }
            // A field constant within a selected group offers no comparison.
            (values.len() > 1).then(|| CategoricalHistogram {
                key: key.clone(),
                count,
                missing_count: rows.len() - count,
                bin_edges_ms: edges.clone(),
                max_share,
                values,
                relevance: None,
            })
        })
        .collect()
}

/// This is a descriptive screen, not a test of causation or significance.
/// Its sampling margins assume independent observations; shared load and
/// temporal dependence can still explain an association.
fn meaningful_histograms(
    rows: &[&Observation],
    groups: &BTreeMap<String, Option<BTreeSet<String>>>,
) -> (Vec<CategoricalHistogram>, PropertyScreen) {
    let candidates = categorical_histograms(rows, groups);
    let pairs_compared: usize = candidates
        .iter()
        .filter(|h| fraction(h.count, rows.len()) >= PROPERTY_MIN_COVERAGE)
        .map(|h| {
            let values = h
                .values
                .iter()
                .filter(|v| v.count >= PROPERTY_MIN_SAMPLES)
                .count();
            values * values.saturating_sub(1) / 2
        })
        .sum();
    let mut screen = PropertyScreen {
        examined: candidates.len(),
        shown: 0,
        limited_coverage: 0,
        limited_observations: 0,
        similar_distributions: 0,
        pairs_compared,
        minimum_samples: PROPERTY_MIN_SAMPLES,
        minimum_tail_samples: PROPERTY_MIN_TAIL_SAMPLES,
        minimum_coverage: PROPERTY_MIN_COVERAGE,
        minimum_cdf_gap: PROPERTY_MIN_CDF_GAP,
        minimum_tail_ratio: PROPERTY_MIN_TAIL_RATIO,
        sampling_budget: PROPERTY_SAMPLING_BUDGET,
        interpretation: "Descriptive distribution or p99 screen. Sampling windows assume independent observations; shared load, temporal dependence, missing values, and other properties can explain an association. No causal or significance claim.",
    };
    let log_budget = (2.0 * pairs_compared.max(1) as f64 / PROPERTY_SAMPLING_BUDGET).ln();
    let mut histograms = Vec::new();
    for mut histogram in candidates {
        if fraction(histogram.count, rows.len()) < PROPERTY_MIN_COVERAGE {
            screen.limited_coverage += 1;
            continue;
        }
        let adequate: Vec<_> = histogram
            .values
            .iter()
            .filter(|v| v.count >= PROPERTY_MIN_SAMPLES)
            .collect();
        if adequate.len() < 2 {
            screen.limited_observations += 1;
            continue;
        }
        // The collector's elapsed order is retained in every value's samples.
        let mut samples: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        for row in rows {
            if let Some(value) = row.groups.get(&histogram.key) {
                samples
                    .entry(value)
                    .or_default()
                    .push(row.example.elapsed_ms);
            }
        }
        let mut relevance = None;
        for (index, a) in adequate.iter().enumerate() {
            for b in &adequate[index + 1..] {
                let a_samples = &samples[a.value.as_str()];
                let b_samples = &samples[b.value.as_str()];
                let cdf_gap = cdf_distance(a_samples, b_samples);
                let cdf_margin = (log_budget / (2.0 * a.count as f64)).sqrt()
                    + (log_budget / (2.0 * b.count as f64)).sqrt();
                let a_window = (a.count >= PROPERTY_MIN_TAIL_SAMPLES)
                    .then(|| quantile_window(a_samples, 0.99, log_budget));
                let b_window = (b.count >= PROPERTY_MIN_TAIL_SAMPLES)
                    .then(|| quantile_window(b_samples, 0.99, log_budget));
                let tail_ratio = a_window.as_ref().zip(b_window.as_ref()).and_then(|(a, b)| {
                    let (lower, higher) = if a.lower_ms < b.lower_ms {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    (higher.lower_ms - lower.upper_ms >= 1e-6
                        && higher.lower_ms >= lower.upper_ms * PROPERTY_MIN_TAIL_RATIO)
                        .then(|| {
                            if lower.upper_ms > 0.0 {
                                higher.lower_ms / lower.upper_ms
                            } else {
                                // A zero baseline has no finite ratio.
                                f64::INFINITY
                            }
                        })
                });
                let reason = if tail_ratio.is_some() {
                    Some("tail")
                } else if cdf_gap >= PROPERTY_MIN_CDF_GAP + cdf_margin {
                    Some("distribution")
                } else {
                    None
                };
                let Some(reason) = reason else { continue };
                let candidate = PropertyRelevance {
                    reason,
                    values: [
                        PropertyValueComparison {
                            value: a.value.clone(),
                            count: a.count,
                            p99_ms: a.p99_ms,
                            p99_window: a_window,
                        },
                        PropertyValueComparison {
                            value: b.value.clone(),
                            count: b.count,
                            p99_ms: b.p99_ms,
                            p99_window: b_window,
                        },
                    ],
                    cdf_gap,
                    cdf_margin,
                    tail_ratio_lower_bound: tail_ratio.filter(|n| n.is_finite()),
                };
                let preferable = relevance.as_ref().is_none_or(|old: &PropertyRelevance| {
                    (candidate.reason == "tail" && old.reason != "tail")
                        || (candidate.reason == old.reason && candidate.cdf_gap > old.cdf_gap)
                });
                if preferable {
                    relevance = Some(candidate);
                }
            }
        }
        if relevance.is_some() {
            histogram.relevance = relevance;
            histograms.push(histogram);
        } else {
            screen.similar_distributions += 1;
        }
    }
    screen.shown = histograms.len();
    (histograms, screen)
}

/// Maximum gap between exact empirical CDFs; equal values are advanced together.
fn cdf_distance(a: &[f64], b: &[f64]) -> f64 {
    let (mut ai, mut bi) = (0, 0);
    let mut distance = 0.0_f64;
    while ai < a.len() || bi < b.len() {
        let value = match (a.get(ai), b.get(bi)) {
            (Some(a), Some(b)) => a.min(*b),
            (Some(a), None) => *a,
            (None, Some(b)) => *b,
            (None, None) => break,
        };
        while a.get(ai).is_some_and(|n| *n <= value) {
            ai += 1;
        }
        while b.get(bi).is_some_and(|n| *n <= value) {
            bi += 1;
        }
        distance = distance.max((fraction(ai, a.len()) - fraction(bi, b.len())).abs());
    }
    distance
}

/// Conservative order-statistic rank window, using a bounded-Bernoulli
/// concentration margin. Ranks are one-based and refer to observed values.
fn quantile_window(samples: &[f64], quantile: f64, log_budget: f64) -> QuantileWindow {
    let count = samples.len() as f64;
    let center = count * quantile;
    let radius =
        (2.0 * count * quantile * (1.0 - quantile) * log_budget).sqrt() + (2.0 / 3.0) * log_budget;
    let lower_rank = ((center - radius).floor() as usize).clamp(1, samples.len());
    let upper_rank = ((center + radius).ceil() as usize).clamp(1, samples.len());
    QuantileWindow {
        lower_ms: samples[lower_rank - 1],
        upper_ms: samples[upper_rank - 1],
        lower_rank,
        upper_rank,
    }
}

/// Same equal-per-instance shares as the catalog's five-way composition.
/// Absent composition is explicitly unknown; zero-time rows have no share.
fn cohort_composition(cohort: &[&Observation]) -> Result<Option<CohortComposition>, String> {
    if cohort.is_empty() {
        return Ok(None);
    }
    let mut means = [0.0; 5];
    let mut shares = [0.0; 5];
    let mut share_count = 0;
    let mut detailed_count = 0;
    for (index, row) in cohort.iter().enumerate() {
        let parts = match &row.composition {
            Some(c) => {
                let values = [
                    c.on_cpu_ns,
                    c.blocked_ns,
                    c.async_wait_ns,
                    c.scheduler_delay_ns,
                    c.unknown_ns,
                ];
                if values.iter().any(|n| *n < 0) {
                    return Err("negative time-composition category".into());
                }
                detailed_count += usize::from(row.detailed);
                values.map(|n| n as f64 / 1e6)
            }
            None => [0.0, 0.0, 0.0, 0.0, row.example.elapsed_ms],
        };
        for (i, value) in parts.iter().enumerate() {
            means[i] += (value - means[i]) / (index + 1) as f64;
        }
        let total: f64 = parts.iter().sum();
        if total > 0.0 {
            share_count += 1;
            for (i, value) in parts.iter().enumerate() {
                shares[i] += (value / total - shares[i]) / share_count as f64;
            }
        }
    }
    Ok(Some(CohortComposition {
        count: cohort.len(),
        detailed_count,
        detailed_coverage: fraction(detailed_count, cohort.len()),
        categories: ["on_cpu", "blocked", "async_wait", "sched_delay", "unknown"]
            .into_iter()
            .enumerate()
            .map(|(i, key)| CompositionCategory {
                key,
                share: shares[i],
                mean_ms: means[i],
            })
            .collect(),
    }))
}

fn fraction(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 / total as f64
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut count = 0;
    let mut average = 0.0;
    for n in values {
        count += 1;
        average += (n - average) / count as f64;
    }
    (count > 0).then_some(average)
}

/// Rank-biserial effect size: 2 P(slow > typical) - 1, splitting ties equally.
fn separation(typical: &[f64], slow: &[f64]) -> Option<f64> {
    if typical.is_empty() || slow.is_empty() {
        return None;
    }
    let mut sorted = typical.to_vec();
    sorted.sort_by(f64::total_cmp);
    let wins: f64 = slow
        .iter()
        .map(|n| {
            let below = sorted.partition_point(|v| v < n);
            let equal_end = sorted.partition_point(|v| v <= n);
            below as f64 + (equal_end - below) as f64 / 2.0
        })
        .sum();
    Some(2.0 * wins / (typical.len() as f64 * slow.len() as f64) - 1.0)
}

fn sample_indices(length: usize, limit: usize) -> Vec<usize> {
    let count = length.min(limit);
    (0..count).map(|i| i * length / count).collect()
}

fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<_> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut end = i + 1;
        while end < order.len() && values[order[end]] == values[order[i]] {
            end += 1;
        }
        for index in &order[i..end] {
            ranks[*index] = (i + end - 1) as f64 / 2.0;
        }
        i = end;
    }
    ranks
}

fn spearman(points: &[(f64, f64)]) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let x = ranks(&points.iter().map(|p| p.0).collect::<Vec<_>>());
    let y = ranks(&points.iter().map(|p| p.1).collect::<Vec<_>>());
    let center = (points.len() - 1) as f64 / 2.0;
    let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
    for (x, y) in x.iter().zip(&y) {
        let (x, y) = (x - center, y - center);
        xy += x * y;
        xx += x * x;
        yy += y * y;
    }
    (xx > 0.0 && yy > 0.0).then(|| xy / (xx * yy).sqrt())
}

fn internal(error: impl std::fmt::Display) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("span analysis failed: {error}"),
    )
}

pub(crate) async fn get_tail(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(scope): Query<SpanStatsParams>,
    Query(options): Query<TailOptions>,
) -> Result<Json<TailResponse>, (StatusCode, String)> {
    let params = TailParams { scope, options };
    let selection = params
        .selection()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let permit = ANALYSIS_LIMIT
        .acquire()
        .await
        .expect("analysis semaphore is never closed");
    let agg = state
        .agg_context_for(
            params.scope.bucket.as_deref(),
            params.scope.prefix.as_deref(),
            params.scope.aws_region.as_deref(),
            creds,
        )
        .await?
        .ok_or((
            StatusCode::NOT_FOUND,
            "span analysis requires aggregation".into(),
        ))?;
    let scope = aggregate::Scope {
        start_ns: params.scope.start_ns,
        end_ns: params.scope.end_ns,
        service: params.scope.service,
        hosts: params.scope.host,
    };
    let resolved = refine::resolve(
        &agg,
        &scope,
        refine::RefineOpts {
            max_files: Some(
                params
                    .scope
                    .max_files
                    .unwrap_or(MAX_FILES)
                    .clamp(1, MAX_FILES),
            ),
        },
    )
    .await
    .ok_or((
        StatusCode::NOT_FOUND,
        "no source files match this scope".into(),
    ))?;
    let files_used = resolved.capped.len();
    let files_matched = resolved.files_matched;
    let agg = std::sync::Arc::new(agg);
    let mut folds = Box::pin(refine::fold_stream(
        agg.clone(),
        state.fold_limits.clone(),
        resolved.unfolded_capped(),
    ));
    while let Some(outcome) = folds.next().await {
        if let refine::FoldOutcome::Failed { raw_key, error } = outcome {
            return Err(internal(format!("{raw_key}: {error}")));
        }
    }
    let collector = Collector::default();
    // Blocking parsing and calculation stay off Tokio workers. Move the permit
    // through each job so cancellation cannot create unbounded orphaned work.
    let mut work = (collector, selection, permit);
    for (source, (raw_key, full_key)) in resolved.capped.iter().enumerate() {
        let key = aggregate::spans_part_key_pub(&agg.output_prefix, full_key);
        let data = agg
            .output
            .get_object(&agg.output_bucket, &key)
            .await
            .map_err(internal)?;
        let url = format!(
            "/api/object?bucket={}&key={}",
            urlencoding::encode(&agg.source_bucket),
            urlencoding::encode(raw_key)
        );
        let (start, end) = (scope.start_ns, scope.end_ns);
        work = tokio::task::spawn_blocking(move || {
            let (mut collector, selection, permit) = work;
            let reader = SpansBatchReader::new(start, end, Some(selection.uid), None)
                .without_exemplar_attributes();
            let (_, result) = reader.read(data, |input| {
                if let SpanStatsInput::Row(row) = input {
                    collector.push(*row, &selection, source, &url);
                }
                Ok(())
            });
            result.map(|()| (collector, selection, permit))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
    }
    let uid = work.1.uid;
    let mut work = tokio::task::spawn_blocking(move || {
        let (collector, selection, permit) = work;
        let response = collector.finish(&selection, files_used, files_matched)?;
        let metadata = MetadataHydrator::new(&response.report);
        Ok::<_, String>((response, metadata, permit))
    })
    .await
    .map_err(internal)?
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Revisit cached rows only for sources containing selected examples. The
    // collector has been dropped, and full attributes never lived on its rows.
    for (source, (_, full_key)) in resolved.capped.iter().enumerate() {
        if !work.1.sources.contains_key(&source) {
            continue;
        }
        let key = aggregate::spans_part_key_pub(&agg.output_prefix, full_key);
        let data = agg
            .output
            .get_object(&agg.output_bucket, &key)
            .await
            .map_err(internal)?;
        let (start, end) = (scope.start_ns, scope.end_ns);
        work = tokio::task::spawn_blocking(move || {
            let (response, mut metadata, permit) = work;
            let coordinates = metadata.sources[&source].keys().copied().collect();
            let reader = SpansBatchReader::new(start, end, Some(uid), None)
                .without_exemplar_attributes()
                .with_selected_coordinates(coordinates);
            let (_, result) = reader.read(data, |input| {
                if let SpanStatsInput::Row(row) = input
                    && let Some(example) = row.exemplar
                {
                    metadata.record(
                        source,
                        &example.span_uid,
                        example.start_ns,
                        example.end_ns,
                        &row.attributes,
                    )?;
                }
                Ok(())
            });
            result.map(|()| (response, metadata, permit))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
    }
    let response = tokio::task::spawn_blocking(move || {
        let (mut response, metadata, _permit) = work;
        metadata.finish(&mut response.report);
        response
    })
    .await
    .map_err(internal)?;
    Ok(Json(response))
}

pub(crate) async fn post_tail(
    Query(scope): Query<SpanStatsParams>,
    Query(options): Query<TailOptions>,
    body: Bytes,
) -> Result<Json<TailResponse>, (StatusCode, String)> {
    let params = TailParams { scope, options };
    let selection = params
        .selection()
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let permit = ANALYSIS_LIMIT
        .acquire()
        .await
        .expect("analysis semaphore is never closed");
    let response = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let trace = raw_trace_bytes(
            &body,
            super::RAW_SPAN_STATS_BODY_LIMIT,
            super::RAW_SPAN_STATS_DECOMPRESSED_LIMIT,
        )?;
        let (_, _, _, spans) = decode::decode_samples(&trace, "raw-trace")?;
        let mut collector = Collector::default();
        for span in spans {
            if params
                .scope
                .start_ns
                .is_some_and(|n| (span.end_ns as i128) < n as i128)
                || params
                    .scope
                    .end_ns
                    .is_some_and(|n| (span.end_ns as i128) >= n as i128)
            {
                continue;
            }
            // Missing estimates contribute zero to the five-way invariant;
            // unknown_ns already carries the unclassified remainder.
            let composition = RowComposition {
                on_cpu_ns: i64::try_from(span.on_cpu_ns_est.unwrap_or(0))?,
                blocked_ns: i64::try_from(span.blocked_ns_est.unwrap_or(0))?,
                async_wait_ns: i64::try_from(span.async_wait_ns.unwrap_or(0))?,
                scheduler_delay_ns: i64::try_from(span.scheduler_delay_ns.unwrap_or(0))?,
                unknown_ns: i64::try_from(span.unknown_ns)?,
            };
            let row = SpanStatsRow {
                span_type_uid: span.span_type_uid,
                kind: span.kind,
                name: span.name,
                target: Some(span.target),
                callsite_file: span.callsite_file,
                callsite_line: span.callsite_line,
                elapsed_ns: i64::try_from(span.elapsed_ns)?,
                observed_active_wall_ns: Some(i64::try_from(span.observed_active_wall_ns)?),
                details_complete: Some(span.details_complete),
                attributes: span.attributes,
                attribute_units: span.attribute_units,
                composition: Some(composition),
                exemplar: Some(Exemplar {
                    span_uid: hex::encode(span.span_uid),
                    start_ns: i64::try_from(span.start_ns)?,
                    end_ns: i64::try_from(span.end_ns)?,
                    elapsed_ns: i64::try_from(span.elapsed_ns)?,
                    host: span.host,
                    source_key: span.source_key,
                    attributes: Vec::new(),
                    callsite_file: None,
                    callsite_line: None,
                    composition: None,
                }),
            };
            collector.push(row, &selection, 0, "");
        }
        let mut response = collector
            .finish(&selection, 1, 1)
            .map_err(anyhow::Error::msg)?;
        let mut metadata = MetadataHydrator::new(&response.report);
        let (_, _, _, spans) = decode::decode_samples(&trace, "raw-trace")?;
        for span in spans {
            if span.span_type_uid == selection.uid {
                metadata.record(
                    0,
                    &hex::encode(span.span_uid),
                    i64::try_from(span.start_ns)?,
                    i64::try_from(span.end_ns)?,
                    &span.attributes,
                )?;
            }
        }
        metadata.finish(&mut response.report);
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .map_err(internal)?
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn params(extra: &str) -> TailParams {
        let uri = format!("/api/span-tail?span_type_uid={}&{extra}", "01".repeat(16))
            .parse()
            .unwrap();
        TailParams {
            scope: Query::<SpanStatsParams>::try_from_uri(&uri).unwrap().0,
            options: Query::<TailOptions>::try_from_uri(&uri).unwrap().0,
        }
    }

    fn row(n: i64) -> SpanStatsRow {
        SpanStatsRow {
            span_type_uid: [1; 16],
            kind: "test".into(),
            name: "same-name".into(),
            target: None,
            callsite_file: None,
            callsite_line: None,
            elapsed_ns: n * 1_000_000,
            exemplar: Some(Exemplar {
                elapsed_ns: n * 1_000_000,
                span_uid: n.to_string(),
                callsite_file: None,
                callsite_line: None,
                host: "host".into(),
                start_ns: 1,
                end_ns: 1 + n * 1_000_000,
                source_key: "test".into(),
                composition: None,
                attributes: Vec::new(),
            }),
            attributes: vec![
                ("Work".into(), (n as f64 / 2.0).to_string()),
                ("ElapsedAlias".into(), n.to_string()),
                ("Constant".into(), "1".into()),
                ("Mode".into(), if n > 900 { "bulk" } else { "small" }.into()),
            ],
            attribute_units: vec![
                ("Work".into(), "ms".into()),
                ("ElapsedAlias".into(), "ms".into()),
            ],
            observed_active_wall_ns: Some(n * 100_000),
            composition: None,
            details_complete: Some(n % 2 == 0),
        }
    }

    #[rstest]
    #[case("1", Some(1.0))]
    #[case("1.5e2", Some(150.0))]
    #[case("NaN", None)]
    #[case("Infinity", None)]
    #[case(" 1", None)]
    #[case("9007199254740992", None)]
    #[case("request-123", None)]
    fn numeric_values_do_not_turn_missing_values_or_identifiers_into_numbers(
        #[case] value: &str,
        #[case] expected: Option<f64>,
    ) {
        assert_eq!(numeric(value), expected);
    }

    #[rstest]
    #[case(vec![1.0, 2.0], vec![3.0], Some(1.0))]
    #[case(vec![3.0], vec![1.0, 2.0], Some(-1.0))]
    #[case(vec![1.0, 2.0], vec![1.0, 2.0], Some(0.0))]
    #[case(vec![], vec![1.0], None)]
    fn effect_size_splits_ties_and_keeps_missing_groups_unavailable(
        #[case] typical: Vec<f64>,
        #[case] slow: Vec<f64>,
        #[case] expected: Option<f64>,
    ) {
        assert_eq!(separation(&typical, &slow), expected);
    }

    #[rstest]
    #[case(vec![], vec![1.0])]
    #[case(vec![1.0], vec![])]
    fn numeric_distribution_difference_requires_both_recorded_cohorts(
        #[case] typical: Vec<f64>,
        #[case] slow: Vec<f64>,
    ) {
        assert!(numeric_tail_difference(&typical, &slow, 1).is_none());
    }

    #[test]
    fn numeric_distribution_difference_detects_shape_without_monotonic_direction() {
        let typical = vec![0.0; 4_000];
        let mut slow = vec![-10.0; 2_000];
        slow.extend(vec![10.0; 2_000]);
        assert_eq!(separation(&typical, &slow), Some(0.0));
        let difference = numeric_tail_difference(&typical, &slow, 3).unwrap();
        assert_eq!(difference.cdf_gap, 0.5);
        assert!(difference.lower_bound > 0.4);
        assert_eq!(difference.typical_median, 0.0);
        assert_eq!(difference.slow_median, -10.0);
        assert_eq!(difference.typical_p99, 0.0);
        assert_eq!(difference.slow_p99, 10.0);
        assert_eq!(difference.comparisons, 3);
        assert_eq!(difference.sampling_budget, PROPERTY_SAMPLING_BUDGET);
    }

    #[test]
    fn numeric_sampling_allowance_accounts_for_counts_and_comparison_budget() {
        let equal = numeric_tail_difference(&[1.0, 2.0], &[1.0, 2.0], 1).unwrap();
        assert_eq!(equal.cdf_gap, 0.0);
        assert_eq!(equal.lower_bound, 0.0);
        let rare = numeric_tail_difference(&[1.0], &[2.0], 1).unwrap();
        assert_eq!(rare.cdf_gap, 1.0);
        assert_eq!(rare.lower_bound, 0.0);
        let typical = vec![1.0; 1_000];
        let slow = vec![2.0; 1_000];
        let one = numeric_tail_difference(&typical, &slow, 1).unwrap();
        let many = numeric_tail_difference(&typical, &slow, 128).unwrap();
        assert_eq!(one.cdf_gap, 1.0);
        assert!(many.sampling_margin > one.sampling_margin);
        assert!(many.lower_bound < one.lower_bound);
    }

    #[test]
    fn strong_recorded_tail_difference_ranks_ahead_of_weak_dense_field() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=10_000 {
            let mut row = row(n);
            row.attributes = vec![("Weak".into(), (n % 2).to_string())];
            row.attribute_units.clear();
            if n % 5 == 0 {
                row.attributes
                    .push(("Sparse".into(), if n >= 9_900 { "100" } else { "0" }.into()));
            }
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.signals[0].key, "Sparse");
        let sparse = &report.signals[0];
        assert_eq!((sparse.typical_count, sparse.slow_count), (1_000, 21));
        assert_eq!(sparse.typical_coverage, 0.2);
        assert!(sparse.slow_coverage < 0.21);
        let difference = sparse.tail_difference.as_ref().unwrap();
        assert_eq!(difference.cdf_gap, 1.0);
        assert_eq!(difference.comparisons, 2);
        assert_eq!(
            (difference.typical_median, difference.slow_median),
            (0.0, 100.0)
        );
        assert_eq!((difference.typical_p99, difference.slow_p99), (0.0, 100.0));
        assert!(difference.lower_bound > 0.5);
    }

    #[test]
    fn an_unavailable_comparison_is_not_ranked_as_an_observed_zero_difference() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=100 {
            let mut row = row(n);
            row.attributes = vec![("ZAvailable".into(), (n % 2).to_string())];
            row.attribute_units.clear();
            if n <= 50 {
                row.attributes
                    .push(("AUnavailable".into(), (n % 2).to_string()));
            }
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.signals[0].key, "ZAvailable");
        let available = report.signals[0].tail_difference.as_ref().unwrap();
        assert_eq!(available.cdf_gap, 0.0);
        assert_eq!(available.lower_bound, 0.0);
        assert_eq!(report.signals[1].key, "AUnavailable");
        assert!(report.signals[1].tail_difference.is_none());
        assert_eq!(report.signals[1].slow_count, 0);
    }

    #[test]
    fn numeric_cohort_summaries_use_every_recorded_value_in_original_units() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=10_000 {
            let mut row = row(n);
            row.attributes = vec![("Work".into(), (n * 500).to_string())];
            row.attribute_units = vec![("Work".into(), "us".into())];
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let signal = &report.signals[0];
        assert_eq!((signal.typical_count, signal.slow_count), (5_000, 101));
        assert_density_sample_bound(signal);
        assert_occupied_cells_have_samples(signal);
        assert_eq!(signal.correlation_count, 2_048);
        let difference = signal.tail_difference.as_ref().unwrap();
        assert_eq!(difference.typical_median, 2_500.0 * 500.0);
        assert_eq!(difference.typical_p99, 4_950.0 * 500.0);
        assert_eq!(difference.slow_median, 9_950.0 * 500.0);
        assert_eq!(difference.slow_p99, 9_999.0 * 500.0);
        assert_eq!(signal.value_axis.multiplier, 0.001);
    }

    fn metadata_report() -> Report {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        collector.push(row(1), &selection, 0, "/source");
        collector.report(&selection).unwrap()
    }

    #[test]
    fn example_metadata_preserves_raw_pairs_and_reports_empty_missing_and_ambiguous_rows() {
        let mut report = metadata_report();
        let mut metadata = MetadataHydrator::new(&report);
        let pairs = vec![
            ("Repeated".into(), "001".into()),
            ("Repeated".into(), "true".into()),
            ("Text".into(), "<tag>\n\"☃\"".into()),
        ];
        metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        metadata.finish(&mut report);
        assert_eq!(report.examples[0].attributes.as_ref(), Some(&pairs));
        assert_eq!(report.examples[0].attributes_omitted, Some(0));
        assert_eq!(report.examples[0].start_ns, "1");

        let mut empty = metadata_report();
        let mut metadata = MetadataHydrator::new(&empty);
        metadata.record(0, "1", 1, 1_000_001, &[]).unwrap();
        metadata.finish(&mut empty);
        assert_eq!(empty.examples[0].attributes, Some(Vec::new()));
        assert_eq!(empty.examples[0].attributes_omitted, Some(0));

        let mut missing = metadata_report();
        let mut metadata = MetadataHydrator::new(&missing);
        metadata.record(1, "1", 1, 1_000_001, &pairs).unwrap();
        metadata.record(0, "other", 1, 1_000_001, &pairs).unwrap();
        metadata.record(0, "1", 2, 1_000_001, &pairs).unwrap();
        metadata.finish(&mut missing);
        assert!(missing.examples[0].attributes.is_none());
        assert!(missing.examples[0].attributes_omitted.is_none());

        let mut ambiguous = metadata_report();
        let mut metadata = MetadataHydrator::new(&ambiguous);
        for _ in 0..3 {
            metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        }
        metadata.finish(&mut ambiguous);
        assert!(ambiguous.examples[0].attributes.is_none());
        assert!(ambiguous.examples[0].attributes_omitted.is_none());
    }

    #[test]
    fn example_metadata_resolves_source_identity_and_repeated_report_references() {
        let mut report = metadata_report();
        let mut second_source = report.examples[0].clone();
        second_source.source = 1;
        second_source.source_url = "/other-source".into();
        report.examples.push(second_source);
        report.examples.push(report.examples[0].clone());
        let mut metadata = MetadataHydrator::new(&report);
        let first = vec![("Source".into(), "first".into())];
        let second = vec![("Source".into(), "second".into())];
        metadata.record(1, "1", 1, 1_000_001, &second).unwrap();
        metadata.record(0, "1", 1, 1_000_001, &first).unwrap();
        metadata.finish(&mut report);
        assert_eq!(report.examples[0].attributes.as_ref(), Some(&first));
        assert_eq!(report.examples[1].attributes.as_ref(), Some(&second));
        assert_eq!(report.examples[2].attributes.as_ref(), Some(&first));
    }

    #[test]
    fn example_metadata_caps_whole_pairs_by_count_and_exact_escaped_json_bytes() {
        let mut report = metadata_report();
        let mut metadata = MetadataHydrator::new(&report);
        let pairs: Vec<_> = (0..100).map(|n| ("Key".into(), n.to_string())).collect();
        metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        metadata.finish(&mut report);
        assert_eq!(
            report.examples[0].attributes.as_ref().unwrap(),
            &pairs[..64]
        );
        assert_eq!(report.examples[0].attributes_omitted, Some(36));

        let mut report = metadata_report();
        let mut metadata = MetadataHydrator::new(&report);
        let pairs = vec![
            ("TooBig".into(), "x".repeat(MAX_EXAMPLE_ATTRIBUTE_BYTES)),
            ("Escaped".into(), "\n".repeat(4_070)),
            ("Later".into(), "kept".into()),
            ("More".into(), "\n".repeat(100)),
        ];
        metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        metadata.finish(&mut report);
        let captured = report.examples[0].attributes.as_ref().unwrap();
        assert_eq!(captured, &pairs[1..3]);
        assert_eq!(report.examples[0].attributes_omitted, Some(2));
        assert!(serde_json::to_vec(captured).unwrap().len() <= MAX_EXAMPLE_ATTRIBUTE_BYTES);
    }

    #[test]
    fn example_metadata_total_budget_counts_every_emitted_reference() {
        let mut report = metadata_report();
        let template = report.examples[0].clone();
        report.examples = vec![template.clone(); 10_000];
        let mut metadata = MetadataHydrator::new(&report);
        let pairs = vec![
            ("First".into(), "a".repeat(1_000)),
            ("Second".into(), "b".repeat(1_000)),
        ];
        metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        assert!(metadata.bytes_used <= MAX_REPORT_ATTRIBUTE_BYTES);
        metadata.finish(&mut report);
        assert!(
            report
                .examples
                .iter()
                .all(|e| e.attributes_omitted == Some(1))
        );
        let emitted_bytes: usize = report
            .examples
            .iter()
            .map(|e| {
                serde_json::to_vec(e.attributes.as_ref().unwrap())
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(emitted_bytes <= MAX_REPORT_ATTRIBUTE_BYTES);
    }

    #[test]
    fn no_included_pairs_can_still_have_recorded_fields_omitted_by_limits() {
        let mut report = metadata_report();
        let mut metadata = MetadataHydrator::new(&report);
        let pairs = vec![("TooBig".into(), "x".repeat(MAX_EXAMPLE_ATTRIBUTE_BYTES))];
        metadata.record(0, "1", 1, 1_000_001, &pairs).unwrap();
        metadata.finish(&mut report);
        assert_eq!(report.examples[0].attributes, Some(Vec::new()));
        assert_eq!(report.examples[0].attributes_omitted, Some(1));
    }

    #[rstest]
    #[case("slow_percentile=NaN")]
    #[case("slow_percentile=100")]
    #[case("min_span_ns=-1")]
    #[case("min_span_ns=5&max_span_ns=1")]
    #[case("start_ns=20&end_ns=10")]
    #[case("group_key=Mode")]
    fn invalid_comparison_parameters_are_rejected(#[case] query: &str) {
        assert!(params(query).selection().is_err());
    }

    #[test]
    fn query_retains_repeated_scope_and_attribute_filters() {
        let p = params("host=a&host=b&attr=Mode=bulk&start_ns=10&end_ns=20&slow_percentile=95");
        assert_eq!(p.scope.host, ["a", "b"]);
        assert_eq!(p.scope.start_ns, Some(10));
        assert_eq!(p.selection().unwrap().filters.len(), 1);
    }

    #[test]
    fn report_ranks_fields_and_uses_only_complete_interval_timing() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=1000 {
            collector.push(row(n), &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(
            (report.count, report.typical_count, report.slow_count),
            (1000, 500, 11)
        );
        assert_eq!(report.boundary_ms, 990.0);
        assert_eq!(report.typical_elapsed_ms, Some(250.5));
        assert_eq!(report.slow_elapsed_ms, Some(995.0));
        assert_eq!(report.typical_attributed, 250);
        assert_eq!(report.slow_attributed, 6);
        assert_eq!(report.signals.len(), 1);
        assert_eq!(report.signals[0].key, "Work");
        assert_eq!(report.signals[0].correlation, Some(1.0));
        assert!(report.signals.iter().all(|s| !s.duplicates_latency));
        report.signals.iter().for_each(assert_density_sample_bound);
        assert_eq!(report.examples.len(), 8);
        let group = params("group_key=Mode&group_value=bulk")
            .selection()
            .unwrap();
        let grouped = collector.report(&group).unwrap();
        assert_eq!(grouped.count, 100);
        assert_eq!(grouped.boundary_ms, 999.0);
        assert_eq!(grouped.slow_count, 2);
    }

    fn category<'a>(composition: &'a CohortComposition, key: &str) -> &'a CompositionCategory {
        composition
            .categories
            .iter()
            .find(|c| c.key == key)
            .unwrap()
    }

    fn tagged_row(n: i64, value: Option<&str>) -> SpanStatsRow {
        let mut row = row(n);
        row.attributes = value
            .map(|v| vec![("Mode".into(), v.into())])
            .unwrap_or_default();
        row
    }

    #[rstest]
    #[case(MAX_GROUP_VALUES)]
    #[case(MAX_GROUP_VALUES + 1)]
    fn selected_groups_require_complete_retained_values(#[case] distinct: usize) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for index in 0..distinct {
            collector.push(
                tagged_row(index as i64 + 1, Some(&format!("value-{index}"))),
                &selection,
                0,
                "",
            );
        }
        collector.push(tagged_row(100, Some("value-0")), &selection, 0, "");
        let grouped = params("group_key=Mode&group_value=value-0")
            .selection()
            .unwrap();
        if distinct > MAX_GROUP_VALUES {
            assert_eq!(
                collector.report(&grouped).map(|report| report.count),
                Err(format!(
                    "Grouping by Mode is unavailable: more than {MAX_GROUP_VALUES} distinct values."
                ))
            );
        } else {
            let report = collector.report(&grouped).unwrap();
            assert_eq!(report.count, 2);
            assert_eq!(report.boundary_ms, 100.0);
        }
        let all = collector.report(&selection).unwrap();
        assert_eq!(all.count, distinct + 1);
        assert_eq!(
            all.groups
                .iter()
                .find(|group| group.key == "Mode")
                .map(|group| group.values.len()),
            (distinct <= MAX_GROUP_VALUES).then_some(distinct)
        );
    }

    fn unfiltered_histograms(
        collector: &Collector,
        selection: &Selection,
    ) -> Vec<CategoricalHistogram> {
        let mut rows: Vec<_> = collector
            .rows
            .iter()
            .filter(|r| {
                selection
                    .group
                    .as_ref()
                    .is_none_or(|(key, value)| r.groups.get(key) == Some(value))
            })
            .collect();
        rows.sort_by(|a, b| a.example.elapsed_ms.total_cmp(&b.example.elapsed_ms));
        categorical_histograms(&rows, &collector.groups)
    }

    #[test]
    fn categorical_histograms_normalize_unequal_groups_and_include_the_last_edge() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (value, repetitions) in [("large", 100), ("small", 1)] {
            for _ in 0..repetitions {
                for latency in [1, 10, 100] {
                    collector.push(tagged_row(latency, Some(value)), &selection, 0, "");
                }
            }
        }
        let histograms = unfiltered_histograms(&collector, &selection);
        let histogram = &histograms[0];
        assert_eq!(histogram.count, 303);
        assert_eq!(histogram.missing_count, 0);
        assert_eq!(histogram.values[0].count, 300);
        assert_eq!(histogram.values[1].count, 3);
        assert_eq!(histogram.values[0].shares, histogram.values[1].shares);
        for value in &histogram.values {
            assert_eq!(value.bin_counts.iter().sum::<usize>(), value.count);
            assert!((value.shares.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            assert_eq!(value.median_ms, 10.0);
            assert_eq!(value.p99_ms, 100.0);
            assert_eq!(value.shares.last(), Some(&(1.0 / 3.0)));
        }
    }

    #[test]
    fn categorical_histograms_distinguish_missing_zero_and_slow_values() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (n, value) in [
            (0, Some("fast")),
            (1, Some("fast")),
            (100, Some("slow")),
            (500, None),
        ] {
            collector.push(tagged_row(n, value), &selection, 0, "");
        }
        let histograms = unfiltered_histograms(&collector, &selection);
        let histogram = &histograms[0];
        assert_eq!((histogram.count, histogram.missing_count), (3, 1));
        assert_eq!(histogram.bin_edges_ms[0], 0.0);
        assert_eq!(histogram.values[0].shares[0], 0.5);
        assert_eq!(histogram.values[0].p99_ms, 1.0);
        assert_eq!(histogram.values[1].p99_ms, 100.0);
        assert_eq!(
            histogram.values[0]
                .shares
                .iter()
                .zip(&histogram.values[1].shares)
                .filter(|(a, b)| **a > 0.0 && **b > 0.0)
                .count(),
            0
        );
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(100_000_000)]
    #[case(100_000_001)]
    #[case(i64::MAX)]
    fn categorical_bins_remain_finite_and_ordered_for_constant_or_extreme_latency(
        #[case] elapsed_ns: i64,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for value in ["a", "b"] {
            let mut row = tagged_row(0, Some(value));
            row.elapsed_ns = elapsed_ns;
            collector.push(row, &selection, 0, "");
        }
        let histograms = unfiltered_histograms(&collector, &selection);
        let histogram = &histograms[0];
        let maximum = if elapsed_ns == 0 {
            1e-6
        } else {
            elapsed_ns as f64 / 1e6
        };
        assert_eq!(histogram.bin_edges_ms.last(), Some(&maximum));
        assert!(
            histogram
                .bin_edges_ms
                .windows(2)
                .all(|w| w[0].is_finite() && w[1] > w[0])
        );
        assert!(
            histogram
                .values
                .iter()
                .all(|v| v.shares.iter().sum::<f64>() == 1.0)
        );
        assert!(
            histogram
                .values
                .iter()
                .all(|v| v.bin_counts.last() == Some(&v.count))
        );
    }

    #[rstest]
    #[case(vec![100.0, 100.000001])]
    #[case(vec![f64::from_bits(1), f64::from_bits(2)])]
    #[case(vec![1e-300, 1e300])]
    fn elapsed_bins_end_at_the_exact_maximum_without_degenerate_edges(
        #[case] latencies_ms: Vec<f64>,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (index, elapsed_ms) in latencies_ms.into_iter().enumerate() {
            collector.push(row(index as i64), &selection, 0, "");
            collector.rows.last_mut().unwrap().example.elapsed_ms = elapsed_ms;
        }
        let rows: Vec<_> = collector.rows.iter().collect();
        let edges = elapsed_bin_edges(&rows);
        assert_eq!(edges.first(), Some(&0.0));
        assert_eq!(edges.last(), Some(&rows.last().unwrap().example.elapsed_ms));
        assert!(
            edges
                .windows(2)
                .all(|w| w[0].is_finite() && w[1].is_finite() && w[1] > w[0])
        );
    }

    #[test]
    fn categorical_histograms_follow_group_filters_and_skip_unbounded_fields() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 0..26 {
            let mut row = tagged_row(n, Some(if n % 2 == 0 { "a" } else { "b" }));
            row.attributes.extend([
                ("Region".into(), if n < 13 { "east" } else { "west" }.into()),
                ("Identifier".into(), format!("id-{n}")),
                ("Constant".into(), "same".into()),
            ]);
            collector.push(row, &selection, 0, "");
        }
        let all = unfiltered_histograms(&collector, &selection);
        assert_eq!(all.len(), 2);
        let selected = params("group_key=Region&group_value=east")
            .selection()
            .unwrap();
        let grouped = unfiltered_histograms(&collector, &selected);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].key, "Mode");
        assert_eq!(grouped[0].count, 13);
        assert_eq!(grouped[0].values[0].count, 7);
        assert_eq!(grouped[0].values[1].count, 6);
    }

    #[test]
    fn density_counts_all_observations_instead_of_the_latency_rank_sample() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 0..10_000 {
            let mut row = row(if n < 9_000 { 1 } else { 100 });
            row.attributes = vec![("Work".into(), if n < 9_000 { "2" } else { "10" }.into())];
            // Some observations have no navigable exemplar, but still count.
            if n % 2 == 0 {
                row.exemplar = None;
            }
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let signal = &report.signals[0];
        assert_density_sample_bound(signal);
        assert_eq!(signal.density.as_ref().unwrap().count, 10_000);
        assert_eq!(
            signal
                .density
                .as_ref()
                .unwrap()
                .counts
                .iter()
                .sum::<usize>(),
            10_000
        );
        assert_eq!(signal.density.as_ref().unwrap().max_count, 9_000);
        assert_eq!(signal.density.as_ref().unwrap().elapsed_edges_ms[0], 0.0);
        assert_eq!(
            signal.density.as_ref().unwrap().counts.len(),
            (signal.density.as_ref().unwrap().elapsed_edges_ms.len() - 1)
                * (signal.density.as_ref().unwrap().value_edges.len() - 1)
        );
        let x = bin_index(&signal.density.as_ref().unwrap().elapsed_edges_ms, 1.0);
        let y = bin_index(&signal.density.as_ref().unwrap().value_edges, 2.0);
        assert_eq!(
            signal.density.as_ref().unwrap().counts
                [y * (signal.density.as_ref().unwrap().elapsed_edges_ms.len() - 1) + x],
            9_000
        );
    }

    fn assert_density_sample_bound(signal: &Signal) {
        let density = signal.density.as_ref().unwrap();
        let occupied_cells = density.counts.iter().filter(|count| **count > 0).count();
        assert!(signal.points.len() <= 330 + occupied_cells);
        assert!(signal.points.len() <= density.count);
    }

    fn assert_occupied_cells_have_samples(signal: &Signal) {
        assert_density_sample_bound(signal);
        let density = signal.density.as_ref().unwrap();
        let elapsed_bins = density.elapsed_edges_ms.len() - 1;
        let mut sampled = BTreeSet::new();
        for (elapsed, value, example) in &signal.points {
            assert!(!example.start_ns.is_empty());
            assert!(!example.span_uid.is_empty());
            let x = bin_index(&density.elapsed_edges_ms, *elapsed);
            let y = bin_index(&density.value_edges, value * signal.value_axis.multiplier);
            sampled.insert(y * elapsed_bins + x);
        }
        for (cell, count) in density.counts.iter().enumerate() {
            if *count > 0 {
                assert!(sampled.contains(&cell), "occupied cell {cell} has no span");
            }
        }
    }

    fn density_row(index: usize, elapsed_ns: i64, value: f64, unit: &str) -> SpanStatsRow {
        let mut row = row(1);
        row.elapsed_ns = elapsed_ns;
        row.attributes = vec![("Work".into(), value.to_string())];
        row.attribute_units = vec![("Work".into(), unit.into())];
        let example = row.exemplar.as_mut().unwrap();
        example.span_uid = index.to_string();
        example.start_ns = index as i64 * 10_000_000_000 + 1;
        example.end_ns = example.start_ns + elapsed_ns;
        example.elapsed_ns = elapsed_ns;
        row
    }

    #[test]
    fn occupied_density_cell_retains_a_span_missed_by_both_rank_samples() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for index in 0..4096 {
            let value = if index == 2 { 99.0 } else { 2.0 };
            collector.push(
                density_row(index, 10_000_000, value, "bytes"),
                &selection,
                0,
                "/source",
            );
        }
        let report = collector.report(&selection).unwrap();
        assert_occupied_cells_have_samples(&report.signals[0]);
        let (_, value, example) = report.signals[0]
            .points
            .iter()
            .find(|(_, _, example)| example.span_uid == "2")
            .unwrap();
        assert_eq!(*value, 99.0);
        assert_eq!(example.start_ns, "20000000001");
        assert_eq!(report.signals[0].density.as_ref().unwrap().count, 4096);
        assert_eq!(report.slow_count, 4096);
        assert_eq!(report.median_ms, 10.0);
        let mut expected: BTreeSet<_> = sample_indices(4096, 250).into_iter().collect();
        expected.extend(sample_indices(4096, 80));
        assert!(expected.insert(2));
        assert_eq!(
            report.signals[0]
                .points
                .iter()
                .map(|(_, _, example)| example.span_uid.parse::<usize>().unwrap())
                .collect::<Vec<_>>(),
            expected.into_iter().collect::<Vec<_>>()
        );
    }

    #[rstest]
    #[case("", 2368)]
    #[case("min_span_ns=100000000&max_span_ns=1000000000", 2368)]
    #[case("group_key=Mode&group_value=chosen", 1184)]
    #[case("group_key=Mode&group_value=chosen&min_span_ns=100000000", 1184)]
    fn occupied_density_cells_exceed_the_old_point_limit_without_losing_scope(
        #[case] query: &str,
        #[case] expected_count: usize,
    ) {
        let selection = params(query).selection().unwrap();
        let mut collector = Collector::default();
        // One row in every positive elapsed/value combination, repeated in
        // another group. This occupies more cells than either old rank sample.
        for elapsed_bin in 0..37 {
            let elapsed_ns = if elapsed_bin == 0 {
                0
            } else {
                (1e6 * 10.0_f64.powf((elapsed_bin as f64 - 0.5) / 12.0)).round() as i64
            };
            for value_bin in 0..32 {
                for group in ["chosen", "other"] {
                    let index = collector.rows.len();
                    let mut row = density_row(index, elapsed_ns, value_bin as f64 + 0.5, "bytes");
                    row.attributes.push(("Mode".into(), group.into()));
                    // Retain explicit extrema so the full shared value range
                    // keeps all 32 interior values in different cells.
                    if value_bin == 0 {
                        row.attributes[0].1 = "0".into();
                    } else if value_bin == 31 {
                        row.attributes[0].1 = "32".into();
                    }
                    collector.push(row, &selection, 0, "/source");
                }
            }
        }
        let report = collector.report(&selection).unwrap();
        let signal = &report.signals[0];
        let density = signal.density.as_ref().unwrap();
        assert_eq!(report.count, expected_count);
        assert_eq!(density.count, expected_count);
        assert_eq!(density.counts.iter().sum::<usize>(), expected_count);
        assert!(density.counts.iter().filter(|count| **count > 0).count() > 330);
        assert_occupied_cells_have_samples(signal);
        assert!(signal.points.windows(2).all(|pair| {
            pair[0].0 < pair[1].0
                || (pair[0].0 == pair[1].0
                    && pair[0].2.span_uid.parse::<usize>().unwrap()
                        < pair[1].2.span_uid.parse::<usize>().unwrap())
        }));
        if selection.group.is_some() {
            assert!(
                signal
                    .points
                    .iter()
                    .all(|(_, _, example)| { example.span_uid.parse::<usize>().unwrap() % 2 == 0 })
            );
        }
        assert_eq!(
            serde_json::to_value(&report.signals).unwrap(),
            serde_json::to_value(collector.report(&selection).unwrap().signals).unwrap()
        );
    }

    #[rstest]
    #[case(0)]
    #[case(10_000_000)]
    fn density_cell_sampling_handles_zero_or_constant_elapsed_and_missing_values(
        #[case] elapsed_ns: i64,
    ) {
        let selection = params("min_span_ns=0&max_span_ns=0").selection().unwrap();
        let mut collector = Collector::default();
        for index in 0..4096 {
            let mut row = density_row(
                index,
                elapsed_ns,
                if index == 2 { 32.0 } else { 1.0 },
                "bytes",
            );
            if index % 2 == 1 {
                row.attributes.clear();
            }
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let signal = &report.signals[0];
        assert_occupied_cells_have_samples(signal);
        assert_eq!(signal.density.as_ref().unwrap().count, 2048);
        assert_eq!(report.count, 4096);
        assert_eq!(report.slow_count, if elapsed_ns == 0 { 4096 } else { 0 });
        assert!(
            signal
                .points
                .iter()
                .all(|(_, _, example)| { example.span_uid.parse::<usize>().unwrap() % 2 == 0 })
        );
    }

    #[test]
    fn density_cell_sampling_uses_final_shared_axes_and_original_time_units() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for index in 0..4096 {
            let value_ns = if index == 2 { 9_900_000.0 } else { 100_000.0 };
            let mut row = density_row(index, 20_000_000, value_ns, "ns");
            row.attributes.extend([
                (
                    "Other".into(),
                    if index % 2 == 0 { "0.001" } else { "0.1" }.into(),
                ),
                (
                    "Duration".into(),
                    if index == 3 { "0.09" } else { "0.001" }.into(),
                ),
            ]);
            row.attribute_units.extend([
                ("Other".into(), "s".into()),
                ("Duration".into(), "s".into()),
            ]);
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.signals.len(), 3);
        for signal in &report.signals {
            assert_eq!(signal.value_axis.unit.as_deref(), Some("ms"));
            assert_eq!(signal.value_axis.maximum, 100.0);
            assert_occupied_cells_have_samples(signal);
        }
        let work = report
            .signals
            .iter()
            .find(|signal| signal.key == "Work")
            .unwrap();
        assert!(
            work.points
                .iter()
                .any(|(_, value, example)| { *value == 9_900_000.0 && example.span_uid == "2" })
        );
        let duration = report
            .signals
            .iter()
            .find(|signal| signal.key == "Duration")
            .unwrap();
        assert!(
            duration
                .points
                .iter()
                .any(|(_, value, example)| { *value == 0.09 && example.span_uid == "3" })
        );
    }

    #[test]
    fn occupied_cell_prefers_a_real_source_row_and_keeps_count_only_cells_honest() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for index in 0..4096 {
            let value = match index {
                2 | 3 => 16.0,
                5 => 32.0,
                _ => 1.0,
            };
            let mut row = density_row(index, 10_000_000, value, "bytes");
            if index == 2 || index == 5 {
                row.exemplar = None;
            }
            collector.push(row, &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let signal = &report.signals[0];
        assert!(
            signal
                .points
                .iter()
                .any(|(_, value, example)| *value == 16.0 && example.span_uid == "3")
        );
        assert!(
            signal
                .points
                .iter()
                .all(|(_, value, example)| *value != 32.0 && !example.start_ns.is_empty())
        );
        let density = signal.density.as_ref().unwrap();
        assert_eq!(density.count, 4096);
        assert_eq!(density.counts.iter().filter(|count| **count > 0).count(), 3);
    }

    #[rstest]
    #[case("ns", 1e-6)]
    #[case("s", 1000.0)]
    #[case("bytes", 1.0)]
    fn density_samples_and_counts_share_internal_and_inclusive_final_boundaries(
        #[case] unit: &str,
        #[case] multiplier: f64,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (index, (elapsed, value)) in [(0, 0.0), (1, 1.0), (2, 2.0), (2, 0.0)]
            .into_iter()
            .enumerate()
        {
            collector.push(
                density_row(index, elapsed * 1_000_000, value / multiplier, unit),
                &selection,
                0,
                "/source",
            );
        }
        let all: Vec<_> = collector
            .rows
            .iter()
            .map(|row| (row, row.numeric["Work"]))
            .collect();
        let axis = ValueAxis {
            unit: Some("normalized".into()),
            multiplier,
            minimum: 0.0,
            maximum: 2.0,
        };
        let mut indices = BTreeSet::from([0, 2]);
        let density = numeric_density(&all, &[0.0, 1.0, 2.0], &axis, &mut indices);
        assert_eq!(indices, BTreeSet::from([0, 1, 2, 3]));
        assert_eq!(density.counts[0], 1); // (0, 0)
        assert_eq!(density.counts[1], 1); // (inclusive final X, 0)
        assert_eq!(density.counts[16 * 2 + 1], 1); // both internal edges
        assert_eq!(density.counts[31 * 2 + 1], 1); // both inclusive final edges
        assert_eq!(density.counts.iter().sum::<usize>(), 4);
    }

    #[test]
    fn density_respects_group_scope_and_missing_numeric_values() {
        let selection = params("group_key=Mode&group_value=a").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=300 {
            let mut row = row(n);
            row.attributes = vec![("Mode".into(), if n <= 100 { "a" } else { "b" }.into())];
            if n % 2 == 0 {
                row.attributes.push(("Work".into(), (n / 2).to_string()));
            }
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.count, 100);
        assert_eq!(report.signals[0].density.as_ref().unwrap().count, 50);
        assert_eq!(
            report.signals[0]
                .density
                .as_ref()
                .unwrap()
                .counts
                .iter()
                .sum::<usize>(),
            50
        );
        assert!(
            report.signals[0]
                .density
                .as_ref()
                .unwrap()
                .elapsed_edges_ms
                .last()
                .unwrap()
                >= &100.0
        );
    }

    #[rstest]
    #[case("", 6, 1_000_000_001)]
    #[case("group_key=Mode&group_value=a", 5, 100_000_001)]
    #[case("min_span_ns=50000000&max_span_ns=50000000", 6, 1_000_000_001)]
    #[case(
        "group_key=Mode&group_value=a&min_span_ns=50000000&max_span_ns=50000000",
        5,
        100_000_001
    )]
    fn numeric_elapsed_bins_cover_every_scoped_observation(
        #[case] query: &str,
        #[case] expected_count: usize,
        #[case] maximum_ns: i64,
    ) {
        let selection = params(query).selection().unwrap();
        let mut collector = Collector::default();
        for (index, elapsed_ns) in [
            0,
            1_000_000,
            50_000_000,
            100_000_000,
            100_000_001,
            1_000_000_001,
        ]
        .into_iter()
        .enumerate()
        {
            let mut row = row(0);
            row.elapsed_ns = elapsed_ns;
            row.attributes = vec![
                ("Mode".into(), if index < 5 { "a" } else { "b" }.into()),
                ("StepMs".into(), ((index + 1) as f64 / 100.0).to_string()),
            ];
            row.attribute_units = vec![("StepMs".into(), "ms".into())];
            if index < 4 {
                row.attributes
                    .push(("StepNs".into(), ((index + 1) * 7_000).to_string()));
                row.attribute_units.push(("StepNs".into(), "ns".into()));
            } else {
                // The full elapsed range must survive missing fields/exemplars.
                row.exemplar = None;
            }
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.count, expected_count);
        assert_eq!(report.signals.len(), 2);
        let edges = &report.signals[0].density.as_ref().unwrap().elapsed_edges_ms;
        assert_eq!(edges.last(), Some(&(maximum_ns as f64 / 1e6)));
        let axis = &report.signals[0].value_axis;
        for signal in &report.signals {
            let density = signal.density.as_ref().unwrap();
            assert_eq!(&density.elapsed_edges_ms, edges);
            assert_eq!(density.counts.iter().sum::<usize>(), density.count);
            assert_eq!(
                density.count,
                if signal.key == "StepNs" {
                    4
                } else {
                    expected_count
                }
            );
            assert_eq!(signal.value_axis.minimum, axis.minimum);
            assert_eq!(signal.value_axis.maximum, axis.maximum);
            assert_eq!(signal.value_axis.unit.as_deref(), Some("ms"));
            if signal.key == "StepMs" {
                let x_bins = density.elapsed_edges_ms.len() - 1;
                let y = bin_index(&density.value_edges, expected_count as f64 / 100.0);
                assert_eq!(density.counts[y * x_bins + x_bins - 1], 1);
            }
        }
    }

    #[test]
    fn known_units_share_normalized_axes_and_unknown_units_remain_independent() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=20 {
            let mut row = row(n * 100);
            row.attributes = vec![
                ("A".into(), (n * 1_000_000).to_string()),
                ("B".into(), (n * 2).to_string()),
                ("C".into(), (n * 3).to_string()),
                ("D".into(), (n * 10).to_string()),
                ("UnknownA".into(), n.to_string()),
                ("UnknownB".into(), (n * 4).to_string()),
                ("ElapsedAlias".into(), (n * 100).to_string()),
                ("Constant".into(), "1000000".into()),
            ];
            row.attribute_units = vec![
                ("A".into(), "ns".into()),
                ("B".into(), "ms".into()),
                ("C".into(), "bytes".into()),
                ("D".into(), "bytes".into()),
                ("ElapsedAlias".into(), "ms".into()),
                ("Constant".into(), "ms".into()),
            ];
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        let signal = |key: &str| report.signals.iter().find(|s| s.key == key).unwrap();
        let a = signal("A");
        let b = signal("B");
        assert_eq!(a.value_axis.unit.as_deref(), Some("ms"));
        assert_eq!(a.value_axis.multiplier, 1e-6);
        assert_eq!(b.value_axis.multiplier, 1.0);
        assert_eq!(a.value_axis.maximum, 40.0);
        assert_eq!(
            a.density.as_ref().unwrap().value_edges,
            b.density.as_ref().unwrap().value_edges
        );
        assert_eq!(signal("C").value_axis.maximum, 200.0);
        assert_eq!(
            signal("C").density.as_ref().unwrap().value_edges,
            signal("D").density.as_ref().unwrap().value_edges
        );
        assert_eq!(signal("UnknownA").value_axis.maximum, 20.0);
        assert_eq!(signal("UnknownB").value_axis.maximum, 80.0);
        assert_eq!(signal("UnknownA").value_axis.unit, None);
        assert!(report.signals.iter().all(|s| s.key != "ElapsedAlias"));
        assert!(report.signals.iter().all(|s| s.key != "Constant"));
    }

    #[test]
    fn elapsed_aliases_do_not_consume_the_visible_field_capacity() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=100 {
            let mut row = row(n);
            row.attributes.clear();
            row.attribute_units.clear();
            for index in 0..MAX_FIELDS {
                let key = format!("Alias{index:02}");
                row.attributes.push((key.clone(), n.to_string()));
                row.attribute_units.push((key, "ms".into()));
            }
            for index in 0..MAX_FIELDS {
                let key = format!("Work{index:02}");
                row.attributes
                    .push((key.clone(), (n * (index as i64 + 1)).to_string()));
                row.attribute_units.push((key, "bytes".into()));
            }
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.signals.len(), MAX_FIELDS);
        assert!(report.signals.iter().all(|s| s.key.starts_with("Work")));
        assert_eq!(report.numeric_fields_omitted, 0);
        assert!(!collector.fields_truncated);
    }

    #[test]
    fn rare_tail_divergence_is_not_mistaken_for_an_elapsed_alias() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=1_000 {
            let mut row = row(n);
            // This timer repeats elapsed time in the ordinary 99% but exposes
            // different work in the slowest 1%. Overall agreement hides it.
            row.attributes.push((
                "TailTimer".into(),
                if n <= 990 { n } else { n * 4 }.to_string(),
            ));
            row.attribute_units.push(("TailTimer".into(), "ms".into()));
            row.attributes
                .push(("UnknownElapsed".into(), n.to_string()));
            if n % 37 == 0 {
                row.attributes.retain(|(key, _)| key != "ElapsedAlias");
            }
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        let timer = report
            .signals
            .iter()
            .find(|s| s.key == "TailTimer")
            .expect("a meaningful rare-tail divergence must remain visible");
        assert_eq!(timer.slow_count, 11);
        assert_eq!(timer.density.as_ref().unwrap().count, 1_000);
        assert!(report.signals.iter().all(|s| s.key != "ElapsedAlias"));
        let unknown = report
            .signals
            .iter()
            .find(|s| s.key == "UnknownElapsed")
            .unwrap();
        assert_eq!(unknown.unit, None);
        assert_eq!(unknown.value_axis.unit, None);
        assert_eq!(unknown.value_axis.maximum, 1_000.0);
    }

    fn jittered_timer_rows(selection: &Selection, count: i64, jitter_count: i64) -> Collector {
        let mut collector = Collector::default();
        for n in 1..=count {
            let mut row = row(0);
            row.elapsed_ns = 100_000 + n * 100;
            let elapsed_ms = row.elapsed_ns as f64 / 1e6;
            let difference_ms = if n <= jitter_count { 0.005 } else { 0.0002 };
            row.attributes = vec![
                (
                    "JitteredTimer".into(),
                    (elapsed_ms - difference_ms).to_string(),
                ),
                ("ExactTimer".into(), elapsed_ms.to_string()),
                ("UnknownElapsed".into(), elapsed_ms.to_string()),
            ];
            row.attribute_units = vec![
                ("JitteredTimer".into(), "ms".into()),
                ("ExactTimer".into(), "ms".into()),
            ];
            if n % 37 == 0 {
                row.attributes.retain(|(key, _)| key != "ExactTimer");
            }
            collector.push(row, selection, 0, "");
        }
        collector
    }

    #[rstest]
    #[case("", 10_000, 20, false)]
    #[case("slow_percentile=95", 10_000, 20, false)]
    // The selected band contains ordinary jitter.
    #[case("min_span_ns=100000&max_span_ns=105000", 10_000, 20, true)]
    // A completely recorded five-value selected cohort has no missing evidence.
    #[case("min_span_ns=1099600", 10_000, 20, false)]
    // The completely recorded p99 tail has only 11 values.
    #[case("slow_percentile=50", 1_000, 3, false)]
    // An empty selected cohort cannot confirm approximate redundancy.
    #[case("min_span_ns=2000000", 10_000, 20, true)]
    fn ordinary_jitter_is_hidden_only_with_verified_tail_and_selected_cohorts(
        #[case] query: &str,
        #[case] count: i64,
        #[case] jitter_count: i64,
        #[case] visible: bool,
    ) {
        let selection = params(query).selection().unwrap();
        let collector = jittered_timer_rows(&selection, count, jitter_count);
        let report = collector.report(&selection).unwrap();
        assert_eq!(
            report.signals.iter().any(|s| s.key == "JitteredTimer"),
            visible,
        );
        // Missing values do not invalidate a true alias's all-match path.
        assert!(report.signals.iter().all(|s| s.key != "ExactTimer"));
        let unknown = report
            .signals
            .iter()
            .find(|s| s.key == "UnknownElapsed")
            .unwrap();
        assert_eq!(unknown.value_axis.unit, None);
    }

    #[test]
    fn a_verified_brush_does_not_hide_divergence_in_the_fixed_global_p99_tail() {
        let selection = params("min_span_ns=110000&max_span_ns=130000")
            .selection()
            .unwrap();
        let mut collector = jittered_timer_rows(&selection, 10_000, 0);
        for (index, row) in collector.rows.iter_mut().enumerate() {
            if index >= 9_900 {
                row.numeric
                    .insert("JitteredTimer".into(), row.example.elapsed_ms * 0.5);
            }
        }
        let report = collector.report(&selection).unwrap();
        assert!(report.slow_count >= ALIAS_MIN_COHORT_OBSERVATIONS);
        assert!(report.signals.iter().any(|s| s.key == "JitteredTimer"));
    }

    #[test]
    fn missing_tail_values_do_not_confirm_an_approximate_elapsed_alias() {
        let selection = params("slow_percentile=95").selection().unwrap();
        let mut collector = jittered_timer_rows(&selection, 10_000, 20);
        for (index, row) in collector.rows.iter_mut().enumerate() {
            if index >= 9_899 {
                row.numeric.remove("JitteredTimer");
            }
        }
        let report = collector.report(&selection).unwrap();
        let timer = report
            .signals
            .iter()
            .find(|s| s.key == "JitteredTimer")
            .unwrap();
        assert!(timer.slow_count >= ALIAS_MIN_COHORT_OBSERVATIONS);
        assert_eq!(timer.density.as_ref().unwrap().count, 9_899);
    }

    #[rstest]
    #[case("slow_percentile=50", 1_000, 3)]
    #[case("min_span_ns=1099600", 10_000, 20)]
    fn incomplete_small_cohorts_do_not_confirm_an_approximate_elapsed_alias(
        #[case] query: &str,
        #[case] count: i64,
        #[case] jitter_count: i64,
    ) {
        let selection = params(query).selection().unwrap();
        let mut collector = jittered_timer_rows(&selection, count, jitter_count);
        let complete = collector.report(&selection).unwrap();
        assert!(complete.signals.iter().all(|s| s.key != "JitteredTimer"));
        // One missing value makes either the 11-value p99 tail or five-value
        // selected cohort incomplete. Both remain below the 20-value guard.
        collector
            .rows
            .last_mut()
            .unwrap()
            .numeric
            .remove("JitteredTimer");
        let incomplete = collector.report(&selection).unwrap();
        assert!(incomplete.signals.iter().any(|s| s.key == "JitteredTimer"));
        assert!(incomplete.signals.iter().all(|s| s.key != "ExactTimer"));
    }

    #[test]
    fn candidate_and_visible_field_limits_are_explicit() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=2 {
            let mut row = row(n);
            row.attributes = (0..=MAX_NUMERIC_CANDIDATES)
                .map(|index| (format!("Value{index:03}"), n.to_string()))
                .collect();
            row.attribute_units.clear();
            collector.push(row, &selection, 0, "");
        }
        let response = collector.finish(&selection, 2, 3).unwrap();
        assert!(response.fields_truncated);
        assert_eq!(response.files_used, 2);
        assert_eq!(response.files_matched, 3);
        let report = response.report;
        assert_eq!(report.signals.len(), MAX_FIELDS);
        assert_eq!(
            report.numeric_fields_omitted,
            MAX_NUMERIC_CANDIDATES - MAX_FIELDS
        );
    }

    #[test]
    fn omitted_fields_and_unknown_units_do_not_distort_visible_time_axes() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=100 {
            let mut row = row(n * 1_000);
            row.attributes.clear();
            row.attribute_units.clear();
            for index in 0..MAX_FIELDS {
                let key = format!("Work{index:02}");
                row.attributes.push((key.clone(), n.to_string()));
                row.attribute_units.push((key, "ms".into()));
            }
            row.attributes
                .push(("ZZHuge".into(), (n * 1_000_000).to_string()));
            row.attribute_units.push(("ZZHuge".into(), "ms".into()));
            row.attributes
                .push(("UnknownElapsed".into(), (n * 1_000).to_string()));
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        // Exact numeric equality without a declared unit is not a time alias.
        let unknown = report
            .signals
            .iter()
            .find(|s| s.key == "UnknownElapsed")
            .unwrap();
        assert_eq!(unknown.value_axis.unit, None);
        assert_eq!(unknown.value_axis.maximum, 100_000.0);
        assert!(report.signals.iter().all(|s| s.key != "ZZHuge"));
        assert!(
            report
                .signals
                .iter()
                .filter(|s| s.unit.as_deref() == Some("ms"))
                .all(|s| s.value_axis.maximum == 100.0)
        );
        assert!(report.signals.iter().all(|s| s.density.is_some()));
    }

    #[rstest]
    #[case("ns", 1e-6)]
    #[case("us", 1e-3)]
    #[case("µs", 1e-3)]
    #[case("μs", 1e-3)]
    #[case("ms", 1.0)]
    #[case("s", 1e3)]
    fn time_unit_aliases_are_excluded_without_hiding_real_submicrosecond_work(
        #[case] unit: &str,
        #[case] multiplier: f64,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for elapsed_ns in [100, 500, 1000] {
            let mut row = row(0);
            row.elapsed_ns = elapsed_ns;
            let elapsed_ms = elapsed_ns as f64 / 1e6;
            row.attributes = vec![
                ("Alias".into(), (elapsed_ms / multiplier).to_string()),
                ("Work".into(), (elapsed_ms * 0.5 / multiplier).to_string()),
            ];
            row.attribute_units = vec![("Alias".into(), unit.into()), ("Work".into(), unit.into())];
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.signals.len(), 1);
        assert_eq!(report.signals[0].key, "Work");
    }

    #[rstest]
    #[case(vec![1.0, 1.0], vec![1.0, 1.0], 0.0)]
    #[case(vec![1.0, 2.0], vec![3.0, 4.0], 1.0)]
    #[case(vec![1.0, 2.0], vec![1.0, 3.0], 0.5)]
    fn exact_cdf_distance_handles_ties_and_shape_changes(
        #[case] a: Vec<f64>,
        #[case] b: Vec<f64>,
        #[case] expected: f64,
    ) {
        assert_eq!(cdf_distance(&a, &b), expected);
    }

    #[test]
    fn property_screen_hides_identical_distributions_even_with_unequal_volume() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (value, repetitions) in [("common", 1_000), ("less-common", 100)] {
            for _ in 0..repetitions {
                for latency in [1, 10, 100] {
                    collector.push(tagged_row(latency, Some(value)), &selection, 0, "");
                }
            }
        }
        let report = collector.report(&selection).unwrap();
        assert!(report.histograms.is_empty());
        assert_eq!(report.property_screen.similar_distributions, 1);
        assert_eq!(report.property_screen.pairs_compared, 1);
    }

    #[test]
    fn property_screen_keeps_adequately_observed_broad_distribution_changes() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (value, latency) in [("fast", 1), ("slow", 100)] {
            for _ in 0..500 {
                collector.push(tagged_row(latency, Some(value)), &selection, 0, "");
            }
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.histograms.len(), 1);
        let relevance = report.histograms[0].relevance.as_ref().unwrap();
        assert_eq!(relevance.reason, "distribution");
        assert_eq!(relevance.cdf_gap, 1.0);
        assert!(relevance.cdf_margin > 0.0);
        assert!(
            relevance
                .values
                .iter()
                .all(|v| v.count == 500 && v.p99_window.is_none())
        );
        assert_eq!(report.property_screen.shown, 1);
    }

    #[test]
    fn property_screen_keeps_p99_changes_when_the_distribution_body_is_identical() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (value, tail_latency) in [("fast-tail", 10), ("slow-tail", 100)] {
            for index in 0..20_000 {
                let latency = if index < 19_600 { 1 } else { tail_latency };
                collector.push(tagged_row(latency, Some(value)), &selection, 0, "");
            }
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.histograms.len(), 1);
        assert_eq!(report.histograms[0].values[0].median_ms, 1.0);
        assert_eq!(report.histograms[0].values[1].median_ms, 1.0);
        let relevance = report.histograms[0].relevance.as_ref().unwrap();
        assert_eq!(relevance.reason, "tail");
        assert!(relevance.cdf_gap < PROPERTY_MIN_CDF_GAP);
        assert_eq!(relevance.tail_ratio_lower_bound, Some(10.0));
        assert_eq!(
            relevance.values[0].p99_window.as_ref().unwrap().upper_ms,
            10.0
        );
        assert_eq!(
            relevance.values[1].p99_window.as_ref().unwrap().lower_ms,
            100.0
        );
        assert!(
            relevance
                .values
                .iter()
                .all(|v| v.p99_window.as_ref().unwrap().lower_rank
                    < v.p99_window.as_ref().unwrap().upper_rank)
        );
    }

    #[rstest]
    #[case(20, 10_000, 0, "limitedObservations")]
    #[case(200, 200, 1_200, "limitedCoverage")]
    #[case(500, 500, 0, "similarDistributions")]
    fn rare_missing_and_underobserved_tails_do_not_pass_the_property_screen(
        #[case] fast_count: usize,
        #[case] slow_count: usize,
        #[case] missing: usize,
        #[case] expected_reason: &str,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (value, count, tail_latency) in [("fast", fast_count, 10), ("slow", slow_count, 100)] {
            for index in 0..count {
                let latency = if index < count * 98 / 100 {
                    1
                } else {
                    tail_latency
                };
                collector.push(tagged_row(latency, Some(value)), &selection, 0, "");
            }
        }
        for _ in 0..missing {
            collector.push(tagged_row(1, None), &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert!(report.histograms.is_empty());
        let screen = serde_json::to_value(report.property_screen).unwrap();
        assert_eq!(screen[expected_reason], 1);
        assert!(
            screen["interpretation"]
                .as_str()
                .unwrap()
                .contains("independent")
        );
    }

    #[test]
    fn property_sampling_margin_accounts_for_all_compared_pairs() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for n in 0..2_000 {
            let mut row = tagged_row(
                if n < 1_000 { 1 } else { 100 },
                Some(if n < 1_000 { "a" } else { "b" }),
            );
            row.attributes
                .push(("Other".into(), if n < 1_000 { "x" } else { "y" }.into()));
            collector.push(row, &selection, 0, "");
        }
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.property_screen.pairs_compared, 2);
        let margin = report.histograms[0].relevance.as_ref().unwrap().cdf_margin;
        assert!(margin > 2.0 * ((2.0 / PROPERTY_SAMPLING_BUDGET).ln() / 2_000.0).sqrt());
        let selected = params("group_key=Other&group_value=x").selection().unwrap();
        let report = collector.report(&selected).unwrap();
        assert!(report.histograms.is_empty());
        assert_eq!(report.property_screen.examined, 0);
    }

    fn composed_row(n: i64, parts: [i64; 5]) -> SpanStatsRow {
        let mut row = row(n);
        row.details_complete = Some(true);
        row.composition = Some(RowComposition {
            on_cpu_ns: parts[0],
            blocked_ns: parts[1],
            async_wait_ns: parts[2],
            scheduler_delay_ns: parts[3],
            unknown_ns: parts[4],
        });
        row
    }

    #[test]
    fn paired_composition_weights_instances_equally_and_follows_exact_cohorts() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        for (n, parts) in [
            (1, [1_000_000, 0, 0, 0, 0]),
            (9, [0, 0, 9_000_000, 0, 0]),
            (100, [0, 100_000_000, 0, 0, 0]),
            (1000, [0, 0, 0, 500_000_000, 500_000_000]),
        ] {
            collector.push(composed_row(n, parts), &selection, 0, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let typical = report.typical_composition.unwrap();
        assert_eq!(typical.count, 2);
        assert_eq!(category(&typical, "on_cpu").share, 0.5);
        assert_eq!(category(&typical, "async_wait").share, 0.5);
        assert_eq!(category(&typical, "on_cpu").mean_ms, 0.5);
        assert_eq!(category(&typical, "async_wait").mean_ms, 4.5);
        let slow = report.slow_composition.unwrap();
        assert_eq!(category(&slow, "sched_delay").share, 0.5);
        assert_eq!(category(&slow, "unknown").mean_ms, 500.0);

        let band = params("min_span_ns=9000000&max_span_ns=100000000")
            .selection()
            .unwrap();
        let report = collector.report(&band).unwrap();
        let selected = report.slow_composition.unwrap();
        assert_eq!(selected.count, 2);
        assert_eq!(category(&selected, "async_wait").share, 0.5);
        assert_eq!(category(&selected, "blocked").share, 0.5);
        assert_eq!(category(&selected, "blocked").mean_ms, 50.0);

        let group = params("group_key=Mode&group_value=small")
            .selection()
            .unwrap();
        let report = collector.report(&group).unwrap();
        assert_eq!(report.slow_composition.unwrap().categories[1].share, 1.0);
    }

    #[test]
    fn missing_composition_stays_unknown_instead_of_disappearing_from_the_bar() {
        let selection = params("min_span_ns=1000000&max_span_ns=3000000")
            .selection()
            .unwrap();
        let mut collector = Collector::default();
        collector.push(composed_row(1, [1_000_000, 0, 0, 0, 0]), &selection, 0, "");
        collector.push(row(3), &selection, 0, "");
        let selected = collector
            .report(&selection)
            .unwrap()
            .slow_composition
            .unwrap();
        assert_eq!(selected.detailed_count, 1);
        assert_eq!(selected.detailed_coverage, 0.5);
        assert_eq!(category(&selected, "on_cpu").share, 0.5);
        assert_eq!(category(&selected, "unknown").share, 0.5);
        assert_eq!(category(&selected, "unknown").mean_ms, 1.5);
        assert!(cohort_composition(&[]).unwrap().is_none());
    }

    #[rstest]
    #[case([0, 0, 0, 0, 0], false)]
    #[case([-1, 0, 0, 0, 0], true)]
    #[case([0, -1, 0, 0, 0], true)]
    #[case([0, 0, -1, 0, 0], true)]
    #[case([0, 0, 0, -1, 0], true)]
    #[case([0, 0, 0, 0, -1], true)]
    fn zero_time_and_invalid_composition_do_not_produce_plausible_shares(
        #[case] parts: [i64; 5],
        #[case] invalid: bool,
    ) {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        collector.push(composed_row(0, parts), &selection, 0, "");
        let report = collector.report(&selection);
        if invalid {
            assert!(report.err().unwrap().contains("negative time-composition"));
        } else {
            let composition = report.unwrap().typical_composition.unwrap();
            assert!(
                composition
                    .categories
                    .iter()
                    .all(|c| c.share == 0.0 && c.mean_ms == 0.0)
            );
        }
    }

    #[test]
    fn exact_type_filter_separates_same_names_and_conflicting_units_are_excluded() {
        let selection = params("").selection().unwrap();
        let mut collector = Collector::default();
        collector.push(row(1), &selection, 0, "/source");
        let mut foreign = row(100);
        foreign.span_type_uid = [2; 16];
        collector.push(foreign, &selection, 0, "/source");
        let mut mismatch = row(2);
        mismatch.attribute_units[0].1 = "bytes".into();
        collector.push(mismatch, &selection, 0, "/source");
        let report = collector.report(&selection).unwrap();
        assert_eq!(report.count, 2);
        assert!(report.signals.iter().all(|s| s.key != "Work"));
    }

    #[rstest]
    #[case(vec![(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)], Some(1.0))]
    #[case(vec![(1.0, 3.0), (2.0, 2.0), (3.0, 1.0)], Some(-1.0))]
    #[case(vec![(1.0, 2.0), (2.0, 2.0), (3.0, 2.0)], None)]
    fn spearman_keeps_constant_fields_unavailable(
        #[case] points: Vec<(f64, f64)>,
        #[case] expected: Option<f64>,
    ) {
        assert_eq!(spearman(&points), expected);
    }

    #[test]
    fn opposite_file_directions_do_not_support_a_pooled_zero_effect() {
        let selection = params("min_span_ns=101000000").selection().unwrap();
        let mut collector = Collector::default();
        for n in 1..=200 {
            let source = usize::from((51..=100).contains(&n) || n > 150);
            let mut row = row(n);
            row.attributes[0].1 = if source == 0 { n } else { -n }.to_string();
            collector.push(row, &selection, source, "/source");
        }
        let report = collector.report(&selection).unwrap();
        let field = report.signals.iter().find(|s| s.key == "Work").unwrap();
        assert_eq!(field.separation, Some(0.0));
        assert_eq!(field.files_compared, 2);
        assert_eq!(field.files_agree, 0);
    }

    #[test]
    fn producer_units_survive_parquet_and_raw_analysis_uses_same_type_identity() {
        let (_, _, _, mut spans) = decode::decode_samples(
            &raw_trace_bytes(
                include_bytes!("../../../ui/public/demo-trace.bin"),
                super::super::RAW_SPAN_STATS_BODY_LIMIT,
                super::super::RAW_SPAN_STATS_DECOMPRESSED_LIMIT,
            )
            .unwrap(),
            "fixture",
        )
        .unwrap();
        let mut span = spans.pop().unwrap();
        span.attributes = vec![("Payload".into(), "42".into())];
        span.attribute_units = vec![("Payload".into(), "bytes".into())];
        let uid = span.span_type_uid;
        let mut data = Vec::new();
        crate::ingest::parquet_writer::write_spans(&mut data, &[span]).unwrap();
        let reader = SpansBatchReader::new(None, None, Some(uid), None);
        let mut seen = 0;
        let (_, result) = reader.read(data, |input| {
            if let SpanStatsInput::Row(row) = input {
                seen += 1;
                assert_eq!(row.attribute_units, [("Payload".into(), "bytes".into())]);
            }
            Ok(())
        });
        result.unwrap();
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn synthetic_raw_and_parquet_reports_have_identical_density_and_property_screens() {
        use dial9_core::schema_extensions::{self, roles};
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
        use dial9_trace_format::types::{FieldType, FieldValue};

        let schema = Schema::from_entry(SchemaEntry::with_annotations(
            "producer:LatencyFixture",
            vec![
                FieldDef::new("started", FieldType::Varint),
                FieldDef::new("operation", FieldType::String),
                FieldDef::new("Work", FieldType::Varint),
                FieldDef::new("ElapsedAlias", FieldType::Varint),
                FieldDef::new("Mode", FieldType::String),
                FieldDef::new("Unrelated", FieldType::String),
            ],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(0, schema_extensions::SPAN_TYPE_KEY, "latency-fixture"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_NAME),
                FieldAnnotation::new(2, "unit", "ms"),
                FieldAnnotation::new(3, "unit", "ms"),
            ],
        ));
        let mut encoder = Encoder::new();
        let mut started = 1;
        for index in 0..2_000 {
            let latency = if index < 1_000 { 10 } else { 1_000 };
            let ended = started + latency * 1_000_000;
            encoder
                .write_event(
                    &schema,
                    ended,
                    &[
                        FieldValue::Varint(started),
                        FieldValue::String("fixture-operation".into()),
                        FieldValue::Varint(if index == 1 { 750 } else { latency / 2 }),
                        FieldValue::Varint(latency),
                        FieldValue::String(if index < 1_000 { "fast" } else { "slow" }.into()),
                        FieldValue::String(if index % 2 == 0 { "a" } else { "b" }.into()),
                    ],
                )
                .unwrap();
            started = ended + 1;
        }
        let trace = encoder.into_inner();
        let (_, _, _, spans) = decode::decode_samples(&trace, "raw-trace").unwrap();
        assert_eq!(spans.len(), 2_000);
        let uid = spans[0].span_type_uid;
        let uri = format!(
            "/api/span-tail?span_type_uid={}&min_span_ns=1000000000&max_span_ns=1000000000",
            hex::encode(uid)
        )
        .parse()
        .unwrap();
        let response = post_tail(
            Query::<SpanStatsParams>::try_from_uri(&uri).unwrap(),
            Query::<TailOptions>::try_from_uri(&uri).unwrap(),
            Bytes::from(trace),
        )
        .await
        .unwrap()
        .0;
        let selection = TailParams {
            scope: Query::<SpanStatsParams>::try_from_uri(&uri).unwrap().0,
            options: Query::<TailOptions>::try_from_uri(&uri).unwrap().0,
        }
        .selection()
        .unwrap();
        let mut parquet = Vec::new();
        crate::ingest::parquet_writer::write_spans(&mut parquet, &spans).unwrap();
        let mut collector = Collector::default();
        let (_, result) = SpansBatchReader::new(None, None, Some(uid), None)
            .without_exemplar_attributes()
            .read(parquet.clone(), |input| {
                if let SpanStatsInput::Row(row) = input {
                    collector.push(*row, &selection, 0, "");
                }
                Ok(())
            });
        result.unwrap();
        let mut folded = collector.finish(&selection, 1, 1).unwrap();
        let mut metadata = MetadataHydrator::new(&folded.report);
        let coordinates = metadata.sources[&0].keys().copied().collect();
        let (_, result) = SpansBatchReader::new(None, None, Some(uid), None)
            .without_exemplar_attributes()
            .with_selected_coordinates(coordinates)
            .read(parquet, |input| {
                if let SpanStatsInput::Row(row) = input
                    && let Some(example) = row.exemplar
                {
                    metadata.record(
                        0,
                        &example.span_uid,
                        example.start_ns,
                        example.end_ns,
                        &row.attributes,
                    )?;
                }
                Ok(())
            });
        result.unwrap();
        metadata.finish(&mut folded.report);
        assert_eq!(response.files_used, folded.files_used);
        assert_eq!(response.files_matched, folded.files_matched);
        assert_eq!(response.truncated, folded.truncated);
        assert_eq!(response.fields_truncated, folded.fields_truncated);
        assert_eq!(response.report.signals.len(), 1);
        assert_eq!(response.report.signals[0].key, "Work");
        assert_occupied_cells_have_samples(&response.report.signals[0]);
        assert!(
            response.report.signals[0]
                .points
                .iter()
                .any(|(_, value, example)| {
                    *value == 750.0
                        && example
                            .attributes
                            .as_ref()
                            .is_some_and(|pairs| pairs.contains(&("Work".into(), "750".into())))
                })
        );
        assert!(
            response.report.signals[0]
                .points
                .iter()
                .all(|(_, _, example)| {
                    example.attributes_omitted == Some(0)
                        && example.attributes.as_ref().is_some_and(|pairs| {
                            pairs.contains(&("operation".into(), "fixture-operation".into()))
                                && pairs.iter().any(|(key, _)| key == "ElapsedAlias")
                        })
                })
        );
        assert_eq!(
            response.report.signals[0].density.as_ref().unwrap().count,
            2_000
        );
        assert_eq!(response.report.histograms.len(), 1);
        assert_eq!(response.report.histograms[0].key, "Mode");
        assert_eq!(response.report.property_screen.similar_distributions, 1);
        assert_eq!(
            serde_json::to_value(response.report.signals).unwrap(),
            serde_json::to_value(folded.report.signals).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(response.report.histograms).unwrap(),
            serde_json::to_value(folded.report.histograms).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(response.report.property_screen).unwrap(),
            serde_json::to_value(folded.report.property_screen).unwrap(),
        );
    }

    #[tokio::test]
    async fn cached_spans_without_units_are_refolded_before_tail_comparison() {
        use std::sync::Arc;

        use axum::extract::FromRequestParts;
        use dial9_core::schema_extensions::{self, roles};
        use dial9_trace_format::encoder::{Encoder, Schema};
        use dial9_trace_format::schema::{FieldAnnotation, FieldDef, SchemaEntry};
        use dial9_trace_format::types::{FieldType, FieldValue};
        use parquet::arrow::ArrowWriter;
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        use crate::ingest::parquet_writer;
        use crate::storage::{LocalBackend, StorageBackend};

        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = Arc::new(LocalBackend::new(source_dir.path()));
        let output = Arc::new(LocalBackend::new(output_dir.path()));
        let schema = Schema::from_entry(SchemaEntry::with_annotations(
            "producer:RequestFixture",
            vec![
                FieldDef::new("started", FieldType::Varint),
                FieldDef::new("operation", FieldType::String),
                FieldDef::new("RequestBodyBytes", FieldType::Varint),
                FieldDef::new("Time", FieldType::Varint),
            ],
            [
                FieldAnnotation::new(0, schema_extensions::ROLE_KEY, roles::SPAN_START),
                FieldAnnotation::new(0, "unit", "ns"),
                FieldAnnotation::new(0, schema_extensions::SPAN_TYPE_KEY, "request-fixture"),
                FieldAnnotation::new(1, schema_extensions::ROLE_KEY, roles::SPAN_NAME),
                FieldAnnotation::new(2, "unit", "bytes"),
                FieldAnnotation::new(3, "unit", "ms"),
            ],
        ));
        let mut encoder = Encoder::new();
        let mut started = 1;
        for latency in 1..=64 {
            let ended = started + latency * 1_000_000;
            encoder
                .write_event(
                    &schema,
                    ended,
                    &[
                        FieldValue::Varint(started),
                        FieldValue::String("request".into()),
                        FieldValue::Varint(latency * 100),
                        FieldValue::Varint(latency),
                    ],
                )
                .unwrap();
            started = ended + 1;
        }
        let trace = encoder.into_inner();
        let mut uid = None;
        for index in 0..2 {
            let key = format!(
                "traces/version=1/date=2026-10-16/service=fixture/time=2100/instance=host/boot=test/1792184400-{index}.bin"
            );
            source
                .put_object("local", &key, trace.clone())
                .await
                .unwrap();
            let (_, _, _, spans) = decode::decode_samples(&trace, &key).unwrap();
            uid = Some(spans[0].span_type_uid);
            let mut spans_data = Vec::new();
            parquet_writer::write_spans(&mut spans_data, &spans).unwrap();
            if index == 0 {
                // v10 parts written before unit persistence lack this column.
                let batch = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(spans_data))
                    .unwrap()
                    .build()
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap();
                let columns = (0..batch.num_columns())
                    .filter(|&column| batch.schema().field(column).name() != "attribute_units")
                    .collect::<Vec<_>>();
                let batch = batch.project(&columns).unwrap();
                spans_data = Vec::new();
                let mut writer =
                    ArrowWriter::try_new(&mut spans_data, batch.schema(), None).unwrap();
                writer.write(&batch).unwrap();
                writer.close().unwrap();
            }
            let mut samples_data = Vec::new();
            parquet_writer::write_samples(&mut samples_data, &[], &spans, &Default::default())
                .unwrap();
            let leaf = aggregate::part_leaf_of(&key);
            for (table, data) in [("spans", spans_data), ("samples", samples_data)] {
                let cached_key = format!(
                    "flamegraph-data/v10/bucket=local/{table}/service=fixture/date=2026-10-16/host=host/{leaf}.parquet"
                );
                output.put_object("local", &cached_key, data).await.unwrap();
            }
        }
        let state = AppState::from_local_dir(source_dir.path()).with_agg(aggregate::AggContext {
            source,
            output,
            source_bucket: "local".into(),
            source_is_local: true,
            output_bucket: "local".into(),
            output_prefix: "flamegraph-data".into(),
            source_prefixes: vec!["traces/".into()],
            segment_duration_secs: 60,
        });
        let uri = format!("/api/span-tail?span_type_uid={}", hex::encode(uid.unwrap()))
            .parse()
            .unwrap();
        let (mut parts, ()) = axum::http::Request::new(()).into_parts();
        let creds = MaybeCreds::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        let response = get_tail(
            State(state),
            creds,
            Query::<SpanStatsParams>::try_from_uri(&uri).unwrap(),
            Query::<TailOptions>::try_from_uri(&uri).unwrap(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response.report.count, 128);
        let signal = response
            .report
            .signals
            .iter()
            .find(|signal| signal.key == "RequestBodyBytes")
            .expect("RequestBodyBytes must survive old cached parts without units");
        assert_eq!(signal.unit.as_deref(), Some("bytes"));
        assert_eq!(signal.density.as_ref().unwrap().count, 128);
        assert!(
            response
                .report
                .signals
                .iter()
                .all(|signal| signal.key != "Time")
        );
    }

    #[tokio::test]
    async fn raw_endpoint_decodes_the_trace_on_the_server() {
        let trace = include_bytes!("../../../ui/public/demo-trace.bin");
        let (_, _, _, spans) = decode::decode_samples(
            &raw_trace_bytes(
                trace,
                super::super::RAW_SPAN_STATS_BODY_LIMIT,
                super::super::RAW_SPAN_STATS_DECOMPRESSED_LIMIT,
            )
            .unwrap(),
            "fixture",
        )
        .unwrap();
        let uid = spans[0].span_type_uid;
        let expected = spans.iter().filter(|s| s.span_type_uid == uid).count();
        let uri = format!("/api/span-tail?span_type_uid={}", hex::encode(uid))
            .parse()
            .unwrap();
        let response = post_tail(
            Query::<SpanStatsParams>::try_from_uri(&uri).unwrap(),
            Query::<TailOptions>::try_from_uri(&uri).unwrap(),
            Bytes::from_static(trace),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response.report.count, expected);
        assert_eq!(response.files_used, 1);
        let selection = TailParams {
            scope: Query::<SpanStatsParams>::try_from_uri(&uri).unwrap().0,
            options: Query::<TailOptions>::try_from_uri(&uri).unwrap().0,
        }
        .selection()
        .unwrap();
        let mut data = Vec::new();
        crate::ingest::parquet_writer::write_spans(&mut data, &spans).unwrap();
        let mut collector = Collector::default();
        let (_, result) = SpansBatchReader::new(None, None, Some(uid), None).read(data, |input| {
            if let SpanStatsInput::Row(row) = input {
                collector.push(*row, &selection, 0, "");
            }
            Ok(())
        });
        result.unwrap();
        let folded = collector.report(&selection).unwrap();
        assert_eq!(
            serde_json::to_value(response.report.typical_composition).unwrap(),
            serde_json::to_value(folded.typical_composition).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(response.report.slow_composition).unwrap(),
            serde_json::to_value(folded.slow_composition).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(response.report.histograms).unwrap(),
            serde_json::to_value(folded.histograms).unwrap(),
        );
    }
}
