//! Tests for the ad-hoc span API.
//!
//! The round-trip tests build a real traced runtime, instrument work, then read
//! the trace back and assert the emitted span events.

#[test]
fn span_id_is_in_adhoc_namespace() {
    // Ad-hoc span ids set the top bit so they never collide with tracing span
    // ids (which are small and assigned per-subscriber).
    let span = dial9_span!("a");
    assert!(span.id() & (1 << 63) != 0, "top bit must be set");
    let span2 = dial9_span!("b");
    assert_ne!(span.id(), span2.id(), "ids must be unique");
}

#[test]
fn macro_accepts_fields_with_sigils() {
    #[derive(Debug)]
    struct Req;
    // Should compile and not panic when telemetry is disabled on this thread.
    let span = dial9_span!(
        "handle",
        bare = 1u32,
        displayed = %"path",
        debugged = ?Req,
    );
    // Values are formatted eagerly, in field order.
    assert_eq!(span.field_values, vec!["1", "path", "Req"]);
}

#[test]
fn derive_name_attribute_sets_event_name() {
    // The generated span structs rely on `#[traceevent(name = ...)]` to get a
    // wire name the viewer recognizes ("SpanEnter:" / "SpanExit:").
    #[derive(dial9_trace_format::TraceEvent)]
    #[traceevent(name = "SpanEnter:custom")]
    #[allow(dead_code)]
    struct Generated {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        v: u64,
    }
    assert_eq!(
        <Generated as dial9_trace_format::TraceEvent>::event_name(),
        "SpanEnter:custom"
    );
}

#[test]
fn disabled_thread_is_a_noop() {
    // No dial9 runtime on this thread: entering, exiting, and dropping must all
    // be harmless no-ops.
    let span = dial9_span!("orphan", k = "v");
    {
        let _entered = span.enter();
    }
    drop(span);
}

mod roundtrip {
    use crate::span::Instrument;
    use dial9_tokio_telemetry::telemetry::analysis::TraceReader;
    use dial9_tokio_telemetry::telemetry::analysis_events::{CustomEvent, Dial9Event};
    use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
    use dial9_trace_format::FieldValue;
    use tempfile::TempDir;

