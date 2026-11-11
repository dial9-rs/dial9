//! Shared fixture for the two leaf-caller-recovery tests.
//!
//! Both backends run the same chain, the same backend check and the same
//! assertions, so a change to one applies to both.
//!
//! Environment:
//! * `DIAL9_NO_SKIP=1` — a skip becomes a failure. In CI a skip reads as a
//!   pass, which is how an untested path looks green.
//! * `DIAL9_EXPECT_LEAVES=frameless|framed` — assert the build really is the
//!   one the caller intended. Under `-C force-frame-pointers=yes` the x29 chain
//!   already names the immediate caller, so a green run proves nothing about
//!   the recovery unless this is checked.
use std::sync::atomic::{AtomicU64, Ordering};

use dial9_perf_self_profile::unwinder::Unwinder;
use dial9_perf_self_profile::{
    ActiveCpuBackend, EventSource, PerfSampler, Sample, SamplerConfig, SamplingMode, resolve_symbol,
};

static SINK: AtomicU64 = AtomicU64::new(0);

/// Frameless leaf under `non-leaf` frame pointers: makes no call.
#[inline(never)]
fn spin_leaf(rounds: u64) -> u64 {
    let mut s = SINK.load(Ordering::Relaxed) | 1;
    let mut i = 0u64;
    while i < rounds {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(i);
        i += 1;
    }
    s
}

/// The frame the x29 chain drops, and the one recovery must restore.
#[inline(never)]
fn mid_frame(rounds: u64) -> u64 {
    spin_leaf(rounds).wrapping_add(1)
}

#[inline(never)]
fn outer_frame(rounds: u64) -> u64 {
    mid_frame(rounds).wrapping_add(2)
}

/// Last `::`-separated segment of a demangled symbol, or `None` if the address
/// does not resolve.
fn leaf_segment(addr: u64) -> Option<String> {
    let name = resolve_symbol(addr).name?;
    Some(name.rsplit("::").next()?.to_string())
}

/// Skip, unless `DIAL9_NO_SKIP` says a skip is a failure.
fn skip_or_fail(reason: &str) {
    if std::env::var_os("DIAL9_NO_SKIP").is_some() {
        panic!("DIAL9_NO_SKIP is set but the test could not run: {reason}");
    }
    eprintln!("skipping: {reason} (set DIAL9_NO_SKIP=1 to make this a failure)");
}

/// `PerfSampler::start` picks a backend from the environment, so a test that
/// does not check which one it got can silently measure the wrong path.
/// Variants are `#[non_exhaustive]`, hence matched rather than constructed.
fn require_backend(got: &ActiveCpuBackend, want_perf: bool, how: &str) -> bool {
    let is_perf = matches!(got, ActiveCpuBackend::Perf { .. });
    if is_perf == want_perf {
        return true;
    }
    skip_or_fail(&format!("active backend is {got:?} - {how}"));
    false
}

/// Assert the build matches `DIAL9_EXPECT_LEAVES`, when it is set.
fn check_expected_framing() {
    let Some(want) = std::env::var_os("DIAL9_EXPECT_LEAVES") else {
        return;
    };
    let want = want.to_string_lossy().to_string();
    let got = Unwinder::leaf_frame_pointers_present();
    let expected = match want.as_str() {
        "frameless" => Some(false),
        "framed" => Some(true),
        other => panic!("DIAL9_EXPECT_LEAVES must be frameless|framed, got {other:?}"),
    };
    assert_eq!(
        got, expected,
        "DIAL9_EXPECT_LEAVES={want} but leaf_frame_pointers_present() is {got:?}; \
         the build's -C force-frame-pointers setting is not the one this run needs"
    );
}

/// Check every sample whose innermost frame is the frameless leaf.
fn count_leaf_samples(samples: &[Sample]) {
    let mut leaf = 0usize;
    let mut with_mid = 0usize;
    let mut duplicated: Option<Vec<String>> = None;
    let mut example: Option<Vec<String>> = None;

    for s in samples {
        let innermost = s.callchain.first().copied().unwrap_or(s.ip);
        if leaf_segment(innermost).as_deref() != Some("spin_leaf") {
            continue;
        }
        leaf += 1;
        // The whole chain, not a prefix: a duplicate can land anywhere in it.
        let chain: Vec<String> = s
            .callchain
            .iter()
            .map(|&a| leaf_segment(a).unwrap_or_else(|| format!("{a:#x}")))
            .collect();
        let hits = chain.iter().skip(1).filter(|n| *n == "mid_frame").count();
        // Position, not presence: the frame is inserted directly above the
        // sampled leaf, so anywhere else means the index was wrong.
        if chain.get(1).map(String::as_str) == Some("mid_frame") {
            with_mid += 1;
        }
        if hits > 1 && duplicated.is_none() {
            duplicated = Some(chain.clone());
        }
        if example.is_none() {
            example = Some(chain);
        }
    }

    if leaf == 0 {
        skip_or_fail(&format!(
            "no samples landed in spin_leaf ({} samples total)",
            samples.len()
        ));
        return;
    }

    println!("leaf_samples={leaf} reporting_immediate_caller={with_mid} example={example:?}");
    assert_eq!(
        with_mid, leaf,
        "every sample in the frameless leaf must report mid_frame as its \
         immediate caller, directly above the leaf; got {with_mid}/{leaf}. The \
         x29 chain drops it, so the recorded LR must be re-inserted. Example \
         chain: {example:?}"
    );
    // Under `=yes` the chain already names mid_frame, so recovery must not add
    // a second copy.
    assert!(
        duplicated.is_none(),
        "mid_frame appears more than once: {duplicated:?}"
    );
}

/// Start the sampler on the wanted backend, drive the fixture, and check the
/// result. `how` explains what the caller must set for that backend.
pub fn run(want_perf: bool, how: &str) {
    check_expected_framing();

    let Ok(mut sampler) = PerfSampler::start(
        SamplerConfig::default()
            .event_source(EventSource::SwCpuClock)
            .sampling(SamplingMode::FrequencyHz(999)),
    ) else {
        skip_or_fail("no CPU sampling backend available");
        return;
    };
    if !require_backend(&sampler.active_backend(), want_perf, how) {
        return;
    }

    drive();
    sampler.disable();
    count_leaf_samples(&sampler.drain_samples());
}

/// Run the fixture hard enough that samples land inside the leaf.
fn drive() {
    let mut acc = 0u64;
    for _ in 0..200 {
        acc = acc.wrapping_add(outer_frame(2_000_000));
    }
    SINK.store(acc, Ordering::Relaxed);
}
