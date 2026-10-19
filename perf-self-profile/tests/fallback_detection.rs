#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use dial9_perf_self_profile::{
    EventSource, PerfSampler, SamplerConfig, SamplingMode, is_ctimer_active,
};

// libc doesn't re-export AUDIT_ARCH_*, see include/uapi/linux/audit.h.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

// Seccomp BPF: return EACCES for perf_event_open, allow everything else.
// PR_SET_NO_NEW_PRIVS lets us install the filter without CAP_SYS_ADMIN.
fn install_seccomp_blocking_perf_event_open() {
    let ld_abs = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let jeq = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;

    // seccomp_data offsets: nr @ 0, arch @ 4.
    let filter = [
        stmt(ld_abs, 4),
        jump(jeq, AUDIT_ARCH, 0, 3),
        stmt(ld_abs, 0),
        jump(jeq, libc::SYS_perf_event_open as u32, 0, 1),
        stmt(ret, libc::SECCOMP_RET_ERRNO | libc::EACCES as u32),
        stmt(ret, libc::SECCOMP_RET_ALLOW),
    ];
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut _,
    };

    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &prog as *const _ as libc::c_ulong,
                0,
                0,
            ),
            0
        );
    }
}

/// Raw self-profile probe. Returns true if the kernel lets us open a
/// minimal perf event on the calling task.
fn perf_event_open_works() -> bool {
    // perf_event_attr: type=PERF_TYPE_SOFTWARE (1), size=128,
    // config=PERF_COUNT_SW_CPU_CLOCK (0). Rest zeroed.
    let mut attr: [u8; 128] = [0; 128];
    attr[0] = 1;
    attr[4..8].copy_from_slice(&128u32.to_le_bytes());
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            attr.as_ptr(),
            0i32,  // pid = self
            0i32,  // cpu = 0
            -1i32, // group_fd
            0u64,  // flags
        )
    };
    if fd >= 0 {
        unsafe { libc::close(fd as i32) };
        true
    } else {
        false
    }
}

#[test]
fn uses_perf_path_when_available() {
    if !perf_event_open_works() {
        eprintln!("skipping: perf_event_open blocked in this env");
        return;
    }

    let _sampler = PerfSampler::start(
        SamplerConfig::default()
            .event_source(EventSource::SwCpuClock)
            .sampling(SamplingMode::FrequencyHz(999)),
    )
    .expect("PerfSampler::start should succeed when perf is available");

    assert!(
        !is_ctimer_active(),
        "dispatcher routed to ctimer despite perf being available"
    );
}

/// Run `child` in a forked process and panic with `failure` if it returns
/// `false`, or with "child panicked" if it panics. Forking keeps the seccomp
/// filter (sticky) and process-wide ctimer state out of the rest of the run.
fn in_fork(failure: &str, child: impl FnOnce() -> bool) {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // A panic must not unwind into the forked copy of the test harness.
        let code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(_) => 2,
        };
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    match libc::WEXITSTATUS(status) {
        0 => {}
        2 => panic!("child panicked, see its stderr"),
        _ => panic!("{failure}"),
    }
}

#[test]
fn falls_back_to_ctimer_when_perf_event_open_blocked() {
    in_fork("fallback did not trigger", || {
        install_seccomp_blocking_perf_event_open();
        let sampler = PerfSampler::start(
            SamplerConfig::default()
                .event_source(EventSource::SwCpuClock)
                .sampling(SamplingMode::FrequencyHz(999)),
        );
        sampler.is_ok() && is_ctimer_active()
    });
}

/// With perf blocked, a default (`Auto`) profiler falls back to ctimer and
/// reports it. Unlike forcing ctimer, this is the fallback users hit.
#[cfg(feature = "cpu-profiling")]
#[test]
fn auto_reports_ctimer_when_perf_event_open_blocked() {
    use dial9_perf_self_profile::{ActiveCpuBackend, CpuProfiler, CpuProfilingConfig};

    in_fork("Auto did not report ctimer", || {
        install_seccomp_blocking_perf_event_open();
        CpuProfiler::start(CpuProfilingConfig::default())
            .is_ok_and(|p| matches!(p.active_backend(), ActiveCpuBackend::Ctimer { .. }))
    });
}

/// An `Auto` profiler on perf reports perf while another profiler runs ctimer.
/// ctimer state is process-wide, so a backend read from it would say ctimer.
#[cfg(feature = "cpu-profiling")]
#[test]
fn auto_reports_perf_while_another_profiler_runs_ctimer() {
    use dial9_core::source::Source;
    use dial9_perf_self_profile::{ActiveCpuBackend, CpuProfiler, CpuProfilingConfig};

    if !perf_event_open_works() {
        eprintln!("skipping: perf_event_open blocked in this env");
        return;
    }
    in_fork("Auto on perf did not report perf", || {
        let _ctimer = CpuProfiler::start(CpuProfilingConfig::with_ctimer_backend())
            .expect("ctimer profiler starts without perf access");
        let mut perf = CpuProfiler::start(
            CpuProfilingConfig::default().event_source(EventSource::SwContextSwitches),
        )
        .expect("context-switch perf should start when perf is available");
        assert!(matches!(
            perf.active_backend(),
            ActiveCpuBackend::Perf { .. }
        ));
        let mut metadata = Vec::new();
        perf.segment_metadata(&mut metadata);
        assert!(metadata.contains(&("cpu.profile.backend".into(), "perf".into())));
        assert!(metadata.contains(&(
            "cpu.profile.event_source".into(),
            "sw_context_switches".into(),
        )));
        true
    });
}

