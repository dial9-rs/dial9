use dial9::core::clock_monotonic_ns;
use dial9::{Dial9Handle, DiskBuffer, recorder};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::Decoder;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, serde::Deserialize, TraceEvent)]
struct PlainThreadEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

fn sealed_segment(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .expect("trace dir readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.ends_with(".bin") && !name.ends_with(".active")
        })
        .expect("a sealed .bin segment")
}

/// Record one event from a thread no runtime owns, reporting whether the global
/// was live there and whether `current` stayed inert.
fn record_from_plain_thread(value: u64) -> (bool, bool) {
    std::thread::spawn(move || {
        let global_live = Dial9Handle::global().is_enabled();
        let current_live = Dial9Handle::current().is_enabled();
        dial9::record_event(PlainThreadEvent {
            timestamp_ns: clock_monotonic_ns(),
            value,
        });
        (global_live, current_live)
    })
    .join()
    .expect("worker thread")
}

#[test]
fn global_handle_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");
    let recorder = recorder(writer).build();

    let (global_live, _) = record_from_plain_thread(1);
    assert!(
        !global_live,
        "opt-in: nothing resolves until install_global"
    );

    recorder.install_global();

    let (global_live, current_live) = record_from_plain_thread(2);
    assert!(
        global_live,
        "after install_global the handle should resolve on an unowned thread"
    );
    assert!(
        !current_live,
        "`current` stays thread-local: installing a global must not change it"
    );

    recorder.graceful_shutdown(Duration::ZERO);

    let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace header");
    let mut values = Vec::new();
    decoder
        .for_each_event(|raw| {
            if raw.name == "PlainThreadEvent" {
                let event: PlainThreadEvent = raw.deserialize().expect("decodes");
                values.push(event.value);
            }
        })
        .expect("decode events");

    assert!(
        !values.contains(&1),
        "recorded before opting in, so dropped"
    );
    assert!(values.contains(&2), "recorded after opting in, so kept");

    assert!(
        !Dial9Handle::global().is_enabled(),
        "the global should not outlive its recorder"
    );
}
