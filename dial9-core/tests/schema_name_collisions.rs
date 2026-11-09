#![cfg(feature = "test-util")]

use dial9_core::buffer::DiskBuffer;
use dial9_core::recorder::recorder;
use dial9_trace_format::decoder::Decoder;
use std::time::Duration;

#[derive(Debug, serde::Deserialize)]
struct Decoded {
    x: Option<u64>,
    y: Option<u64>,
    z: Option<u64>,
}

mod a {
    #[derive(dial9_trace_format::TraceEvent)]
    pub struct Ev {
        #[traceevent(timestamp)]
        pub ts: u64,
        pub x: u64,
    }
}

mod b {
    #[derive(dial9_trace_format::TraceEvent)]
    pub struct Ev {
        #[traceevent(timestamp)]
        pub ts: u64,
        pub y: u64,
        pub z: u64,
    }
}

#[test]
fn same_named_events_do_not_poison_the_recording_thread() {
    let dir = tempfile::tempdir().unwrap();
    let rec = recorder(DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap()).build();
    let handle = rec.handle().clone();
    std::thread::spawn(move || {
        handle.with_encoder(|enc| {
            enc.encode(&a::Ev { ts: 1, x: 10 });
            enc.encode(&b::Ev {
                ts: 2,
                y: 20,
                z: 30,
            });
        });
        handle.with_encoder(|enc| enc.encode(&a::Ev { ts: 3, x: 40 }));
    })
    .join()
    .expect("same-named events must not panic or poison the thread-local buffer");
    rec.graceful_shutdown(Duration::ZERO);

    let bytes = std::fs::read(dir.path().join("trace.0.bin")).unwrap();
    let mut decoder = Decoder::new(&bytes).unwrap();
    let mut events = Vec::new();
    decoder
        .for_each_event(|event| {
            if event.name == "Ev" {
                events.push(event.deserialize::<Decoded>().unwrap());
            }
        })
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].x, Some(10));
    assert_eq!(events[1].y, Some(20));
    assert_eq!(events[1].z, Some(30));
    assert_eq!(events[2].x, Some(40), "later events must still be recorded");
}
