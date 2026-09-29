//! Building blocks for the smoke test: a CPU burn that survives release
//! optimizations, and an address to symbolize.

use std::hint::black_box;
use std::time::{Duration, Instant};

/// Spin the calling thread until it has consumed `target` of its own CPU
/// time, or `wall_cap` elapses. Returns the CPU time actually consumed.
///
/// Bounded by the thread's CPU clock rather than an iteration count, so the
/// optimizer cannot shrink it and a descheduled thread still does the full
/// amount of work. On platforms without a per-thread CPU clock it falls back
/// to wall time.
pub(crate) fn burn_thread_cpu(target: Duration, wall_cap: Duration) -> Duration {
    let start_wall = Instant::now();
    let Some(start_cpu) = thread_cpu_time() else {
        let mut x = 0u64;
        while start_wall.elapsed() < target {
            x = black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        return start_wall.elapsed();
    };
    let mut x = 0u64;
    loop {
        for _ in 0..1024 {
            x = black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        let Some(now) = thread_cpu_time() else {
            return start_wall.elapsed();
        };
        let spent = now.saturating_sub(start_cpu);
        if spent >= target || start_wall.elapsed() >= wall_cap {
            return spent;
        }
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
fn thread_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
fn thread_cpu_time() -> Option<Duration> {
    None
}

/// Address for a symbolization check. Kept out of line with a body unlike
/// any other function, so it is neither inlined nor merged with another.
#[cfg_attr(
    not(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    )),
    allow(dead_code)
)]
#[inline(never)]
pub(crate) fn symbolization_probe(x: u64) -> u64 {
    black_box(x).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5eed_d1a1_9000
}

/// Outcome of resolving [`symbolization_probe`]'s address with a separate
/// [`OfflineSymbolizer`](dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    )),
    allow(dead_code)
)]
pub(crate) enum SymbolizationProbe {
    /// Resolved to this name.
    Resolved(String),
    /// No symbol table entry came back for the address.
    NoEntry,
    /// The symbolizer returned an error.
    Error(String),
}

/// Resolve [`symbolization_probe`]'s address with a fresh
/// [`OfflineSymbolizer`](dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer), the
/// type the pipeline's symbolize stage wraps. Blocking; the first call pays the
/// full debug-info parse. The symbolizer is dropped on return.
pub(crate) fn probe_symbolization() -> SymbolizationProbe {
    #[cfg(not(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    )))]
    {
        SymbolizationProbe::Error("no symbolizer on this platform".into())
    }
    #[cfg(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    ))]
    {
        probe_symbolization_linux()
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
fn probe_symbolization_linux() -> SymbolizationProbe {
    use dial9_trace_format::decoder::Decoder;
    use dial9_trace_format::encoder::Encoder;
    use dial9_trace_format::schema::FieldDef;
    use dial9_trace_format::types::{FieldType, FieldValue, FieldValueRef};

    let addr = symbolization_probe as *const () as u64;
    let segment = {
        let mut enc = Encoder::new();
        let written = enc
            .register_schema(
                "SmokeProbe",
                vec![FieldDef::new("frames", FieldType::StackFrames)],
            )
            .and_then(|schema| {
                enc.write_event(&schema, 0, &[FieldValue::StackFrames(vec![addr].into())])
            });
        if let Err(e) = written {
            return SymbolizationProbe::Error(format!("encode probe: {e}"));
        }
        enc.finish()
    };
    let symbolizer = dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer::new();
    let symbols = match symbolizer.symbolize(&segment, &dial9_perf_self_profile::read_proc_maps()) {
        Ok(symbols) => symbols,
        Err(e) => return SymbolizationProbe::Error(e.to_string()),
    };
    let mut combined = segment;
    combined.extend_from_slice(&symbols);
    let Some(mut dec) = Decoder::new(&combined) else {
        return SymbolizationProbe::Error("decode symbols: bad trace header".into());
    };
    let mut found = None;
    let decoded = dec.for_each_event(|ev| {
        if found.is_some() || ev.name != "SymbolTableEntry" {
            return;
        }
        let field = |name: &str| {
            ev.field_names()
                .position(|n| n == name)
                .and_then(|i| ev.fields.get(i))
        };
        if let (Some(FieldValueRef::Varint(a)), Some(FieldValueRef::Varint(0))) =
            (field("addr"), field("inline_depth"))
            && *a == addr
            && let Some(FieldValueRef::PooledString(id)) = field("symbol_name")
        {
            found = ev.string_pool.get(*id).map(str::to_owned);
        }
    });
    if let Err(e) = decoded {
        return SymbolizationProbe::Error(format!("decode symbols: {e}"));
    }
    found.map_or(SymbolizationProbe::NoEntry, SymbolizationProbe::Resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burn_consumes_at_least_the_target_cpu_time() {
        let target = Duration::from_millis(20);
        let spent = burn_thread_cpu(target, Duration::from_secs(5));
        assert!(spent >= target, "spent {spent:?}, wanted {target:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_resolves_its_own_name() {
        match probe_symbolization() {
            SymbolizationProbe::Resolved(name) => {
                assert!(name.contains("symbolization_probe"), "{name}")
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }
}
