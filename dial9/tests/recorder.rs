use dial9::core::{FlushContext, Source, clock_monotonic_ns};
use dial9::{DiskBuffer, recorder};
use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::Decoder;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, serde::Deserialize, TraceEvent)]
struct TestEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

/// Emits one `TestEvent` on its first flush.
struct OnceSource {
    emitted: bool,
}

impl Source for OnceSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        if !self.emitted {
            self.emitted = true;
            ctx.record_event(&TestEvent {
                timestamp_ns: clock_monotonic_ns(),
                value: 99,
            });
        }
    }
    fn name(&self) -> &'static str {
        "once"
    }
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

#[test]
fn facade_recorder_records_a_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");

    let recorder = recorder(writer)
        .source(OnceSource { emitted: false })
        .build_and_start();
    // The recorder exposes its sources through the public `shared()` API.
    let source_names: Vec<String> = recorder
        .shared()
        .expect("enabled recorder")
        .with_sources_mut(|sources| sources.iter().map(|s| s.name().to_string()).collect())
        .expect("sources lock");
    assert!(
        source_names.iter().any(|name| name == "once"),
        "the registered source should be visible on the recorder"
    );
    recorder
        .graceful_shutdown(Duration::ZERO)
        .expect("graceful shutdown");

    let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
    let mut decoder = Decoder::new(&bytes).expect("valid trace header");
    let mut values = Vec::new();
    decoder
        .for_each_event(|raw| {
            if raw.name == "TestEvent" {
                let event: TestEvent = raw.deserialize().expect("TestEvent decodes");
                values.push(event.value);
            }
        })
        .expect("decode events");

    assert!(
        values.contains(&99),
        "the source's event should round-trip through dial9::recorder"
    );
}
