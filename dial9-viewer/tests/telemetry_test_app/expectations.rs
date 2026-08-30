use anyhow::{Context as _, Result, bail, ensure};
use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::{collections::BTreeSet, num::NonZeroU64};

const EXPECTATION_EVENT: &str = "TelemetryFixtureExpectationEvent";
const MARKER_EVENT: &str = "TelemetryFixtureMarkerEvent";
const FIXTURE_PREFIX: &str = "dial9_fixture_";
const WEIGHT_SEPARATOR: &str = "_weight_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixtureFeature {
    Cpu,
    TaskDump,
    Span,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FunctionSymbol(String);

impl FunctionSymbol {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SpanName(String);

impl SpanName {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExpectedSymbol {
    pub(crate) feature: FixtureFeature,
    pub(crate) symbol: FunctionSymbol,
    pub(crate) name: String,
    pub(crate) weight: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExpectedEdge<T> {
    pub(crate) parent: T,
    pub(crate) child: T,
}

pub(crate) type ExpectedStackEdge = ExpectedEdge<FunctionSymbol>;
pub(crate) type ExpectedSpanEdge = ExpectedEdge<SpanName>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExpectedSpanAssociation {
    pub(crate) symbol: FunctionSymbol,
    pub(crate) active_span: SpanName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MeasurementWindow {
    pub(crate) start_ns: u64,
    pub(crate) end_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedModel {
    pub(crate) symbols: BTreeSet<ExpectedSymbol>,
    pub(crate) spans: BTreeSet<SpanName>,
    pub(crate) stack_edges: BTreeSet<ExpectedStackEdge>,
    pub(crate) span_edges: BTreeSet<ExpectedSpanEdge>,
    pub(crate) span_associations: BTreeSet<ExpectedSpanAssociation>,
    pub(crate) measurement: MeasurementWindow,
}

#[derive(Debug, Deserialize)]
struct ExpectationEvent {
    timestamp_ns: u64,
    feature: FixtureFeature,
    name: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    active_span: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarkerEvent {
    timestamp_ns: u64,
    phase: MarkerPhase,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MarkerPhase {
    MeasurementStart,
    MeasurementEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolDomain {
    Cpu,
    Wait,
}

struct ParsedWeightedSymbol {
    domain: SymbolDomain,
    name: String,
    weight: NonZeroU64,
}

#[derive(Default)]
struct ModelBuilder {
    symbols: BTreeSet<ExpectedSymbol>,
    symbol_identities: BTreeSet<(FixtureFeature, String)>,
    spans: BTreeSet<SpanName>,
    stack_edges: BTreeSet<ExpectedStackEdge>,
    span_edges: BTreeSet<ExpectedSpanEdge>,
    span_associations: BTreeSet<ExpectedSpanAssociation>,
    expectation_timestamps: Vec<u64>,
    measurement_start_ns: Option<u64>,
    measurement_end_ns: Option<u64>,
}

impl ModelBuilder {
    fn add_expectation(&mut self, event: ExpectationEvent) -> Result<()> {
        let feature = event.feature;
        self.expectation_timestamps.push(event.timestamp_ns);

        match feature {
            FixtureFeature::Cpu | FixtureFeature::TaskDump => {
                let parsed = parse_weighted_symbol(&event.name)?;
                let expected_domain = match feature {
                    FixtureFeature::Cpu => SymbolDomain::Cpu,
                    FixtureFeature::TaskDump => SymbolDomain::Wait,
                    FixtureFeature::Span => unreachable!(),
                };
                ensure!(
                    parsed.domain == expected_domain,
                    "feature {:?} does not match weighted symbol {:?}",
                    feature,
                    event.name
                );

                ensure!(
                    self.symbol_identities
                        .insert((feature, parsed.name.clone())),
                    "duplicate fixture identity ({:?}, {:?})",
                    feature,
                    parsed.name
                );
                let symbol = ExpectedSymbol {
                    feature,
                    symbol: FunctionSymbol::new(&event.name),
                    name: parsed.name,
                    weight: parsed.weight,
                };
                ensure!(
                    self.symbols.insert(symbol),
                    "duplicate fixture expectation for {:?}",
                    event.name
                );

                if let Some(parent) = event.parent {
                    validate_mixed_function_name(&parent)?;
                    self.stack_edges.insert(ExpectedEdge {
                        parent: FunctionSymbol::new(parent),
                        child: FunctionSymbol::new(&event.name),
                    });
                }
                if let Some(active_span) = event.active_span {
                    validate_span_name(&active_span)?;
                    self.span_associations.insert(ExpectedSpanAssociation {
                        symbol: FunctionSymbol::new(event.name),
                        active_span: SpanName::new(active_span),
                    });
                }
            }
            FixtureFeature::Span => {
                validate_span_name(&event.name)?;
                ensure!(
                    event.active_span.is_none(),
                    "span expectation {:?} cannot declare active_span",
                    event.name
                );
                ensure!(
                    self.spans.insert(SpanName::new(&event.name)),
                    "duplicate fixture expectation for {:?}",
                    event.name
                );
                if let Some(parent) = event.parent {
                    validate_span_name(&parent)?;
                    self.span_edges.insert(ExpectedEdge {
                        parent: SpanName::new(parent),
                        child: SpanName::new(event.name),
                    });
                }
            }
        }

        Ok(())
    }

    fn add_marker(&mut self, event: MarkerEvent) -> Result<()> {
        let slot = match event.phase {
            MarkerPhase::MeasurementStart => &mut self.measurement_start_ns,
            MarkerPhase::MeasurementEnd => &mut self.measurement_end_ns,
        };
        ensure!(slot.is_none(), "duplicate fixture marker {:?}", event.phase);
        *slot = Some(event.timestamp_ns);
        Ok(())
    }

    fn finish(self) -> Result<ExpectedModel> {
        ensure!(
            !self.symbols.is_empty(),
            "trace declares no fixture symbols"
        );
        ensure!(!self.spans.is_empty(), "trace declares no fixture spans");

        let start_ns = self
            .measurement_start_ns
            .context("trace is missing measurement_start marker")?;
        let end_ns = self
            .measurement_end_ns
            .context("trace is missing measurement_end marker")?;
        ensure!(
            start_ns < end_ns,
            "measurement markers are not ordered: start={start_ns}, end={end_ns}"
        );
        ensure!(
            self.expectation_timestamps.iter().all(|&ts| ts < start_ns),
            "fixture expectations must be emitted before measurement_start"
        );

        for edge in &self.span_edges {
            ensure!(
                self.spans.contains(&edge.parent),
                "span {:?} refers to undeclared parent {:?}",
                edge.child,
                edge.parent
            );
        }
        for association in &self.span_associations {
            ensure!(
                self.spans.contains(&association.active_span),
                "symbol {:?} refers to undeclared active span {:?}",
                association.symbol,
                association.active_span
            );
        }

        Ok(ExpectedModel {
            symbols: self.symbols,
            spans: self.spans,
            stack_edges: self.stack_edges,
            span_edges: self.span_edges,
            span_associations: self.span_associations,
            measurement: MeasurementWindow { start_ns, end_ns },
        })
    }
}

pub(crate) fn read_expected_model<'a>(
    segments: impl IntoIterator<Item = &'a [u8]>,
) -> Result<ExpectedModel> {
    let mut builder = ModelBuilder::default();
    let mut segment_count = 0_usize;

    for (index, bytes) in segments.into_iter().enumerate() {
        segment_count += 1;
        let mut decoder = Decoder::new(bytes)
            .with_context(|| format!("trace segment {index} has no valid header"))?;
        decoder
            .try_for_each_event(|raw| match raw.name {
                EXPECTATION_EVENT => raw
                    .deserialize::<ExpectationEvent>()
                    .context("decode TelemetryFixtureExpectationEvent")
                    .and_then(|event| builder.add_expectation(event)),
                MARKER_EVENT => raw
                    .deserialize::<MarkerEvent>()
                    .context("decode TelemetryFixtureMarkerEvent")
                    .and_then(|event| builder.add_marker(event)),
                _ => Ok(()),
            })
            .map_err(|error| anyhow::anyhow!("read trace segment {index}: {error}"))?;
    }

    ensure!(segment_count > 0, "no trace segments supplied");
    builder.finish()
}

fn parse_weighted_symbol(symbol: &str) -> Result<ParsedWeightedSymbol> {
    let rest = symbol
        .strip_prefix(FIXTURE_PREFIX)
        .with_context(|| format!("weighted symbol {symbol:?} lacks {FIXTURE_PREFIX:?} prefix"))?;
    let (domain, rest) = if let Some(rest) = rest.strip_prefix("cpu_") {
        (SymbolDomain::Cpu, rest)
    } else if let Some(rest) = rest.strip_prefix("wait_") {
        (SymbolDomain::Wait, rest)
    } else {
        bail!("weighted symbol {symbol:?} has unknown domain");
    };
    let (name, weight) = rest
        .rsplit_once(WEIGHT_SEPARATOR)
        .with_context(|| format!("weighted symbol {symbol:?} has no weight suffix"))?;
    ensure!(!name.is_empty(), "weighted symbol {symbol:?} has no name");
    let weight = weight
        .parse::<NonZeroU64>()
        .with_context(|| format!("weighted symbol {symbol:?} has invalid weight"))?;
    Ok(ParsedWeightedSymbol {
        domain,
        name: name.to_owned(),
        weight,
    })
}

fn validate_mixed_function_name(name: &str) -> Result<()> {
    ensure!(
        name.strip_prefix("dial9_fixture_mixed_")
            .is_some_and(|name| !name.is_empty()),
        "invalid fixture parent function {name:?}"
    );
    Ok(())
}

fn validate_span_name(name: &str) -> Result<()> {
    ensure!(
        name.strip_prefix("dial9_fixture_span_")
            .is_some_and(|name| !name.is_empty()),
        "invalid fixture span name {name:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dial9_trace_format::{TraceEvent, encoder::Encoder};

    #[derive(TraceEvent)]
    struct TelemetryFixtureExpectationEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        feature: String,
        name: String,
        parent: Option<String>,
        active_span: Option<String>,
    }

    #[derive(TraceEvent)]
    struct TelemetryFixtureMarkerEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        phase: String,
    }

    fn expectation(
        timestamp_ns: u64,
        feature: &str,
        name: &str,
        parent: Option<&str>,
        active_span: Option<&str>,
    ) -> TelemetryFixtureExpectationEvent {
        TelemetryFixtureExpectationEvent {
            timestamp_ns,
            feature: feature.to_owned(),
            name: name.to_owned(),
            parent: parent.map(str::to_owned),
            active_span: active_span.map(str::to_owned),
        }
    }

    fn marker(timestamp_ns: u64, phase: &str) -> TelemetryFixtureMarkerEvent {
        TelemetryFixtureMarkerEvent {
            timestamp_ns,
            phase: phase.to_owned(),
        }
    }

    fn valid_trace() -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder
            .write(&expectation(
                10,
                "cpu",
                "dial9_fixture_cpu_outer_weight_1",
                Some("dial9_fixture_mixed_cycle"),
                Some("dial9_fixture_span_cycle"),
            ))
            .unwrap();
        encoder
            .write(&expectation(
                11,
                "task_dump",
                "dial9_fixture_wait_database_lookup_weight_14",
                Some("dial9_fixture_mixed_inner"),
                Some("dial9_fixture_span_inner"),
            ))
            .unwrap();
        encoder
            .write(&expectation(
                12,
                "span",
                "dial9_fixture_span_cycle",
                None,
                None,
            ))
            .unwrap();
        encoder
            .write(&expectation(
                13,
                "span",
                "dial9_fixture_span_inner",
                Some("dial9_fixture_span_cycle"),
                None,
            ))
            .unwrap();
        encoder.write(&marker(100, "measurement_start")).unwrap();
        encoder.write(&marker(200, "measurement_end")).unwrap();
        encoder.finish()
    }

    #[test]
    fn reads_the_self_described_expected_model() {
        let trace = valid_trace();
        let model = read_expected_model([trace.as_slice()]).unwrap();

        assert_eq!(
            model.measurement,
            MeasurementWindow {
                start_ns: 100,
                end_ns: 200
            }
        );
        assert_eq!(model.symbols.len(), 2);
        assert!(model.symbols.contains(&ExpectedSymbol {
            feature: FixtureFeature::Cpu,
            symbol: FunctionSymbol::new("dial9_fixture_cpu_outer_weight_1"),
            name: "outer".to_owned(),
            weight: NonZeroU64::new(1).unwrap(),
        }));
        assert!(model.symbols.contains(&ExpectedSymbol {
            feature: FixtureFeature::TaskDump,
            symbol: FunctionSymbol::new("dial9_fixture_wait_database_lookup_weight_14"),
            name: "database_lookup".to_owned(),
            weight: NonZeroU64::new(14).unwrap(),
        }));
        assert_eq!(
            model.spans,
            BTreeSet::from([
                SpanName::new("dial9_fixture_span_cycle"),
                SpanName::new("dial9_fixture_span_inner"),
            ])
        );
        assert_eq!(model.stack_edges.len(), 2);
        assert_eq!(
            model.span_edges,
            BTreeSet::from([ExpectedEdge {
                parent: SpanName::new("dial9_fixture_span_cycle"),
                child: SpanName::new("dial9_fixture_span_inner"),
            }])
        );
        assert_eq!(model.span_associations.len(), 2);
    }

    #[test]
    fn rejects_a_feature_that_disagrees_with_the_symbol_domain() {
        let mut encoder = Encoder::new();
        encoder
            .write(&expectation(
                10,
                "cpu",
                "dial9_fixture_wait_inner_weight_2",
                None,
                None,
            ))
            .unwrap();
        encoder
            .write(&expectation(
                11,
                "span",
                "dial9_fixture_span_cycle",
                None,
                None,
            ))
            .unwrap();
        encoder.write(&marker(100, "measurement_start")).unwrap();
        encoder.write(&marker(200, "measurement_end")).unwrap();
        let trace = encoder.finish();

        let error = read_expected_model([trace.as_slice()]).unwrap_err();
        assert!(error.to_string().contains("does not match weighted symbol"));
    }

    #[test]
    fn rejects_two_weights_for_the_same_fixture_identity() {
        let mut encoder = Encoder::new();
        for (timestamp_ns, weight) in [(10, 1), (11, 2)] {
            encoder
                .write(&expectation(
                    timestamp_ns,
                    "cpu",
                    &format!("dial9_fixture_cpu_outer_weight_{weight}"),
                    None,
                    None,
                ))
                .unwrap();
        }
        encoder
            .write(&expectation(
                12,
                "span",
                "dial9_fixture_span_cycle",
                None,
                None,
            ))
            .unwrap();
        encoder.write(&marker(100, "measurement_start")).unwrap();
        encoder.write(&marker(200, "measurement_end")).unwrap();
        let trace = encoder.finish();

        let error = read_expected_model([trace.as_slice()]).unwrap_err();
        assert!(error.to_string().contains("duplicate fixture identity"));
    }

    #[test]
    fn rejects_missing_or_unordered_measurement_markers() {
        let trace = valid_trace();
        let mut missing_end = Encoder::new();
        missing_end
            .write(&expectation(
                10,
                "cpu",
                "dial9_fixture_cpu_outer_weight_1",
                None,
                None,
            ))
            .unwrap();
        missing_end
            .write(&expectation(
                11,
                "span",
                "dial9_fixture_span_cycle",
                None,
                None,
            ))
            .unwrap();
        missing_end
            .write(&marker(100, "measurement_start"))
            .unwrap();
        let missing_end = missing_end.finish();
        assert!(
            read_expected_model([missing_end.as_slice()])
                .unwrap_err()
                .to_string()
                .contains("missing measurement_end")
        );

        let mut unordered = Encoder::new();
        unordered
            .write(&expectation(
                10,
                "cpu",
                "dial9_fixture_cpu_outer_weight_1",
                None,
                None,
            ))
            .unwrap();
        unordered
            .write(&expectation(
                11,
                "span",
                "dial9_fixture_span_cycle",
                None,
                None,
            ))
            .unwrap();
        unordered.write(&marker(200, "measurement_start")).unwrap();
        unordered.write(&marker(100, "measurement_end")).unwrap();
        let unordered = unordered.finish();
        assert!(
            read_expected_model([unordered.as_slice()])
                .unwrap_err()
                .to_string()
                .contains("markers are not ordered")
        );

        // Keep the happy-path fixture used so accidental test refactors cannot
        // silently make it malformed while only exercising error cases.
        read_expected_model([trace.as_slice()]).unwrap();
    }
}
