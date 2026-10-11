#![cfg(target_os = "linux")]

use dial9_perf_self_profile::{
    EventSource, PerfSampler, SamplerConfig, SamplingMode, resolve_symbol,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

#[inline(never)]
fn spin(deadline: Instant) {
    let mut sum = 0u64;
    while Instant::now() < deadline {
        let mut i = 0u64;
        while i < 100_000 {
            sum = std::hint::black_box(sum.wrapping_add(i));
            i += 1;
        }
    }
}

static SPIN_CALL_LINE: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn caller(deadline: Instant) {
    SPIN_CALL_LINE.store(line!() + 1, Ordering::Relaxed);
    spin(deadline);
    // SAFETY: `nop` has no effect.
    unsafe { core::arch::asm!("nop", options(nomem, nostack, preserves_flags)) };
}

#[test]
fn caller_frame_symbolizes_to_call_site() {
    let mut sampler = PerfSampler::start(
        SamplerConfig::default()
            .event_source(EventSource::SwCpuClock)
            .sampling(SamplingMode::FrequencyHz(999)),
    )
    .expect("failed to start sampler");

    caller(Instant::now() + Duration::from_millis(300));
    let call_line = SPIN_CALL_LINE.load(Ordering::Relaxed);

    sampler.disable();
    let samples = sampler.drain_samples();

    let mut checked = 0;
    for sample in &samples {
        let is_spin = |&addr: &u64| {
            resolve_symbol(addr)
                .name
                .is_some_and(|n| n.ends_with("::spin"))
        };
        let Some(spin_idx) = sample.callchain.iter().position(is_spin) else {
            continue;
        };
        let Some(&caller_addr) = sample.callchain.get(spin_idx + 1) else {
            continue;
        };
        let caller = resolve_symbol(caller_addr);
        assert_eq!(
            caller.code_info.and_then(|c| c.line),
            Some(call_line),
            "frame above `spin` should be its call site, got {:?}",
            caller.name
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no sample had `spin` on the stack ({} samples)",
        samples.len()
    );
}
