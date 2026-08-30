use super::expectations::{ExpectedModel, ExpectedStackEdge, FixtureFeature, FunctionSymbol};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use std::{path::Path, process::Command};

const MIN_FIXTURE_CPU_SAMPLES: u64 = 20;

#[derive(Debug, Deserialize)]
struct LocalObservations {
    stacks: Vec<ObservedStack>,
    spans: Vec<ObservedSpan>,
    associations: Vec<ObservedSpanAssociation>,
}

#[derive(Debug, Deserialize)]
struct ObservedStack {
    feature: FixtureFeature,
    frames: Vec<String>,
    count: u64,
}

#[derive(Debug, Deserialize)]
struct ObservedSpan {
    name: String,
    parent: Option<String>,
    field_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ObservedSpanAssociation {
    feature: FixtureFeature,
    symbol: String,
    active_span: String,
}

pub(crate) fn check_local_trace(
    trace_paths: &[impl AsRef<Path>],
    expected: &ExpectedModel,
) -> Result<()> {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/telemetry_test_app/check_local.js");
    let output = Command::new("node")
        .arg(script)
        .args(trace_paths.iter().map(AsRef::as_ref))
        .output()
        .context("run local JavaScript telemetry checker with Node.js")?;
    ensure!(
        output.status.success(),
        "local JavaScript telemetry checker failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: LocalObservations = serde_json::from_slice(&output.stdout)
        .context("decode local JavaScript telemetry observations")?;
    compare_observations(expected, &observed)
}

fn compare_observations(expected: &ExpectedModel, observed: &LocalObservations) -> Result<()> {
    for symbol in &expected.symbols {
        ensure!(
            symbol_count(observed, symbol.feature, &symbol.symbol) > 0,
            "local JavaScript parser did not observe {:?} symbol {:?}",
            symbol.feature,
            symbol.symbol.as_str()
        );
    }

    let cpu_samples: u64 = expected
        .symbols
        .iter()
        .filter(|symbol| symbol.feature == FixtureFeature::Cpu)
        .map(|symbol| symbol_count(observed, symbol.feature, &symbol.symbol))
        .sum();
    ensure!(
        cpu_samples >= MIN_FIXTURE_CPU_SAMPLES,
        "local JavaScript parser observed only {cpu_samples} fixture CPU samples"
    );
    assert_cpu_weight_order(expected, observed)?;

    for edge in &expected.stack_edges {
        let feature = expected
            .symbols
            .iter()
            .find(|symbol| symbol.symbol == edge.child)
            .with_context(|| {
                format!(
                    "expected stack edge has undeclared child {:?}",
                    edge.child.as_str()
                )
            })?
            .feature;
        ensure!(
            observed
                .stacks
                .iter()
                .any(|stack| stack.feature == feature && stack_has_edge(stack, edge)),
            "local JavaScript parser did not observe {:?} stack edge {:?} -> {:?}",
            feature,
            edge.parent.as_str(),
            edge.child.as_str()
        );
    }

    for span in &expected.spans {
        ensure!(
            observed.spans.iter().any(|item| item.name == span.as_str()),
            "local JavaScript parser did not observe span {:?}",
            span.as_str()
        );
    }
    for edge in &expected.span_edges {
        ensure!(
            observed.spans.iter().any(|span| {
                span.name == edge.child.as_str()
                    && span.parent.as_deref() == Some(edge.parent.as_str())
            }),
            "local JavaScript parser did not observe span edge {:?} -> {:?}",
            edge.parent.as_str(),
            edge.child.as_str()
        );
    }
    for association in &expected.span_associations {
        let feature = expected
            .symbols
            .iter()
            .find(|symbol| symbol.symbol == association.symbol)
            .with_context(|| {
                format!(
                    "expected span association has undeclared symbol {:?}",
                    association.symbol.as_str()
                )
            })?
            .feature;
        ensure!(
            observed.associations.iter().any(|item| {
                item.feature == feature
                    && item.symbol == association.symbol.as_str()
                    && item.active_span == association.active_span.as_str()
            }),
            "local JavaScript parser did not associate {:?} with span {:?}",
            association.symbol.as_str(),
            association.active_span.as_str()
        );
    }

    let cycle_spans: Vec<_> = observed
        .spans
        .iter()
        .filter(|span| span.name == "dial9_fixture_span_cycle")
        .collect();
    ensure!(
        !cycle_spans.is_empty()
            && cycle_spans
                .iter()
                .all(|span| span.field_names.iter().any(|field| field == "cycle")),
        "local JavaScript parser did not retain the cycle field on cycle spans"
    );

    Ok(())
}

fn symbol_count(
    observed: &LocalObservations,
    feature: FixtureFeature,
    symbol: &FunctionSymbol,
) -> u64 {
    observed
        .stacks
        .iter()
        .filter(|stack| {
            stack.feature == feature && stack.frames.iter().any(|frame| frame == symbol.as_str())
        })
        .map(|stack| stack.count)
        .sum()
}

fn stack_has_edge(stack: &ObservedStack, edge: &ExpectedStackEdge) -> bool {
    let parent = stack
        .frames
        .iter()
        .position(|frame| frame == edge.parent.as_str());
    let child = stack
        .frames
        .iter()
        .position(|frame| frame == edge.child.as_str());
    matches!((parent, child), (Some(parent), Some(child)) if parent < child)
}

fn assert_cpu_weight_order(expected: &ExpectedModel, observed: &LocalObservations) -> Result<()> {
    let symbols: Vec<_> = expected
        .symbols
        .iter()
        .filter(|symbol| symbol.feature == FixtureFeature::Cpu)
        .collect();
    for left in &symbols {
        for right in &symbols {
            if left.weight <= right.weight {
                continue;
            }
            let left_count = symbol_count(observed, FixtureFeature::Cpu, &left.symbol);
            let right_count = symbol_count(observed, FixtureFeature::Cpu, &right.symbol);
            ensure!(
                left_count > right_count,
                "CPU symbol {:?} (weight {}) has {left_count} samples; {:?} (weight {}) has {right_count}",
                left.symbol.as_str(),
                left.weight,
                right.symbol.as_str(),
                right.weight
            );
        }
    }
    Ok(())
}
