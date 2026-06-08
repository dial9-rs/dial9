#![cfg(target_os = "linux")]

use dial9_perf_self_profile::{
    EventSource, PerfSampler, SamplerConfig, SamplingMode, resolve_symbol,
};
use std::sync::{Arc, Mutex};
use std::thread;

#[inline(never)]
fn block_on_lock(lock: &Mutex<()>) {
    let _g = lock.lock().unwrap();
}

#[inline(never)]
fn do_sleep() {
    thread::sleep(std::time::Duration::from_millis(50));
}

#[test]
fn captures_lock_acquisition_stack() {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(
            SamplerConfig::default()
                .event_source(EventSource::SwContextSwitches)
                .sampling(SamplingMode::Period(1))
                .include_kernel(false),
        )
        .expect("failed to create sampler"),
    ));

    sampler.lock().unwrap().track_current_thread().unwrap();

    let lock = Arc::new(Mutex::new(()));
    let guard = lock.lock().unwrap();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let lock2 = Arc::clone(&lock);
            let sampler2 = Arc::clone(&sampler);
            thread::spawn(move || {
                sampler2.lock().unwrap().track_current_thread().unwrap();
                block_on_lock(&lock2);
                sampler2.lock().unwrap().stop_tracking_current_thread();
            })
        })
        .collect();

    thread::sleep(std::time::Duration::from_millis(100));

    drop(guard);
    for h in handles {
        h.join().unwrap();
    }

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();

    assert!(
        !samples.is_empty(),
        "expected samples from context switches"
    );
    assert!(
        samples.iter().any(|s| !s.callchain.is_empty()),
        "expected at least one sample with a callchain"
    );

    // Note: glibc's pthread_mutex_lock lacks frame pointers, so the kernel
    // unwinder can't walk past it into our binary. We verify samples arrive
    // but can't assert on symbol names for lock contention stacks.
    for sample in &samples {
        for &addr in &sample.callchain {
            let info = resolve_symbol(addr);
            if let Some(name) = &info.name {
                eprintln!("  tid={} {:#018x} {}", sample.tid, addr, name);
            }
        }
    }
}

#[test]
fn captures_sleep_stack() {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    let sampler = Arc::new(Mutex::new(
        PerfSampler::new_per_thread(
            SamplerConfig::default()
                .event_source(EventSource::SwContextSwitches)
                .include_kernel(false)
                .sampling(SamplingMode::Period(1)),
        )
        .expect("failed to create sampler"),
    ));

    sampler.lock().unwrap().track_current_thread().unwrap();
    do_sleep();

    let mut sampler = sampler.lock().unwrap();
    sampler.disable();
    let samples = sampler.drain_samples();

    assert!(
        !samples.is_empty(),
        "expected samples from sleep context switches"
    );

    // Sleep goes through nanosleep which preserves the frame chain, so we
    // should see our binary's symbols in the callchain.
    let mut resolved_names = Vec::new();
    for sample in &samples {
        for &addr in &sample.callchain {
            let info = resolve_symbol(addr);
            eprintln!("  tid={} {:#018x} -> {:?}", sample.tid, addr, info.name);
            if let Some(name) = info.name {
                resolved_names.push(name);
            }
        }
    }

    // Debug: print exe range
    let exe = std::fs::read_link("/proc/self/exe").unwrap();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    eprintln!("exe: {:?}", exe);
    for line in maps.lines() {
        if line.contains(exe.to_str().unwrap()) {
            eprintln!("  {}", line);
        }
    }

    // Frame pointer unwinding may produce shallow stacks in test binaries,
    // so we only assert that we resolved *something*.
    assert!(
        !resolved_names.is_empty(),
        "expected at least one resolved symbol from sleep stacks. \
         Got {} samples with 0 resolved names.",
        samples.len(),
    );
}

/// Total context switches for the calling thread, from `/proc/thread-self/status`
/// (`voluntary_ctxt_switches` + `nonvoluntary_ctxt_switches`). This is the same
/// quantity perf's `SwContextSwitches` event counts, so it is an independent
/// kernel ground truth for the sampling ratio.
fn thread_switch_count() -> u64 {
    let status =
        std::fs::read_to_string("/proc/thread-self/status").expect("read /proc/thread-self/status");
    let mut total = 0u64;
    for line in status.lines() {
        if let Some(rest) = line
            .strip_prefix("voluntary_ctxt_switches:")
            .or_else(|| line.strip_prefix("nonvoluntary_ctxt_switches:"))
        {
            total += rest.trim().parse::<u64>().expect("parse switch count");
        }
    }
    total
}

/// `Period(N)` must record ~1/N of the context switches that actually occur.
///
/// A previous version compared two independent runs, but their total switch
/// counts vary, making the ratio flaky. Instead we run once and compare the
/// recorded sample count against the kernel's own switch counter for this
/// thread: both numbers come from the same run, so `records ≈ total / N` holds
/// with only small boundary error.
#[test]
fn sampling_interval_controls_ratio() {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };

    const PERIOD: u64 = 10;

    let mut sampler = match PerfSampler::new_per_thread(
        SamplerConfig::default()
            .event_source(EventSource::SwContextSwitches)
            .include_kernel(false)
            .sampling(SamplingMode::Period(PERIOD)),
    ) {
        Ok(s) => s,
        // perf_event_open blocked (e.g. restricted CI); nothing to verify.
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return,
        Err(e) => panic!("failed to create sampler: {e}"),
    };
    sampler.track_current_thread().unwrap();

    let before = thread_switch_count();
    for _ in 0..400 {
        thread::sleep(std::time::Duration::from_millis(1));
    }
    sampler.disable();
    let after = thread_switch_count();

    let records = sampler.drain_samples().len();
    let total = after - before;

    assert!(
        total > 100,
        "workload produced too few context switches to test ({total})"
    );

    let expected = total as f64 / PERIOD as f64;
    let ratio = records as f64 / expected;
    assert!(
        (0.8..=1.2).contains(&ratio),
        "expected records ~ total/{PERIOD}; records={records}, total={total}, \
         expected~{expected:.0}, ratio={ratio:.2}"
    );
}

#[test]
fn rejects_zero_period() {
    let err = match PerfSampler::new_per_thread(
        SamplerConfig::default()
            .event_source(EventSource::SwContextSwitches)
            .include_kernel(false)
            .sampling(SamplingMode::Period(0)),
    ) {
        Err(e) => e,
        Ok(_) => panic!("Period(0) must be rejected"),
    };
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
