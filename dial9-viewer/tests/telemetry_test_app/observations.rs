use super::expectations::{
    ExpectedModel, ExpectedStackEdge, FixtureFeature, FunctionSymbol, SpanName,
};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;

const MIN_FIXTURE_CPU_SAMPLES: u64 = 20;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Observations {
    pub(crate) stacks: Vec<ObservedStack>,
    pub(crate) spans: Vec<ObservedSpan>,
    pub(crate) associations: Vec<ObservedSpanAssociation>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ObservedStack {
    pub(crate) feature: FixtureFeature,
    pub(crate) frames: Vec<FunctionSymbol>,
    pub(crate) count: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ObservedSpan {
    pub(crate) name: SpanName,
    pub(crate) parent: Option<SpanName>,
    pub(crate) field_names: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ObservedSpanAssociation {
    pub(crate) feature: FixtureFeature,
    pub(crate) symbol: FunctionSymbol,
    pub(crate) active_span: SpanName,
}

pub(crate) fn compare_observations(
    source: &str,
    expected: &ExpectedModel,
    observed: &Observations,
) -> Result<()> {
    for symbol in &expected.symbols {
        ensure!(
            symbol_count(observed, symbol.feature, &symbol.symbol) > 0,
            "{source} did not observe {:?} symbol {:?}",
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
        "{source} observed only {cpu_samples} fixture CPU samples"
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
        let feature_stacks: Vec<_> = observed
            .stacks
            .iter()
            .filter(|stack| stack.feature == feature)
            .map(|stack| {
                stack
                    .frames
                    .iter()
                    .map(FunctionSymbol::as_str)
                    .collect::<Vec<_>>()
            })
            .collect();
        ensure!(
            observed
                .stacks
                .iter()
                .any(|stack| stack.feature == feature && stack_has_edge(stack, edge)),
            "{source} did not observe {:?} stack edge {:?} -> {:?}; observed stacks: {:?}",
            feature,
            edge.parent.as_str(),
            edge.child.as_str(),
            feature_stacks,
        );
    }

    for span in &expected.spans {
        ensure!(
            observed.spans.iter().any(|item| &item.name == span),
            "{source} did not observe span {:?}",
            span.as_str()
        );
    }
    for edge in &expected.span_edges {
        ensure!(
            observed.spans.iter().any(|span| {
                span.name == edge.child && span.parent.as_ref() == Some(&edge.parent)
            }),
            "{source} did not observe span edge {:?} -> {:?}",
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
                    && item.symbol == association.symbol
                    && item.active_span == association.active_span
            }),
            "{source} did not associate {:?} with span {:?}",
            association.symbol.as_str(),
            association.active_span.as_str()
        );
    }

    let cycle_spans: Vec<_> = observed
        .spans
        .iter()
        .filter(|span| span.name.as_str() == "dial9_fixture_span_cycle")
        .collect();
    ensure!(
        !cycle_spans.is_empty()
            && cycle_spans
                .iter()
                .all(|span| span.field_names.iter().any(|field| field == "cycle")),
        "{source} did not retain the cycle field on cycle spans"
    );

    Ok(())
}

fn symbol_count(observed: &Observations, feature: FixtureFeature, symbol: &FunctionSymbol) -> u64 {
    observed
        .stacks
        .iter()
        .filter(|stack| {
            stack.feature == feature && stack.frames.iter().any(|frame| frame == symbol)
        })
        .map(|stack| stack.count)
        .sum()
}

fn stack_has_edge(stack: &ObservedStack, edge: &ExpectedStackEdge) -> bool {
    let parent = stack.frames.iter().position(|frame| frame == &edge.parent);
    let child = stack.frames.iter().position(|frame| frame == &edge.child);
    matches!((parent, child), (Some(parent), Some(child)) if parent < child)
}

fn assert_cpu_weight_order(expected: &ExpectedModel, observed: &Observations) -> Result<()> {
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
