mod builder;
mod flush_loop;
mod guard;
mod handle;
mod runtime_context;
mod shared_state;
pub(crate) mod source;

pub(crate) use runtime_context::RuntimeContext;
pub use runtime_context::current_worker_id;
pub(crate) use runtime_context::poll_start_ts_monotonic;
pub(crate) use shared_state::SharedState;

pub use builder::{
    HasTracePath, NoTracePath, PipelineCustom, PipelineS3, PipelineUnset, TelemetryCore,
    TelemetryCoreBuilder, TelemetryRuntimeError, TracedRuntime, TracedRuntimeBuilder,
};
pub use guard::{TelemetryGuard, TraceRuntimeCoreBuilder};
pub use handle::{RuntimeTelemetryHandle, TelemetryHandle, spawn};

pub(crate) use flush_loop::FlushStats;

// Re-exports for internal test access
#[cfg(test)]
use builder::PipelineConfig;
#[cfg(test)]
use handle::InstrumentedSpawnGuard;

use handle::{CURRENT_HANDLE, INSTRUMENTED_SPAWN};
use runtime_context::{make_poll_end, make_poll_start, make_worker_park, make_worker_unpark};

use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::Ordering;
use crate::rate_limit::rate_limited;
use crate::telemetry::format::TaskTerminateEvent;
use crate::telemetry::task_metadata::TaskId;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Channel-based control for the flush thread
// ---------------------------------------------------------------------------

/// Commands sent to the flush thread from TelemetryHandle / TelemetryGuard.
pub(crate) enum ControlCommand {
    /// Flush, finalize (seal segment), then exit the thread.
    FinalizeAndStop(crate::primitives::sync::mpsc::SyncSender<()>),
}

/// Register telemetry callbacks on a runtime builder.
/// Closures capture `Arc<RuntimeContext>` (runtime-specific) and `Arc<SharedState>` (recording core).
///
/// # Worker ID resolution
///
/// `WORKER_ID` TLS is populated lazily on the first `on_thread_unpark` / `on_before_task_poll`
/// call via [`resolve_worker_id`](runtime_context::resolve_worker_id), not in `on_thread_start`.
/// This is intentional: `on_thread_start` fires before `RuntimeMetrics` is available, so we
/// cannot yet call `metrics.worker_thread_id(i)` to determine which worker index we are.
/// By the time any waker calls `current_worker_id()`, at least one unpark or poll has occurred
/// and TLS is guaranteed to be populated.
fn register_hooks(
    builder: &mut tokio::runtime::Builder,
    ctx: &Arc<RuntimeContext>,
    shared: &Arc<SharedState>,
    control_tx: &crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    task_tracking_enabled: bool,
) {
    // TODO: these should rely on public APIs instead of utilizing `SharedState`

    let c1 = ctx.clone();
    let s1 = shared.clone();
    let c2 = ctx.clone();
    let s2 = shared.clone();
    let c3 = ctx.clone();
    let s3 = shared.clone();
    let c4 = ctx.clone();
    let s4 = shared.clone();

    builder
        .on_thread_park(move || {
            s1.if_enabled(|buf| {
                let event = make_worker_park(&c1, &s1);
                buf.record_encodable_event(&event);
            });
        })
        .on_thread_unpark(move || {
            s2.if_enabled(|buf| {
                let event = make_worker_unpark(&c2, &s2);
                buf.record_encodable_event(&event);
            });
        })
        .on_before_task_poll(move |meta| {
            s3.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let event = make_poll_start(&c3, &s3, location, task_id);
                buf.record_encodable_event(&event);
            });
        })
        .on_after_task_poll(move |_meta| {
            s4.if_enabled(|buf| {
                let event = make_poll_end(&c4, &s4);
                buf.record_encodable_event(&event);
            });
        });

    if task_tracking_enabled {
        let s5 = shared.clone();
        builder.on_task_spawn(move |meta| {
            s5.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let instrumented = INSTRUMENTED_SPAWN.with(|f| f.get()) > 0;
                let timestamp_ns = crate::telemetry::events::clock_monotonic_ns();
                buf.record_encodable_event(&runtime_context::TaskSpawn {
                    timestamp_ns,
                    task_id,
                    location,
                    instrumented,
                });
            });
        });
        let s6 = shared.clone();
        builder.on_task_terminate(move |meta| {
            s6.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                buf.record_encodable_event(&TaskTerminateEvent {
                    timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
                    task_id,
                });
            });
        });
    }

    // Unified on_thread_start / on_thread_stop. Tokio only stores one
    // callback per hook, so any feature-gated work must live here rather
    // than registering its own hook.
    let handle_for_tl = TelemetryHandle::enabled(shared.clone(), control_tx.clone());
    #[cfg(feature = "cpu-profiling")]
    let s_start = shared.clone();
    #[cfg(feature = "cpu-profiling")]
    let s_stop = shared.clone();

    builder
        .on_thread_start(move || {
            // Install this thread's TelemetryHandle so user code can call
            // `TelemetryHandle::current()` from anywhere on this thread.
            CURRENT_HANDLE.with(|cell| {
                *cell.borrow_mut() = Some(handle_for_tl.clone());
            });

            #[cfg(feature = "cpu-profiling")]
            {
                // Register as Blocking initially; worker threads will
                // overwrite this to Worker(i) in resolve_worker_id.
                // NOTE: `tokio::runtime::worker_index()` will always return `None` at this point
                // so we can't utilize that here.
                let tid = crate::telemetry::events::current_tid();
                s_start
                    .thread_roles
                    .lock()
                    .unwrap()
                    .insert(tid, crate::telemetry::events::ThreadRole::Blocking);
                // Sched event sampling is deferred to register_tid_if_needed(),
                // which runs only for worker threads on their first poll/park.
                // This avoids opening perf fds for blocking pool threads.

                // Registers the current thread for the CPU-profiling fallback (ctimer).
                // No-op when perf is the active backend (perf uses inherit).
                let _ = dial9_perf_self_profile::register_current_thread();
            }
        })
        .on_thread_stop(move || {
            CURRENT_HANDLE.with(|cell| {
                *cell.borrow_mut() = None;
            });

            #[cfg(feature = "cpu-profiling")]
            {
                let tid = crate::telemetry::events::current_tid();
                s_stop.thread_roles.lock().unwrap().remove(&tid);
                if let Ok(mut sources) = s_stop.sources.lock() {
                    for source in sources.iter_mut() {
                        source.on_thread_stop();
                    }
                }
                dial9_perf_self_profile::unregister_current_thread();
            }
        });
}