/// Startup failure metadata survives size-based rotation in the real recorder.
#[cfg(feature = "cpu-profiling")]
#[test]
fn failed_profiler_metadata_survives_segment_rotation() {
    use dial9_core::buffer::DiskBuffer;
    use dial9_core::recorder::recorder;
    use dial9_core::test_util::drain_thread_local;
    use dial9_perf_self_profile::{
        CpuProfiler, CpuProfilingConfig, RecorderPerfExt, SchedEventConfig, SchedProfiler,
        StartFailed,
    };
    use dial9_trace_format::TraceEvent;
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(TraceEvent)]
    struct Marker {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        payload: String,
    }

    #[derive(Deserialize)]
    #[serde(tag = "event")]
    enum DecodedEvent {
        SegmentMetadataEvent {
            entries: HashMap<String, String>,
        },
        #[serde(other)]
        Other,
    }

    in_fork("startup failure metadata missing after rotation", || {
        install_seccomp_blocking_perf_event_open();
        let dir = tempfile::tempdir().expect("trace directory");
        let writer = DiskBuffer::builder()
            .base_path(dir.path())
            .max_file_size(1024)
            .max_total_size(128 * 1024)
            .rotation_period(std::time::Duration::MAX)
            .build()
            .expect("writer");
        let rec = recorder(writer)
            .with_cpu_profiling(CpuProfilingConfig::with_perf_backend())
            .with_sched_events(SchedEventConfig::default())
            .build();
        let handle = rec.handle();
        let cpu_error = handle
            .with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_owned())
            .expect("CPU failure placeholder");
        let sched_error = handle
            .with_source(|f: &mut StartFailed<SchedProfiler>| f.message().to_owned())
            .expect("scheduler failure placeholder");
        let shared = rec.shared().expect("live recorder");
        for _ in 0..4 {
            handle.record_event(Marker {
                timestamp_ns: dial9_core::clock::clock_monotonic_ns(),
                payload: "x".repeat(4096),
            });
            drain_thread_local(shared);
        }
        rec.graceful_shutdown(std::time::Duration::ZERO);

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("trace directory")
            .map(|entry| entry.expect("trace entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        assert!(files.len() >= 2, "expected at least two rotated segments");
        for path in files {
            let bytes = std::fs::read(&path).expect("sealed segment");
            let mut decoder =
                dial9_trace_format::decoder::Decoder::new(&bytes).expect("trace header");
            let mut metadata = HashMap::new();
            decoder
                .for_each_event(|raw| {
                    if let DecodedEvent::SegmentMetadataEvent { entries } =
                        raw.deserialize().expect("event should deserialize")
                    {
                        metadata.extend(entries);
                    }
                })
                .expect("segment should decode");
            assert_eq!(metadata.get("cpu.profile.start_error"), Some(&cpu_error));
            assert_eq!(
                metadata.get("sched.profile.start_error"),
                Some(&sched_error)
            );
        }
        true
    });
}

/// With perf blocked, `with_cpu_profiling` and `with_sched_events` register
/// `StartFailed` placeholders whose errors say how to allow `perf_event_open`.
/// The CPU profiler is forced onto perf: `Auto` would fall back to ctimer.
#[cfg(feature = "cpu-profiling")]
#[test]
fn failed_profilers_register_placeholders_when_perf_event_open_blocked() {
    use dial9_core::buffer::MemoryBuffer;
    use dial9_core::recorder::recorder;
    use dial9_perf_self_profile::{
        CpuProfiler, CpuProfilingConfig, RecorderPerfExt, SchedEventConfig, SchedProfiler,
        StartFailed,
    };

    in_fork("placeholders missing or without the perf hint", || {
        install_seccomp_blocking_perf_event_open();
        let rec = recorder(MemoryBuffer::new(64 * 1024).expect("writer"))
            .with_cpu_profiling(CpuProfilingConfig::with_perf_backend())
            .with_sched_events(SchedEventConfig::default())
            .build();
        let h = rec.handle();
        let ok = h
            .with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_string())
            .is_some_and(|m| m.contains("CAP_PERFMON"))
            && h.with_source(|f: &mut StartFailed<SchedProfiler>| f.message().to_string())
                .is_some_and(|m| m.contains("perf_event_paranoid"))
            && h.with_source(|_: &mut CpuProfiler| ()).is_none()
            && h.with_source(|_: &mut SchedProfiler| ()).is_none();
        rec.graceful_shutdown(std::time::Duration::ZERO);
        ok
    });
}