    /// Run `body` on a current-thread traced runtime, then return all custom
    /// (span) events read back from the trace file.
    ///
    /// Dropping the guard drains the calling thread's buffer (where these span
    /// events live, since the calling thread is the current-thread runtime's
    /// worker) and finalizes the trace to disk.
    fn capture(body: impl FnOnce(&tokio::runtime::Runtime)) -> Vec<CustomEvent> {
        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        let (runtime, guard) = TracedRuntime::build_and_start(
            tokio::runtime::Builder::new_current_thread(),
            DiskWriter::single_file(&trace_path).unwrap(),
        )
        .unwrap();

        body(&runtime);
        drop(guard);

        let sealed = dir.path().join("trace.0.bin");
        let reader = TraceReader::new(sealed.to_str().unwrap()).unwrap();
        reader
            .all_events
            .into_iter()
            .filter_map(|e| match e {
                Dial9Event::Custom(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// Like [`capture`], but returns the raw trace bytes so tests can inspect
    /// the on-wire schema frames (field types), not just decoded values.
    fn capture_bytes(body: impl FnOnce(&tokio::runtime::Runtime)) -> Vec<u8> {
        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        let (runtime, guard) = TracedRuntime::build_and_start(
            tokio::runtime::Builder::new_current_thread(),
            DiskWriter::single_file(&trace_path).unwrap(),
        )
        .unwrap();

        body(&runtime);
        drop(guard);

        std::fs::read(dir.path().join("trace.0.bin")).unwrap()
    }

    fn field_str(ev: &CustomEvent, key: &str) -> Option<String> {
        match ev.fields.get(key) {
            Some(FieldValue::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    #[test]
    fn macro_inline_field_is_not_pooled() {
        use dial9_trace_format::decoder::{DecodedFrame, Decoder};
        use dial9_trace_format::types::FieldType;

        let bytes = capture_bytes(|rt| {
            rt.block_on(async {
                // `route` interned (default), `id` inline via `~`.
                let span = dial9_span!("req", route = "/users", id = ~"req-xyz");
                let _e = span.enter();
            });
        });

        let mut dec = Decoder::new(&bytes).unwrap();
        let frames = dec.decode_all();
        let enter = frames
            .iter()
            .find_map(|f| match f {
                DecodedFrame::Schema(s) if s.name().starts_with("SpanEnter:") => Some(s),
                _ => None,
            })
            .expect("SpanEnter schema present");

        let ty = |name: &str| {
            enter
                .fields()
                .iter()
                .find(|f| f.name() == name)
                .unwrap_or_else(|| panic!("field {name} present"))
                .field_type()
        };
        assert_eq!(ty("route"), FieldType::PooledString, "interned -> pooled");
        assert_eq!(ty("id"), FieldType::String, "inline (~) -> inline string");
    }

    #[test]
    fn distinct_field_sets_across_callsites_do_not_collide() {
        // Regression: every `dial9_span!` enter/exit schema is named uniquely per
        // call site, so two call sites with *different* field sets register
        // distinct schemas instead of colliding on a shared name (which the
        // encoder rejects, panicking the program).
        let events = capture(|rt| {
            rt.block_on(async {
                let s1 = dial9_span!("a", x = 1u32);
                let _e1 = s1.enter();
                let s2 = dial9_span!("b", y = 2u32, z = 3u32);
                let _e2 = s2.enter();
            });
        });

        let enters = events
            .iter()
            .filter(|e| e.name.starts_with("SpanEnter:"))
            .count();
        assert_eq!(
            enters, 2,
            "both distinct-field call sites must emit: {events:#?}"
        );
    }

    #[test]
    fn instrumented_future_emits_enter_exit_close() {
        let events = capture(|rt| {
            rt.block_on(async {
                async {
                    // Two polls (yield) → two enter/exit segments.
                    tokio::task::yield_now().await;
                }
                .instrument(dial9_span!("handle_request", request_id = 7u32))
                .await;
            });
        });

        let enters: Vec<_> = events
            .iter()
            .filter(|e| e.name.starts_with("SpanEnter:"))
            .collect();
        let exits: Vec<_> = events
            .iter()
            .filter(|e| e.name.starts_with("SpanExit:"))
            .collect();
        let closes: Vec<_> = events
            .iter()
            .filter(|e| e.name == "SpanCloseEvent")
            .collect();

        assert!(
            enters.len() >= 2,
            "expected >=2 enter segments, got {}: {events:#?}",
            enters.len()
        );
        assert_eq!(enters.len(), exits.len(), "enters and exits must pair");
        assert_eq!(closes.len(), 1, "exactly one close per span");

        // Field plumbing: span_name and the user field round-trip.
        let enter = enters[0];
        assert_eq!(
            field_str(enter, "span_name").as_deref(),
            Some("handle_request")
        );
        assert_eq!(field_str(enter, "request_id").as_deref(), Some("7"));
        assert!(enter.fields.contains_key("worker_id"));
        assert!(enter.fields.contains_key("span_id"));

        // The single close references the same span id as the enters.
        let span_id = enter.fields.get("span_id").cloned();
        assert_eq!(closes[0].fields.get("span_id").cloned(), span_id);
    }

    #[test]
    fn sync_enter_guard_emits_one_segment() {
        let events = capture(|rt| {
            rt.block_on(async {
                let span = dial9_span!("compute");
                let _entered = span.enter();
            });
        });

        let enters = events
            .iter()
            .filter(|e| e.name.starts_with("SpanEnter:"))
            .count();
        let exits = events
            .iter()
            .filter(|e| e.name.starts_with("SpanExit:"))
            .count();
        assert_eq!(enters, 1);
        assert_eq!(exits, 1);
    }

    #[test]
    fn parent_link_is_recorded() {
        let events = capture(|rt| {
            rt.block_on(async {
                let parent = dial9_span!("parent");
                let parent_id = parent.id();
                let child = dial9_span!("child").with_parent(&parent);
                {
                    let _p = parent.enter();
                    let _c = child.enter();
                }
                // keep parent_id referenced
                let _ = parent_id;
            });
        });

        let child_enter = events
            .iter()
            .filter(|e| e.name.starts_with("SpanEnter:"))
            .find(|e| field_str(e, "span_name").as_deref() == Some("child"))
            .expect("child enter present");
        assert!(
            matches!(
                child_enter.fields.get("parent_span_id"),
                Some(FieldValue::Varint(_))
            ),
            "child must carry a parent_span_id: {child_enter:#?}"
        );
    }

    #[cfg(feature = "tower-layer")]
    #[test]
    fn tower_layer_instruments_each_request() {
        use crate::span::Dial9SpanLayer;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tower_layer::Layer;
        use tower_service::Service;

        // Minimal echo service.
        #[derive(Clone)]
        struct Echo;
        impl Service<u32> for Echo {
            type Response = u32;
            type Error = ();
            type Future = Pin<Box<dyn Future<Output = Result<u32, ()>> + Send>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, req: u32) -> Self::Future {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    Ok(req)
                })
            }
        }

        let events = capture(|rt| {
            rt.block_on(async {
                let mut svc = Dial9SpanLayer::named("rpc").layer(Echo);
                assert_eq!(svc.call(1).await, Ok(1));
            });
        });

        assert!(
            events.iter().any(|e| {
                e.name.starts_with("SpanEnter:")
                    && field_str(e, "span_name").as_deref() == Some("rpc")
            }),
            "expected an 'rpc' span enter from the tower layer: {events:#?}"
        );
    }
}