/// Attach a runtime to an existing telemetry session: register hooks, build
/// the runtime, reserve worker IDs, and push the context.
fn attach_runtime(
    shared: &Arc<SharedState>,
    mut builder: tokio::runtime::Builder,
    runtime_name: Option<String>,
    control_tx: &crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    task_tracking_enabled: bool,
) -> std::io::Result<tokio::runtime::Runtime> {
    let ctx = Arc::new(RuntimeContext::new(runtime_name));
    register_hooks(
        &mut builder,
        &ctx,
        shared,
        control_tx,
        task_tracking_enabled,
    );

    let runtime = builder.build()?;

    // Install the handle on the calling thread. For current_thread runtimes,
    // this thread IS the worker (block_on runs here), so the tracing layer
    // needs CURRENT_HANDLE to be set. Harmless for multi_thread runtimes.
    CURRENT_HANDLE.with(|cell| {
        *cell.borrow_mut() = Some(TelemetryHandle::enabled(shared.clone(), control_tx.clone()));
    });

    // Pre-reserve a contiguous block of worker IDs and set metrics atomically.
    let metrics = runtime.handle().metrics();
    let num_workers = metrics.num_workers() as u64;
    let base = shared
        .next_worker_id
        .fetch_add(num_workers, Ordering::Relaxed);
    ctx.metrics_and_base
        .set((metrics, base))
        .unwrap_or_else(|_| {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    "metrics_and_base already set for runtime context; ignoring duplicate attach"
                );
            });
        });

    // Eagerly populate worker_ids so segment metadata is complete from the
    // first flush cycle, rather than waiting for each worker thread to lazily
    // register on its first poll/park event.
    {
        let mut ids = ctx.worker_ids.write().unwrap();
        for i in 0..num_workers {
            ids.insert(i as usize, base + i);
        }
    }

    shared.contexts.lock().unwrap().push(ctx);

    Ok(runtime)
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;
    use crate::telemetry::NullWriter;
    use crate::telemetry::buffer;
    use crate::telemetry::collector::CentralCollector;
    use crate::telemetry::writer::RotatingWriter;
    use std::panic::Location;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Drain all pending batches from a `CentralCollector` into a writer.
    /// Call `buffer::drain_to_collector` first to flush the thread-local buffer.
    fn drain_collector_to_writer(
        collector: &CentralCollector,
        writer: &mut dyn crate::telemetry::writer::TraceWriter,
    ) {
        while let Some(batch) = collector.next() {
            if batch.event_count > 0 {
                writer.write_encoded_batch(&batch).unwrap();
            }
        }
    }

    /// Writer that captures encoded bytes for test assertions.
    struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl crate::telemetry::writer::TraceWriter for CapturingWriter {
        fn write_encoded_batch(
            &mut self,
            batch: &crate::telemetry::collector::Batch,
        ) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .extend_from_slice(batch.encoded_bytes());
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Nested `InstrumentedSpawnGuard`s must compose: inner drop must not
    /// clear the outer scope. Counter, not flag.
    #[test]
    fn instrumented_spawn_guard_nests() {
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 0);
        let outer = InstrumentedSpawnGuard::enter();
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 1);
        {
            let _inner = InstrumentedSpawnGuard::enter();
            assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 2);
        }
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 1);
        drop(outer);
        assert_eq!(INSTRUMENTED_SPAWN.with(|c| c.get()), 0);
    }

    #[test]
    fn current_thread_runtime_resolves_worker_ids() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();

        let (rt, guard) = TracedRuntime::builder()
            .build_and_start_with_writer(builder, CapturingWriter(data.clone()))
            .unwrap();

        rt.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        drop(rt);
        drop(guard);

        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let poll_starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. } => {
                    Some(*worker_id)
                }
                _ => None,
            })
            .collect();
        assert!(!poll_starts.is_empty(), "expected at least one PollStart");
        let unknown: Vec<_> = poll_starts
            .iter()
            .filter(|id| **id == crate::telemetry::format::WorkerId::UNKNOWN)
            .collect();
        assert!(
            unknown.is_empty(),
            "all PollStart events should have a known worker ID, \
             but {}/{} were UNKNOWN",
            unknown.len(),
            poll_starts.len()
        );
    }

    #[test]
    fn test_shared_state_no_spawn_location_fields() {
        let _shared = SharedState::new(crate::telemetry::events::clock_monotonic_ns(), None);
    }

    #[test]
    fn build_disabled_produces_working_runtime_with_noop_guard() {
        let builder = tokio::runtime::Builder::new_multi_thread();
        let (runtime, guard) = TracedRuntime::builder()
            .install(false)
            .build(builder, NullWriter)
            .unwrap();

        // Guard methods should be safe no-ops
        guard.enable();
        guard.disable();
        let handle = guard.handle();
        let _start = guard.start_time();

        // Runtime should work normally, including handle.spawn
        runtime.block_on(async {
            let result = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(result, 42);

            let traced = handle.spawn(async { 7 }).await.unwrap();
            assert_eq!(traced, 7);
        });

        // No flush thread or worker to join — the guard is in its
        // disabled state.
        assert!(!guard.is_enabled());
    }

    #[test]
    #[cfg(feature = "analysis")]
    fn test_spawn_locations_resolve_after_rotation() {
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::format::WorkerId;

        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("trace");

        #[track_caller]
        fn loc_a() -> &'static Location<'static> {
            Location::caller()
        }
        #[track_caller]
        fn loc_b() -> &'static Location<'static> {
            Location::caller()
        }
        let location_a = loc_a();
        let location_b = loc_b();

        let writer = crate::telemetry::writer::RotatingWriter::builder()
            .base_path(&base)
            .max_file_size(100)
            .max_total_size(100_000)
            .build()
            .unwrap();
        let mut ew: Box<dyn crate::telemetry::writer::TraceWriter> = Box::new(writer);
        let collector = Arc::new(CentralCollector::new());
        let drain_epoch = AtomicU64::new(0);

        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            let ts = (i as u64 + 1) * 1000;
            buffer::with_encoder(
                |enc| {
                    let spawn_loc = enc.intern_location(loc);
                    enc.encode(&crate::telemetry::format::TaskSpawnEvent {
                        timestamp_ns: ts,
                        task_id,
                        spawn_loc,
                        instrumented: true,
                    });
                },
                &collector,
                &drain_epoch,
            );
            buffer::with_encoder(
                |enc| {
                    let spawn_loc = enc.intern_location(loc);
                    enc.encode(&crate::telemetry::format::PollStartEvent {
                        timestamp_ns: ts,
                        worker_id: WorkerId::from(0usize),
                        local_queue: 0,
                        task_id,
                        spawn_loc,
                    });
                },
                &collector,
                &drain_epoch,
            );
            // Drain after each iteration to produce separate small batches
            // that trigger file rotation (max_file_size is 100 bytes).
            buffer::drain_to_collector(&collector);
            drain_collector_to_writer(&collector, &mut *ew);
        }
        ew.flush().unwrap();
        ew.finalize().unwrap();

        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        assert!(
            files.len() > 1,
            "expected multiple files from rotation, got {}",
            files.len()
        );

        let mut total_events = 0;
        for file in &files {
            let path = file.to_str().unwrap();
            let reader = TraceReader::new(path).unwrap();

            for (spawn_loc, loc) in &reader.spawn_locations {
                assert!(
                    loc.contains(':'),
                    "location should be file:line:col, got {loc:?} for {spawn_loc:?}"
                );
            }

            for (task_id, spawn_loc) in &reader.task_spawn_locs {
                reader.spawn_locations.get(spawn_loc).unwrap_or_else(|| {
                    panic!(
                        "file {path:?}: task {task_id:?} spawn_loc {spawn_loc:?} has no definition"
                    )
                });
            }

            let events = &reader.runtime_events;
            total_events += events.len();
        }
        assert_eq!(
            total_events, 6,
            "all PollStart events should be readable across files"
        );
    }

    #[test]
    fn build_and_attach_to_telemetry_attaches_second_runtime() {
        let builder_a = tokio::runtime::Builder::new_multi_thread();
        let (runtime_a, guard) = TracedRuntime::builder()
            .build_and_start_with_writer(builder_a, NullWriter)
            .unwrap();

        let builder_b = tokio::runtime::Builder::new_multi_thread();
        let runtime_b = TracedRuntime::builder()
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Both runtimes should work
        runtime_a.block_on(async {
            let r = tokio::spawn(async { 1 }).await.unwrap();
            assert_eq!(r, 1);
        });
        runtime_b.block_on(async {
            let r = tokio::spawn(async { 2 }).await.unwrap();
            assert_eq!(r, 2);
        });
    }

    #[test]
    fn build_and_attach_to_telemetry_produces_unique_worker_ids() {
        use crate::telemetry::format::WorkerId;
        use std::collections::HashSet;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_start_with_writer(builder_a, CapturingWriter(data.clone()))
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Generate poll events on both runtimes. Spawn many concurrent tasks
        // to ensure work lands on actual worker threads (not just block_on's thread).
        runtime_a.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(tokio::spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });
        runtime_b.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(tokio::spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });

        // Drop runtimes, then guard to flush
        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in captured.iter() {
            match event {
                crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::PollEnd { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::WorkerPark { worker_id, .. }
                | crate::telemetry::events::TelemetryEvent::WorkerUnpark { worker_id, .. }
                    if *worker_id != WorkerId::UNKNOWN =>
                {
                    worker_ids.insert(worker_id.as_u64());
                }
                _ => {}
            }
        }

        // Runtime A has 2 workers → IDs 0,1. Runtime B → IDs 2,3.
        // We should see at least one ID from each runtime's range.
        let has_runtime_a = worker_ids.iter().any(|&id| id < 2);
        let has_runtime_b = worker_ids.iter().any(|&id| (2..4).contains(&id));
        assert!(
            has_runtime_a && has_runtime_b,
            "expected worker IDs from both runtimes (0..2 and 2..4), got: {worker_ids:?}"
        );
    }

    /// Verify that `build_and_attach_to_telemetry` propagates the second runtime's metadata
    /// (runtime name → worker ID mapping) into the trace file's segment metadata.
    #[test]
    fn build_and_attach_to_telemetry_propagates_second_runtime_metadata() {
        use crate::telemetry::events::TelemetryEvent;

        let dir = tempfile::TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        let writer = crate::telemetry::writer::RotatingWriter::builder()
            .base_path(&trace_path)
            .max_file_size(1024 * 1024)
            .max_total_size(10 * 1024 * 1024)
            .build()
            .unwrap();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_runtime_name("main")
            .with_trace_path(trace_path.to_str().unwrap())
            .build_and_start(builder_a, writer)
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_runtime_name("io")
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Run work on both runtimes so workers resolve their identities.
        for rt in [&runtime_a, &runtime_b] {
            rt.block_on(async {
                let mut handles = Vec::new();
                for _ in 0..20 {
                    handles.push(tokio::spawn(async {
                        tokio::task::yield_now().await;
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        }

        // Give the flush thread time to run (it cycles every 5ms and merges
        // runtime metadata into the writer on each cycle).
        std::thread::sleep(std::time::Duration::from_millis(50));

        drop(runtime_a);
        drop(runtime_b);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(5));

        // Read all sealed trace files and collect SegmentMetadata entries.
        let mut all_metadata: Vec<Vec<(String, String)>> = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
            .collect();
        files.sort();
        for file in &files {
            let data = std::fs::read(file).unwrap();
            let events = crate::telemetry::format::decode_events(&data).unwrap();
            for event in &events {
                if let TelemetryEvent::SegmentMetadata { entries, .. } = event {
                    all_metadata.push(entries.clone());
                }
            }
        }

        assert!(
            !all_metadata.is_empty(),
            "expected at least one SegmentMetadata event in trace files"
        );

        // At least one segment's metadata should contain both runtime mappings
        // with the exact worker IDs (eagerly populated at attach time).
        let has_both = all_metadata.iter().any(|entries| {
            let has_main = entries
                .iter()
                .any(|(k, v)| k == "runtime.main" && v == "0,1");
            let has_io = entries.iter().any(|(k, v)| k == "runtime.io" && v == "2,3");
            has_main && has_io
        });
        assert!(
            has_both,
            "expected segment metadata to contain runtime.main=0,1 and runtime.io=2,3, \
             got: {all_metadata:?}"
        );
    }

    /// Wake events from runtime B's workers must carry global worker IDs (≥ num_workers_a),
    /// not local indices that collide with runtime A's workers.
    #[test]
    fn wake_events_use_global_worker_id_in_multi_runtime() {
        use crate::telemetry::events::TelemetryEvent;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2);
        let (runtime_a, guard) = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_start_with_writer(builder_a, CapturingWriter(data.clone()))
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let runtime_b = TracedRuntime::builder()
            .with_task_tracking(true)
            .build_and_attach_to_telemetry(builder_b, &guard)
            .unwrap();

        // Use handle.spawn on runtime B to get wake-tracked wrapping → wake events.
        let handle = guard.handle();
        runtime_b.block_on(async {
            let mut handles = Vec::new();
            for _ in 0..50 {
                handles.push(handle.spawn(async {
                    tokio::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
        });

        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let wake_workers: Vec<u8> = captured
            .iter()
            .filter_map(|e| match e {
                TelemetryEvent::WakeEvent { target_worker, .. } => Some(*target_worker),
                _ => None,
            })
            .collect();
        assert!(!wake_workers.is_empty(), "expected at least one WakeEvent");

        // Runtime A has workers 0,1. Runtime B has workers 2,3.
        // Wakes issued from runtime B's workers must have target_worker >= 2.
        let has_global_id = wake_workers.iter().any(|&w| w >= 2 && w != 255);
        assert!(
            has_global_id,
            "expected wake events from runtime B to use global worker IDs (>= 2), \
             but got: {wake_workers:?}"
        );
    }

    #[cfg(all(feature = "cpu-profiling", feature = "analysis"))]
    mod rotation_proptest {
        use super::*;
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::buffer::ThreadLocalBuffer;
        use crate::telemetry::collector::Batch;
        use crate::telemetry::events::{CpuSampleData, CpuSampleSource, TelemetryEvent};
        use crate::telemetry::format::WorkerId;
        use crate::telemetry::task_metadata::TaskId;
        use crate::telemetry::writer::RotatingWriter;
        use proptest::prelude::*;

        /// Encode a single event into a batch and write it through the writer.
        fn write_raw_event(
            writer: &mut dyn crate::telemetry::writer::TraceWriter,
            event: &dyn crate::telemetry::buffer::Encodable,
        ) -> std::io::Result<()> {
            let encoded_bytes = ThreadLocalBuffer::encode_single(event);
            let batch = Batch {
                encoded_bytes,
                event_count: 1,
            };
            writer.write_encoded_batch(&batch)
        }

        #[derive(Debug, Clone)]
        enum FlushOp {
            CpuSample {
                worker_id: WorkerId,
                tid: u32,
                callchain: Vec<u64>,
            },
            PollStart {
                location_idx: usize,
            },
        }

        fn arb_flush_op() -> impl Strategy<Value = FlushOp> {
            prop_oneof![
                (
                    prop::bool::ANY,
                    0u32..4,
                    prop::collection::vec(0u64..8, 0..3),
                )
                    .prop_map(|(is_worker, tid, callchain)| {
                        FlushOp::CpuSample {
                            worker_id: if is_worker {
                                WorkerId::from(0usize)
                            } else {
                                WorkerId::UNKNOWN
                            },
                            tid,
                            callchain,
                        }
                    }),
                (0usize..3).prop_map(|idx| FlushOp::PollStart { location_idx: idx }),
            ]
        }

        #[derive(Debug, Clone)]
        struct FlushRound {
            cpu_ops: Vec<FlushOp>,
            raw_ops: Vec<FlushOp>,
        }

        fn arb_flush_round() -> impl Strategy<Value = FlushRound> {
            (
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::CpuSample { .. }))
                        .collect()
                }),
                prop::collection::vec(arb_flush_op(), 0..12).prop_map(|ops| {
                    ops.into_iter()
                        .filter(|o| matches!(o, FlushOp::PollStart { .. }))
                        .collect()
                }),
            )
                .prop_map(|(cpu_ops, raw_ops)| FlushRound { cpu_ops, raw_ops })
        }

        fn execute_flush_round(
            round: &FlushRound,
            ew: &mut Box<dyn crate::telemetry::writer::TraceWriter>,
            locations: &[&'static Location<'static>],
            timestamp: &mut u64,
            expected_raw: &mut usize,
        ) {
            for op in &round.cpu_ops {
                if let FlushOp::CpuSample {
                    worker_id,
                    tid,
                    callchain,
                } = op
                {
                    let data = CpuSampleData {
                        timestamp_nanos: *timestamp,
                        worker_id: *worker_id,
                        tid: *tid,
                        source: CpuSampleSource::CpuProfile,
                        thread_name: None,
                        callchain: callchain.clone(),
                        cpu: None,
                    };
                    *timestamp += 1;
                    write_raw_event(&mut **ew, &data).unwrap();
                }
            }

            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let ts = *timestamp;
                    *timestamp += 1;

                    write_raw_event(
                        &mut **ew,
                        &runtime_context::PollStart {
                            timestamp_ns: ts,
                            worker_id: WorkerId::from(0usize),
                            local_queue: 0,
                            task_id,
                            location: loc,
                        },
                    )
                    .unwrap();
                    *expected_raw += 1;
                }
            }
        }

        fn verify_files(dir: &std::path::Path) -> usize {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
                .collect();
            files.sort();

            let mut total_raw = 0;

            for file in &files {
                let path_str = file.to_str().unwrap();
                let reader = TraceReader::new(path_str)
                    .unwrap_or_else(|e| panic!("failed to open {path_str}: {e}"));

                // In the new format, spawn locations come from the string pool.
                // Verify every PollStart's spawn_loc_id resolves.
                let spawn_locs = &reader.spawn_locations;

                for ev in &reader.all_events {
                    match ev {
                        TelemetryEvent::PollStart { spawn_loc, .. } => {
                            assert!(
                                spawn_locs.contains_key(spawn_loc),
                                "{path_str}: PollStart references spawn_loc {spawn_loc:?} but no definition in this file. Defs: {spawn_locs:?}"
                            );
                            total_raw += 1;
                        }
                        TelemetryEvent::CpuSample { .. } => {
                            // Callchain addresses are raw; symbolization
                            // happens in the background worker now.
                        }
                        _ => {}
                    }
                }
            }
            total_raw
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn rotation_preserves_self_containedness(
                rounds in prop::collection::vec(arb_flush_round(), 1..6),
                max_file_size in 60u64..300,
            ) {
                let dir = tempfile::TempDir::new().unwrap();
                let base = dir.path().join("trace");

                let writer = RotatingWriter::builder()
                    .base_path(&base)
                    .max_file_size(max_file_size)
                    .max_total_size(1_000_000)
                    .build()
                    .unwrap();

                let mut ew: Box<dyn crate::telemetry::writer::TraceWriter> = Box::new(writer);

                #[track_caller]
                fn loc0() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc1() -> &'static Location<'static> { Location::caller() }
                #[track_caller]
                fn loc2() -> &'static Location<'static> { Location::caller() }
                let locations: Vec<&'static Location<'static>> = vec![loc0(), loc1(), loc2()];

                let mut timestamp = 1u64;
                let mut expected_raw = 0usize;

                for round in &rounds {
                    execute_flush_round(
                        round,
                        &mut ew,
                        &locations,
                        &mut timestamp,
                        &mut expected_raw,
                    );
                }
                ew.flush().unwrap();
                ew.finalize().unwrap();

                let actual_raw = verify_files(dir.path());

                prop_assert_eq!(
                    actual_raw, expected_raw,
                    "raw event count mismatch: expected {}, got {}", expected_raw, actual_raw
                );
            }
        }
    }

    #[test]
    fn telemetry_core_builds_guard_without_runtime() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        assert!(guard.is_enabled());
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    #[test]
    fn telemetry_core_trace_runtime_produces_working_runtime() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard.trace_runtime("main").build(builder).unwrap();

        runtime.block_on(async {
            let r = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(r, 42);
        });

        drop(runtime);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    #[test]
    fn telemetry_core_task_tracking_produces_task_spawn_events() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard
            .trace_runtime("main")
            .task_tracking(true)
            .build(builder)
            .unwrap();

        runtime.block_on(async {
            tokio::spawn(async { tokio::task::yield_now().await })
                .await
                .unwrap();
        });

        drop(runtime);
        drop(guard);

        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let spawn_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::events::TelemetryEvent::TaskSpawn { .. }
                )
            })
            .count();
        assert!(
            spawn_count > 0,
            "expected TaskSpawn events when task_tracking is enabled, got none"
        );
    }

    #[test]
    fn telemetry_core_trace_runtime_multiple_runtimes_unique_worker_ids() {
        use crate::telemetry::format::WorkerId;
        use std::collections::HashSet;

        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2).enable_all();
        let (runtime_a, _handle_a) = guard
            .trace_runtime("main")
            .task_tracking(true)
            .build(builder_a)
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2).enable_all();
        let (runtime_b, _handle_b) = guard
            .trace_runtime("io")
            .task_tracking(true)
            .build(builder_b)
            .unwrap();

        for rt in [&runtime_a, &runtime_b] {
            rt.block_on(async {
                let mut handles = Vec::new();
                for _ in 0..50 {
                    handles.push(tokio::spawn(async {
                        tokio::task::yield_now().await;
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        }

        drop(runtime_a);
        drop(runtime_b);
        drop(guard);

        let raw = data.lock().unwrap();
        let captured = crate::telemetry::format::decode_events(&raw).unwrap();
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in &captured {
            if let crate::telemetry::events::TelemetryEvent::PollStart { worker_id, .. } = event
                && *worker_id != WorkerId::UNKNOWN
            {
                worker_ids.insert(worker_id.as_u64());
            }
        }

        let has_runtime_a = worker_ids.iter().any(|&id| id < 2);
        let has_runtime_b = worker_ids.iter().any(|&id| (2..4).contains(&id));
        assert!(
            has_runtime_a && has_runtime_b,
            "expected worker IDs from both runtimes, got: {worker_ids:?}"
        );
    }

    #[test]
    fn trace_runtime_build_returns_telemetry_handle() {
        let data = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let guard = TelemetryCore::builder()
            .writer(CapturingWriter(data.clone()))
            .build()
            .unwrap();
        guard.enable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, handle) = guard.trace_runtime("main").build(builder).unwrap();

        runtime.block_on(async {
            // handle.spawn wraps the future with wake tracking;
            // yield_now triggers a wake so we can verify it's recorded.
            let result = handle
                .spawn(async {
                    tokio::task::yield_now().await;
                    42
                })
                .await
                .unwrap();
            assert_eq!(result, 42);
        });

        // Drain thread-local buffers before shutdown.
        crate::telemetry::buffer::drain_to_collector(
            &guard
                .handle()
                .traced_handle()
                .expect("enabled handle must yield a TracedHandle")
                .shared
                .collector,
        );

        drop(runtime);
        drop(guard);

        // Verify wake events were recorded (handle.spawn wraps with wake tracking)
        let raw = data.lock().unwrap();
        let events = crate::telemetry::format::decode_events(&raw).unwrap();
        let wake_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::events::TelemetryEvent::WakeEvent { .. }
                )
            })
            .count();
        assert!(
            wake_count > 0,
            "expected WakeEvent from handle.spawn(), got none"
        );
    }

    /// The handle returned by `trace_runtime().build()` must spawn on the
    /// correct runtime even when called from outside any runtime context.
    #[test]
    fn trace_runtime_handle_spawns_on_correct_runtime_from_outside() {
        let guard = TelemetryCore::builder().writer(NullWriter).build().unwrap();
        guard.enable();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(1).enable_all().thread_name("rt-a");
        let (rt_a, handle_a) = guard.trace_runtime("a").build(builder_a).unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(1).enable_all().thread_name("rt-b");
        let (rt_b, handle_b) = guard.trace_runtime("b").build(builder_b).unwrap();

        // Spawn from outside any runtime context — should target the correct runtime.
        let join_a = handle_a.spawn(async {
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        });
        let join_b = handle_b.spawn(async {
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        });

        let name_a = rt_a.block_on(join_a).unwrap();
        let name_b = rt_b.block_on(join_b).unwrap();

        assert!(
            name_a.starts_with("rt-a"),
            "expected task to run on rt-a, got: {name_a}"
        );
        assert!(
            name_b.starts_with("rt-b"),
            "expected task to run on rt-b, got: {name_b}"
        );

        drop(rt_a);
        drop(rt_b);
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    // ---------------------------------------------------------------
    // High-level construction tests (TracedRuntime::new / try_new)
    // ---------------------------------------------------------------

    fn dial9_config_tmp_base_path() -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn try_new_enabled_path_returns_value_and_exposes_guard() {
        let cfg = crate::Dial9Config::builder()
            .base_path(dial9_config_tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .build()
            .expect("strict build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "enabled config must install a live guard"
        );
        // Smoke-test the runtime accessor — exists and is usable.
        let _ = rt.runtime().handle();
        let value = rt.block_on(async { 5u32 });
        assert_eq!(value, 5);
    }

    #[test]
    fn try_new_disabled_path_returns_value_no_guard() {
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "disabled config must yield an inert guard"
        );
        let value = rt.block_on(async { 11u32 });
        assert_eq!(value, 11);
    }

    #[test]
    fn new_returns_runtime_for_valid_disabled_config() {
        // Happy-path counterpart to the strict-I/O panic story: when the
        // config is valid `TracedRuntime::new` returns a usable runtime
        // without panicking. The matching panic path is covered by hand at
        // the type level — `new` is a thin wrapper around `try_into()` that
        // calls `unwrap_or_else(|e| panic!(...))`, and the surrounding
        // tests assert that the inner `TelemetryRuntimeError` formats
        // through `Display` correctly.
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::new(cfg);
        let value = rt.block_on(async { 13u32 });
        assert_eq!(value, 13);
    }

    #[test]
    fn telemetry_runtime_error_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = TelemetryRuntimeError::TelemetryCore(inner);
        let display = format!("{err}");
        assert!(
            display.contains("telemetry core:"),
            "Display should label the variant, got: {display}"
        );
        assert!(
            display.contains("boom"),
            "Display should include the inner io::Error message, got: {display}"
        );
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "source() must return the inner io::Error");
    }

    // ---------------------------------------------------------------
    // Always-present TelemetryGuard / inert TelemetryHandle (Phase 3)
    // ---------------------------------------------------------------

    /// Off-runtime callers should get a usable, inert handle rather
    /// than a panic.
    #[test]
    fn telemetry_handle_current_off_runtime_returns_inert_handle() {
        // We're on the test thread, which is not owned by any dial9
        // runtime. `current()` used to panic here.
        let handle = TelemetryHandle::current();
        assert!(
            !handle.is_enabled(),
            "off-runtime current() must return an inert handle"
        );
        // No-op control methods must not panic.
        handle.enable();
        handle.disable();
    }

    /// `TelemetryHandle::disabled` is the explicit constructor for an
    /// inert handle.
    #[test]
    fn telemetry_handle_disabled_constructor_is_inert() {
        let handle = TelemetryHandle::disabled();
        assert!(!handle.is_enabled());
    }

    /// Spawning through a disabled handle still resolves the future —
    /// it just falls through to plain `tokio::spawn` without wake
    /// tracking.
    #[test]
    fn disabled_handle_spawn_falls_through_to_tokio_spawn() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = TelemetryHandle::disabled();
        let result = runtime.block_on(async move {
            handle
                .spawn(async { 17u32 })
                .await
                .expect("disabled spawn must still resolve")
        });
        assert_eq!(result, 17);
    }

    /// A disabled guard's `graceful_shutdown` must be a successful
    /// no-op — there is no flush thread or background worker to drain.
    #[test]
    fn disabled_guard_graceful_shutdown_is_noop_ok() {
        let guard = TelemetryGuard::disabled();
        assert!(!guard.is_enabled());
        guard
            .graceful_shutdown(std::time::Duration::from_secs(1))
            .expect("graceful_shutdown on disabled guard must be Ok(())");
    }

    /// The guard returned from a disabled `Dial9Config` is always
    /// present, exposes an inert handle, and reports `is_enabled() ==
    /// false`.
    #[test]
    fn disabled_dial9_config_yields_inert_guard() {
        let cfg = crate::Dial9Config::builder()
            .enabled(false)
            .build()
            .expect("disabled build should succeed");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");

        let guard = rt.guard();
        assert!(!guard.is_enabled());
        let handle = guard.handle();
        assert!(!handle.is_enabled());
        // start_time is None on a disabled guard.
        assert!(guard.start_time().is_none());
        // The runtime still works end-to-end.
        let value = rt.block_on(async { 21u32 });
        assert_eq!(value, 21);
    }

    #[cfg(feature = "worker-s3")]
    #[test]
    fn with_s3_client_then_with_s3_uploader_preserves_client() {
        use crate::background_task::s3::S3Config;

        fn dummy_client() -> aws_sdk_s3::Client {
            let conf = aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "test", "test", None, None, "test",
                ))
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .build();
            aws_sdk_s3::Client::from_conf(conf)
        }

        fn cfg(boot_id: &str) -> S3Config {
            S3Config::builder()
                .bucket("b")
                .service_name("s")
                .boot_id(boot_id)
                .build()
        }

        // Order A: client set after the uploader — already worked.
        let mut builder = TracedRuntime::builder()
            .with_s3_uploader(cfg("a"))
            .with_s3_client(dummy_client());
        match &mut builder.pipeline {
            PipelineConfig::S3(u) => {
                assert!(
                    u.take_client().is_some(),
                    "client must be present in order A"
                );
            }
            _ => panic!("expected S3 pipeline"),
        }

        // Order B: client set first, then a follow-up `with_s3_uploader`. The
        // replacement must carry the previously-bound client across.
        let mut builder = TracedRuntime::builder()
            .with_s3_uploader(cfg("a"))
            .with_s3_client(dummy_client())
            .with_s3_uploader(cfg("b"));
        match &mut builder.pipeline {
            PipelineConfig::S3(u) => {
                assert!(
                    u.take_client().is_some(),
                    "client bound before the second with_s3_uploader must be carried over"
                );
            }
            _ => panic!("expected S3 pipeline"),
        }
    }

    /// Pin which builder paths populate `segment_metadata` (the static
    /// entries the writer embeds as a `SegmentMetadata` event in every
    /// sealed segment file). Today the S3 preset auto-injects;
    /// `with_custom_pipeline` does not, so users on that path opt in via
    /// `with_segment_metadata`.
    mod segment_metadata_routing {
        use super::*;

        fn entries<P, M>(builder: &TracedRuntimeBuilder<P, M>) -> &[(String, String)] {
            &builder.segment_metadata
        }

        #[cfg(feature = "worker-s3")]
        fn s3_cfg() -> crate::background_task::s3::S3Config {
            crate::background_task::s3::S3Config::builder()
                .bucket("test-bucket")
                .service_name("checkout-api")
                .instance_path("us-east-1/i-0abc123")
                .boot_id("test-boot")
                .build()
        }

        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_preset_populates_from_config() {
            let builder = TracedRuntime::builder().with_s3_uploader(s3_cfg());
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(m.get("bucket"), Some(&"test-bucket"));
            assert_eq!(m.get("service_name"), Some(&"checkout-api"));
            assert_eq!(m.get("instance_path"), Some(&"us-east-1/i-0abc123"));
            assert_eq!(m.get("boot_id"), Some(&"test-boot"));
        }

        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_preset_replace_overwrites_metadata() {
            let cfg2 = crate::background_task::s3::S3Config::builder()
                .bucket("other-bucket")
                .service_name("other-svc")
                .boot_id("other-boot")
                .build();
            let builder = TracedRuntime::builder()
                .with_s3_uploader(s3_cfg())
                .with_s3_uploader(cfg2);
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            // cfg2 wins; nothing leaks from the first call.
            assert_eq!(m.get("bucket"), Some(&"other-bucket"));
            assert_eq!(m.get("service_name"), Some(&"other-svc"));
            assert_eq!(m.get("boot_id"), Some(&"other-boot"));
        }

        /// Custom pipeline does NOT auto-populate, even when `b.s3(cfg)` is
        /// composed inside it. Documented behavior — pinned here so a future
        /// change is intentional.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn custom_pipeline_with_s3_does_not_auto_populate() {
            let builder = TracedRuntime::builder().with_custom_pipeline(|b| b.gzip().s3(s3_cfg()));
            assert!(
                entries(&builder).is_empty(),
                "with_custom_pipeline must not auto-inject segment metadata; got {:?}",
                entries(&builder)
            );
        }

        #[test]
        fn custom_pipeline_without_s3_is_empty() {
            let builder = TracedRuntime::builder().with_custom_pipeline(|b| b.gzip().write_back());
            assert!(entries(&builder).is_empty());
        }

        #[test]
        fn unset_pipeline_is_empty() {
            let builder = TracedRuntime::builder();
            assert!(entries(&builder).is_empty());
        }

        /// Custom-pipeline users can recover S3-preset parity by calling
        /// `with_segment_metadata` explicitly.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn with_segment_metadata_recovers_parity_in_custom_pipeline() {
            let cfg = s3_cfg();
            let preset_entries: Vec<(String, String)> = cfg
                .as_metadata()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let builder = TracedRuntime::builder()
                .with_custom_pipeline(|b| b.gzip().s3(s3_cfg()))
                .with_segment_metadata(preset_entries.clone());
            assert_eq!(entries(&builder), preset_entries.as_slice());
        }

        /// `with_segment_metadata` after `with_s3_uploader` overrides the
        /// preset's injection — last call wins.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn with_segment_metadata_after_s3_overrides_preset() {
            let custom = vec![("env".to_string(), "prod".to_string())];
            let builder = TracedRuntime::builder()
                .with_s3_uploader(s3_cfg())
                .with_segment_metadata(custom.clone());
            assert_eq!(entries(&builder), custom.as_slice());
        }

        /// `with_s3_uploader` after `with_segment_metadata` overwrites the
        /// custom entries — same "last call wins" rule.
        #[cfg(feature = "worker-s3")]
        #[test]
        fn s3_after_with_segment_metadata_overwrites() {
            let builder = TracedRuntime::builder()
                .with_segment_metadata(vec![("env".into(), "prod".into())])
                .with_s3_uploader(s3_cfg());
            let m: std::collections::HashMap<&str, &str> = entries(&builder)
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(m.get("bucket"), Some(&"test-bucket"));
            assert!(
                !m.contains_key("env"),
                "with_s3_uploader should overwrite, not merge"
            );
        }
    }

    /// Regression test for issue #400: `TelemetryCoreBuilder` must expose
    /// `.s3_config()` so callers can configure S3 without going through
    /// `TracedRuntimeBuilder`.
    #[cfg(feature = "worker-s3")]
    #[test]
    fn telemetry_core_builder_s3_config_builds_successfully() {
        use crate::background_task::s3::S3Config;

        let dir = tempfile::tempdir().unwrap();
        let trace_path = dir.path().join("trace.bin");
        let s3 = S3Config::builder().bucket("b").service_name("s").build();

        let guard = TelemetryCore::builder()
            .writer(NullWriter)
            .trace_path(&trace_path)
            .s3_config(s3)
            .build()
            .expect("TelemetryCoreBuilder with s3_config must build");

        assert!(guard.is_enabled());
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(1));
    }

    /// Regression test: `TelemetryCore::builder()` with `cpu_profiling` but
    /// without `s3_config` must auto-wire the processor pipeline (symbolize +
    /// gzip + write-back) so the background worker is spawned.
    #[cfg(feature = "cpu-profiling")]
    #[test]
    fn telemetry_core_builder_cpu_profiling_auto_wires_processors() {
        use crate::telemetry::cpu_profile::CpuProfilingConfig;

        let dir = tempfile::tempdir().unwrap();
        let trace_path = dir.path().join("trace.bin");

        // Small max_file_size to force rotation quickly.
        let writer = RotatingWriter::new(&trace_path, 4 * 1024, 10 * 1024 * 1024).unwrap();

        let guard = TelemetryCore::builder()
            .writer(writer)
            .trace_path(&trace_path)
            .cpu_profiling(CpuProfilingConfig::default())
            .worker_poll_interval(std::time::Duration::from_millis(50))
            .build()
            .expect("TelemetryCoreBuilder with cpu_profiling must build");

        guard.enable();

        // Attach a runtime and generate enough events to force segment rotation.
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = guard.trace_runtime("test").build(builder).unwrap();

        runtime.block_on(async {
            // Generate events to fill the small 4KB segment.
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            // Give the worker time to process the sealed segment.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        let _ = guard.graceful_shutdown(std::time::Duration::from_secs(2));

        // After shutdown, the worker should have processed at least one
        // segment. WriteBackProcessor writes .bin.gz files.
        let gz_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "gz"))
            .collect();
        assert!(
            !gz_files.is_empty(),
            "cpu_profiling should auto-wire processors that produce .gz files, \
             but no .gz files found in {:?}. Files present: {:?}",
            dir.path(),
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        );
    }
}
