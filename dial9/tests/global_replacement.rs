//! Two recorders contending for the process-global handle.

use dial9::core::clock_monotonic_ns;
use dial9::{Dial9Handle, DiskBuffer, recorder};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::Decoder;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, serde::Deserialize, TraceEvent)]
struct Marker {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

/// Marker values in a recorder's trace. Empty when it sealed no segment, which
/// is what a recorder that never recorded anything leaves behind.
fn markers(dir: &Path) -> Vec<u64> {
    let Some(segment) = std::fs::read_dir(dir)
        .expect("trace dir readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.ends_with(".bin") && !name.ends_with(".active")
        })
    else {
        return Vec::new();
    };

    let bytes = std::fs::read(segment).expect("read segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace header");
    let mut values = Vec::new();
    decoder
        .for_each_event(|raw| {
            if raw.name == "Marker" {
                let event: Marker = raw.deserialize().expect("decodes");
                values.push(event.value);
            }
        })
        .expect("decode events");
    values
}

fn build(dir: &Path) -> dial9::Recorder {
    recorder(DiskBuffer::single_file(dir.join("trace.bin")).expect("writer")).build()
}

/// Stopping a recorder that has already been displaced must leave the global
/// its successor installed alone.
#[test]
fn a_displaced_recorder_does_not_clear_its_successors_global() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");

    let rec_a = build(dir_a.path());
    rec_a.install_global();

    let rec_b = build(dir_b.path());
    rec_b.install_global();

    rec_a.graceful_shutdown(Duration::ZERO);

    assert!(
        Dial9Handle::current().is_enabled(),
        "B installed last, so B still holds the global after A stops"
    );

    std::thread::spawn(|| {
        dial9::record_event(Marker {
            timestamp_ns: clock_monotonic_ns(),
            value: 7,
        });
    })
    .join()
    .expect("worker thread");

    rec_b.graceful_shutdown(Duration::ZERO);

    assert!(markers(dir_b.path()).contains(&7), "recorded into B");
    assert!(!markers(dir_a.path()).contains(&7), "and not into A");
    assert!(
        !Dial9Handle::current().is_enabled(),
        "B stopping does clear it"
    );
}
