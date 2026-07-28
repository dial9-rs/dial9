#![cfg(all(unix, feature = "process-resource"))]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, ProcessResourceUsageConfig, RecorderPerfExt, RecorderPipelineExt,
    TokioAttachOptions, recorder,
};
use std::time::Duration;

#[cfg(target_os = "freebsd")]
fn wait_for_rss_accounting_tick() {
    use std::mem::MaybeUninit;
    use std::time::Instant;

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut working_set = vec![0_u8; 1024 * 1024];
    loop {
        // FreeBSD updates ru_maxrss from its statclock path while the process
        // is running. Sleeping alone may leave it at zero, so exercise a
        // resident working set through at least one likely clock tick.
        let work_until = Instant::now() + Duration::from_millis(10);
        while Instant::now() < work_until {
            for page in working_set.chunks_mut(4096) {
                page[0] = page[0].wrapping_add(1);
            }
            std::hint::black_box(&working_set);
        }

        let mut usage = MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `usage.as_mut_ptr()` points to valid storage for getrusage
        // to initialize, and RUSAGE_SELF has no additional preconditions.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(
            rc,
            0,
            "getrusage failed while waiting for RSS accounting: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: getrusage returned success and initialized `usage`.
        let usage = unsafe { usage.assume_init() };
        if usage.ru_maxrss > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "ru_maxrss remained zero after waiting for a resource-accounting tick"
        );
    }
}

#[cfg(not(target_os = "freebsd"))]
fn wait_for_rss_accounting_tick() {}

#[test]
fn traced_runtime_records_process_resource_usage() {
    // FreeBSD may need one accounting tick before ru_maxrss becomes non-zero.
    // Since it is a process maximum, every later source sample must retain it.
    wait_for_rss_accounting_tick();

    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_process_resource_usage(ProcessResourceUsageConfig::default())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);
    let metrics: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Dial9Event::ProcessResourceUsageEvent(event) => Some(event),
            _ => None,
        })
        .collect();

    assert!(
        !metrics.is_empty(),
        "expected at least one process resource usage event"
    );
    assert!(metrics[0].timestamp_ns > 0);
    assert!(metrics[0].max_rss_bytes > 0);
}

#[test]
fn traced_runtime_does_not_record_process_resource_usage_by_default() {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(&recorder, 1, TokioAttachOptions::default());

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Dial9Event::ProcessResourceUsageEvent(_))),
        "process resource usage should be opt-in"
    );
}
