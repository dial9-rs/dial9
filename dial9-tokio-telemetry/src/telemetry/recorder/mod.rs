mod builder;
mod guard;
mod handle;
mod runtime_context;
pub(crate) use dial9_core::shared_state::SharedState;
pub(crate) use dial9_core::source;

pub(crate) use runtime_context::RuntimeContext;
pub use runtime_context::current_worker_id;
pub(crate) use runtime_context::poll_start_ts_monotonic;

pub use builder::TracedRuntime;
pub use dial9_core::handle::Dial9Handle;
pub(crate) use handle::traced_handle;
pub use handle::{Dial9TokioHandle, spawn};

mod tokio_hooks;
pub use tokio_hooks::TokioHooks;

mod traced_recorder;
pub use traced_recorder::{
    RecorderBuilderTokioExt, TokioAttachConfig, TracedRuntimeBuilder, build_traced,
};

// Re-exports for internal test access
#[cfg(test)]
use handle::InstrumentedSpawnGuard;

use dial9_core::handle::{clear_tl_handle, set_tl_handle};
use handle::INSTRUMENTED_SPAWN;
use runtime_context::{make_poll_end, make_poll_start, make_worker_park, make_worker_unpark};

use crate::primitives::sync::Arc;
use crate::rate_limit::rate_limited;
use crate::telemetry::format::TaskTerminateEvent;
use crate::telemetry::task_metadata::TaskId;
use std::time::Duration;

/// Register a tokio hook, composing with an optional user callback.
/// When `$user_hook` is None, registers only the dial9 closure (zero-cost).
/// When Some, registers a closure that runs dial9 logic first, then the user callbacks.
macro_rules! register_hook {
    // For hooks with no arguments: on_thread_park, on_thread_unpark, on_thread_start, on_thread_stop
    ($builder:expr, $method:ident, $user_hook:expr, $dial9_body:expr) => {
        if let Some(user_hook) = $user_hook {
            $builder.$method(move || {
                $dial9_body;
                user_hook.execute();
            });
        } else {
            $builder.$method(move || {
                $dial9_body;
            });
        }
    };
    // For hooks with a TaskMeta argument: on_before_task_poll, on_after_task_poll, on_task_spawn, on_task_terminate
    (meta: $builder:expr, $method:ident, $user_hook:expr, |$meta:ident| $dial9_body:expr) => {
        if let Some(user_hook) = $user_hook {
            $builder.$method(move |$meta| {
                $dial9_body;
                user_hook.execute($meta);
            });
        } else {
            $builder.$method(move |$meta| {
                $dial9_body;
            });
        }
    };
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
    handle: &Dial9Handle,
    task_tracking_enabled: bool,
    tokio_hooks: TokioHooks,
    #[cfg_attr(not(feature = "taskdump"), allow(unused_variables))] taskdump_config: Option<
        crate::telemetry::task_dump_config::TaskDumpConfig,
    >,
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

    register_hook!(builder, on_thread_park, tokio_hooks.on_thread_park, {
        s1.if_enabled(|buf| {
            let event = make_worker_park(&c1, &s1);
            buf.record_encodable_event(&event);
        })
    });

    register_hook!(builder, on_thread_unpark, tokio_hooks.on_thread_unpark, {
        s2.if_enabled(|buf| {
            let event = make_worker_unpark(&c2, &s2);
            buf.record_encodable_event(&event);
        })
    });

    register_hook!(
        meta: builder,
        on_before_task_poll,
        tokio_hooks.on_before_task_poll,
        |meta| {
            s3.if_enabled(|buf| {
                let task_id = TaskId::from(meta.id());
                let location = meta.spawned_at();
                let event = make_poll_start(&c3, &s3, location, task_id);
                buf.record_encodable_event(&event);
            })
        }
    );

    register_hook!(
        meta: builder,
        on_after_task_poll,
        tokio_hooks.on_after_task_poll,
        |_meta| {
            s4.if_enabled(|buf| {
                let event = make_poll_end(&c4, &s4);
                buf.record_encodable_event(&event);
            })
        }
    );

    if task_tracking_enabled {
        let s5 = shared.clone();
        register_hook!(meta: builder, on_task_spawn, tokio_hooks.on_task_spawn, |meta| {
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
            })
        });
        let s6 = shared.clone();
        register_hook!(
            meta: builder,
            on_task_terminate,
            tokio_hooks.on_task_terminate,
            |meta| {
                s6.if_enabled(|buf| {
                    let task_id = TaskId::from(meta.id());
                    buf.record_encodable_event(&TaskTerminateEvent {
                        timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
                        task_id,
                    });
                })
            }
        );
    } else {
        // When task tracking is disabled, still register user hooks if provided
        if let Some(user_hook) = tokio_hooks.on_task_spawn {
            builder.on_task_spawn(move |meta| {
                user_hook.execute(meta);
            });
        }
        if let Some(user_hook) = tokio_hooks.on_task_terminate {
            builder.on_task_terminate(move |meta| {
                user_hook.execute(meta);
            });
        }
    }

    // Unified on_thread_start / on_thread_stop. Tokio only stores one
    // callback per hook, so any feature-gated work must live here rather
    // than registering its own hook.
    let handle_for_tl = handle.clone();
    #[cfg(feature = "cpu-profiling")]
    let s_stop = shared.clone();

    register_hook!(builder, on_thread_start, tokio_hooks.on_thread_start, {
        // Install this thread's Dial9Handle so user code can call
        // `Dial9Handle::current()` from anywhere on this thread.
        set_tl_handle(handle_for_tl.clone());

        // Install this thread's task-dump config for `TaskDumped` to read.
        #[cfg(feature = "taskdump")]
        if let Some(config) = taskdump_config {
            crate::task_dumped::set_taskdump_config(config);
        }

        #[cfg(feature = "cpu-profiling")]
        {
            // Sched event sampling is deferred to start_sched_sampling_if_needed(),
            // which runs only for worker threads on their first poll/park.
            // This avoids opening perf fds for blocking pool threads.

            // Registers the current thread for the CPU-profiling fallback (ctimer).
            // No-op when perf is the active backend (perf uses inherit).
            let _ = dial9_perf_self_profile::register_current_thread();
        }
    });

    register_hook!(builder, on_thread_stop, tokio_hooks.on_thread_stop, {
        clear_tl_handle();

        #[cfg(feature = "taskdump")]
        crate::task_dumped::clear_taskdump_config();

        #[cfg(feature = "cpu-profiling")]
        {
            s_stop.with_sources_mut(|sources| {
                for source in sources.iter_mut() {
                    source.on_thread_stop();
                }
            });
            dial9_perf_self_profile::unregister_current_thread();
        }
    });
}

/// Attach a runtime to an existing recorder: register hooks, build
/// the runtime, reserve worker IDs, and push the context.
#[allow(clippy::too_many_arguments)]
fn attach_runtime(
    shared: &Arc<SharedState>,
    contexts: &runtime_context::RuntimeContextRegistry,
    mut builder: tokio::runtime::Builder,
    runtime_name: Option<String>,
    handle: &Dial9Handle,
    task_tracking_enabled: bool,
    tokio_hooks: TokioHooks,
    taskdump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
) -> std::io::Result<tokio::runtime::Runtime> {
    let ctx = Arc::new(RuntimeContext::new(runtime_name));
    register_hooks(
        &mut builder,
        &ctx,
        shared,
        handle,
        task_tracking_enabled,
        tokio_hooks,
        taskdump_config,
    );

    let runtime = builder.build()?;

    // Install the handle on the calling thread. For current_thread runtimes,
    // this thread IS the worker (block_on runs here), so the tracing layer
    // needs the TL handle to be set. Harmless for multi_thread runtimes.
    set_tl_handle(handle.clone());

    // Same for the task-dump config: on_thread_start skips this thread.
    #[cfg(feature = "taskdump")]
    if let Some(config) = taskdump_config {
        crate::task_dumped::set_taskdump_config(config);
    }

    // Pre-reserve a contiguous block of worker IDs and set metrics atomically.
    let metrics = runtime.handle().metrics();
    let num_workers = metrics.num_workers() as u64;
    let base = shared.reserve_worker_ids(num_workers);
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

    contexts.lock().unwrap().push(ctx);

    // No need to announce the metadata change: `TokioRuntimesSource` detects the
    // new runtime (and its eagerly-populated workers) from the runtime/worker
    // counts on its next flush.

    Ok(runtime)
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;
    use crate::background_task::testutil::{CapturingProcessor, decode_captured};
    use crate::telemetry::buffer::MemoryBuffer;
    use dial9_core::recorder::recorder;
    use dial9_core::test_util;
    use std::panic::Location;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory capture budget for runtime tests.
    const CAPTURE_SIZE: u64 = 16 * 1024 * 1024;

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
        let (capture, data) = CapturingProcessor::new();

        let traced = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_tokio(|t| {
                *t = tokio::runtime::Builder::new_current_thread();
                t.enable_all();
            })
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        traced.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let poll_starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::telemetry::analysis_events::Dial9Event::PollStartEvent(ev) => {
                    Some(crate::telemetry::format::WorkerId(ev.worker_id.0))
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
    fn tokio_instrumentation_can_be_disabled_without_installing_hooks() {
        let (capture, data) = CapturingProcessor::new();
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let on_thread_start_calls = hook_calls.clone();
        let on_before_poll_calls = hook_calls.clone();

        let traced = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .with_tokio_instrumentation(false)
            .with_task_tracking(true)
            .with_tokio_hooks(|hooks| {
                hooks.on_thread_start(move || {
                    on_thread_start_calls.fetch_add(1, Ordering::Relaxed);
                });
                hooks.on_before_task_poll(move |_meta| {
                    on_before_poll_calls.fetch_add(1, Ordering::Relaxed);
                });
            })
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        assert!(traced.is_enabled());
        assert!(traced.shared().unwrap().is_enabled());
        let runtime_meta = traced
            .shared()
            .unwrap()
            .with_sources_mut(source::collect_segment_metadata)
            .unwrap();
        assert!(
            !runtime_meta.iter().any(|(k, _)| k.starts_with("runtime.")),
            "disabled Tokio instrumentation should not produce runtime metadata"
        );

        traced.runtime().block_on(async {
            for _ in 0..8 {
                tokio::spawn(async {
                    tokio::task::yield_now().await;
                })
                .await
                .unwrap();
            }
        });

        let runtime_meta = traced
            .shared()
            .unwrap()
            .with_sources_mut(source::collect_segment_metadata)
            .unwrap();
        assert!(
            !runtime_meta.iter().any(|(k, _)| k.starts_with("runtime.")),
            "disabled Tokio instrumentation should not produce runtime metadata after running work"
        );
        assert_eq!(
            hook_calls.load(Ordering::Relaxed),
            0,
            "user Tokio hooks should not be installed when Tokio instrumentation is disabled"
        );

        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = if raw.is_empty() {
            Vec::new()
        } else {
            decode_captured(&raw)
        };
        assert!(
            events.iter().all(|event| !matches!(
                event,
                crate::telemetry::analysis_events::Dial9Event::PollStartEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::PollEndEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::WorkerParkEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::WorkerUnparkEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::QueueSampleEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::TaskSpawnEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::TaskTerminateEvent(..)
                    | crate::telemetry::analysis_events::Dial9Event::WakeEvent(..)
            )),
            "Tokio runtime events should not be recorded when Tokio instrumentation is disabled: {events:?}"
        );
    }

    #[test]
    fn test_shared_state_no_spawn_location_fields() {
        let _shared = SharedState::new(crate::telemetry::events::clock_monotonic_ns());
    }

    #[test]
    fn build_disabled_produces_working_runtime_with_noop_guard() {
        let traced = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .with_tokio(|_| {})
            .enabled(false)
            .build()
            .unwrap();

        // Guard methods should be safe no-ops
        traced.enable();
        traced.disable();
        let handle = traced.handle();
        let _start = traced.start_time();

        // Runtime should work normally, including handle.spawn
        traced.runtime().block_on(async {
            let result = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(result, 42);

            let traced = handle.spawn(async { 7 }).await.unwrap();
            assert_eq!(traced, 7);
        });

        // No flush thread or worker to join — the guard is in its
        // disabled state.
        assert!(!traced.is_enabled());
    }

    #[test]
    #[cfg(feature = "analysis")]
    fn test_spawn_locations_resolve_after_rotation() {
        use crate::telemetry::analysis::TraceReader;
        use crate::telemetry::format::WorkerId;

        let dir = tempfile::TempDir::new().unwrap();

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

        let writer = crate::telemetry::buffer::DiskBuffer::builder()
            .base_path(dir.path())
            .max_file_size(100)
            .max_total_size(100_000)
            .build()
            .unwrap();
        let mut ew = writer;
        let shared = crate::telemetry::recorder::SharedState::new(0);

        let locations = [
            location_a, location_b, location_a, location_b, location_a, location_b,
        ];
        for (i, loc) in locations.iter().enumerate() {
            let task_id = crate::telemetry::task_metadata::TaskId::from_u32(i as u32);
            let ts = (i as u64 + 1) * 1000;
            shared.flush_context().with_encoder(|enc| {
                let spawn_loc = enc.intern_location(loc);
                enc.encode(&crate::telemetry::format::TaskSpawnEvent {
                    timestamp_ns: ts,
                    task_id,
                    spawn_loc,
                    instrumented: true,
                });
            });
            shared.flush_context().with_encoder(|enc| {
                let spawn_loc = enc.intern_location(loc);
                enc.encode(&crate::telemetry::format::PollStartEvent {
                    timestamp_ns: ts,
                    worker_id: WorkerId::from(0usize),
                    local_queue: 0,
                    task_id,
                    spawn_loc,
                });
            });
            // Drain after each iteration to produce separate small batches
            // that trigger file rotation (max_file_size is 100 bytes).
            test_util::drain_into(&shared, &mut ew).unwrap();
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

            for loc in reader.task_spawn_locs.values() {
                assert!(
                    loc.contains(':'),
                    "location should be file:line:col, got {loc:?}"
                );
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
    fn trace_runtime_attaches_second_runtime() {
        let traced = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .with_tokio(|_| {})
            .build()
            .unwrap();

        let builder_b = tokio::runtime::Builder::new_multi_thread();
        let (runtime_b, _handle_b) = traced.trace_runtime("attached").build(builder_b).unwrap();

        // Both runtimes should work
        traced.block_on(async {
            let r = tokio::spawn(async { 1 }).await.unwrap();
            assert_eq!(r, 1);
        });
        runtime_b.block_on(async {
            let r = tokio::spawn(async { 2 }).await.unwrap();
            assert_eq!(r, 2);
        });
    }

    #[test]
    fn trace_runtime_produces_unique_worker_ids() {
        use std::collections::HashSet;

        let (capture, data) = CapturingProcessor::new();

        let traced = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .with_task_tracking(true)
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let (runtime_b, _handle_b) = traced
            .trace_runtime("attached")
            .task_tracking(true)
            .build(builder_b)
            .unwrap();

        // Generate poll events on both runtimes. Spawn many concurrent tasks
        // to ensure work lands on actual worker threads (not just block_on's thread).
        traced.block_on(async {
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
        drop(runtime_b);
        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let captured = decode_captured(&raw);
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in captured.iter() {
            let wid = match event {
                crate::telemetry::analysis_events::Dial9Event::PollStartEvent(e) => {
                    Some(e.worker_id)
                }
                crate::telemetry::analysis_events::Dial9Event::PollEndEvent(e) => Some(e.worker_id),
                crate::telemetry::analysis_events::Dial9Event::WorkerParkEvent(e) => {
                    Some(e.worker_id)
                }
                crate::telemetry::analysis_events::Dial9Event::WorkerUnparkEvent(e) => {
                    Some(e.worker_id)
                }
                _ => None,
            };
            if let Some(id) = wid
                && id != crate::telemetry::analysis_events::WorkerId::UNKNOWN
            {
                worker_ids.insert(id.as_u64());
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

    /// Verify that attaching a second runtime via `trace_runtime` propagates its
    /// metadata (runtime name → worker ID mapping) into the trace file's segment metadata.
    #[test]
    fn trace_runtime_propagates_second_runtime_metadata() {
        use crate::telemetry::analysis_events::Dial9Event;

        let dir = tempfile::TempDir::new().unwrap();

        let writer = crate::telemetry::buffer::DiskBuffer::builder()
            .base_path(dir.path())
            .max_file_size(1024 * 1024)
            .max_total_size(10 * 1024 * 1024)
            .build()
            .unwrap();

        let traced = recorder(writer)
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .with_runtime_name("main")
            .build()
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let (runtime_b, _handle_b) = traced.trace_runtime("io").build(builder_b).unwrap();

        // Run work on both runtimes so workers resolve their identities.
        for rt in [traced.runtime(), &runtime_b] {
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

        drop(runtime_b);
        traced.graceful_shutdown(Duration::from_secs(1));

        // Read all sealed trace files and collect SegmentMetadata entries.
        let mut all_metadata: Vec<std::collections::HashMap<String, String>> = Vec::new();
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
                if let Dial9Event::SegmentMetadataEvent(meta) = event {
                    all_metadata.push(meta.entries.clone());
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

    /// End-to-end: a runtime attached to an existing recorder has its
    /// self-detected segment metadata (the runtime→worker mapping) written into
    /// a sealed segment that decodes back. Exercises the full wiring:
    /// `attach → TokioRuntimesSource::segment_metadata → writer → encode → decode`.
    ///
    /// Fully deterministic, with no `sleep`: the only synchronization is
    /// `graceful_shutdown`, which blocks until the flush thread runs its final
    /// source poll, writes the segment metadata, and seals the segment. Both
    /// runtimes' workers are eagerly populated at attach time, so the metadata
    /// is complete regardless of how (or whether) each runtime is driven.
    ///
    /// The narrower "re-emit only after the runtime/worker count actually grows"
    /// logic is unit-tested deterministically in
    /// `runtime_context::tests::segment_metadata_only_rebuilds_after_a_change`.
    #[test]
    fn attached_runtime_metadata_reaches_sealed_segment() {
        use crate::telemetry::analysis_events::Dial9Event;

        let dir = tempfile::TempDir::new().unwrap();

        let writer = crate::telemetry::buffer::DiskBuffer::builder()
            .base_path(dir.path())
            .max_file_size(1024 * 1024)
            .max_total_size(10 * 1024 * 1024)
            .build()
            .unwrap();

        let traced = recorder(writer)
            .with_tokio(|t| {
                *t = tokio::runtime::Builder::new_current_thread();
                t.enable_all();
            })
            .with_runtime_name("first")
            .with_task_tracking(true)
            .build()
            .unwrap();

        // Drive a little real work so the final segment is sealed rather than
        // discarded: `finalize()` removes a segment that holds only header +
        // metadata (no real events). Spawning a tracked task emits real events
        // synchronously — no timing wait.
        traced.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        // Attach B to the same recorder. Its workers are eagerly populated at
        // attach time, so its metadata is complete without ever driving it.
        let builder_b = tokio::runtime::Builder::new_current_thread();
        let (runtime_b, _handle_b) = traced.trace_runtime("second").build(builder_b).unwrap();

        drop(runtime_b);
        // Blocks until the flush thread polls every source one final time, writes
        // the segment metadata, and seals the segment, so both runtimes are
        // guaranteed to be in the sealed trace once this returns.
        traced.graceful_shutdown(Duration::from_secs(1));

        let mut saw_first = false;
        let mut saw_second = false;
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
                if let Dial9Event::SegmentMetadataEvent(meta) = event {
                    if meta.entries.keys().any(|k| k == "runtime.first") {
                        saw_first = true;
                    }
                    if meta.entries.keys().any(|k| k == "runtime.second") {
                        saw_second = true;
                    }
                }
            }
        }

        assert!(
            saw_first,
            "the initial runtime should appear in segment metadata"
        );
        assert!(
            saw_second,
            "an attached runtime should appear in a sealed segment's metadata; \
             missing runtime.second means TokioRuntimesSource failed to \
             self-detect the new runtime"
        );
    }

    /// Wake events from runtime B's workers must carry global worker IDs (≥ num_workers_a),
    /// not local indices that collide with runtime A's workers.
    #[test]
    fn wake_events_use_global_worker_id_in_multi_runtime() {
        use crate::telemetry::analysis_events::Dial9Event;

        let (capture, data) = CapturingProcessor::new();

        let traced = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_tokio(|t| {
                t.worker_threads(2);
            })
            .with_task_tracking(true)
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2);
        let (runtime_b, _handle_b) = traced
            .trace_runtime("attached")
            .task_tracking(true)
            .build(builder_b)
            .unwrap();

        // Spawn on runtime B with wake-tracked wrapping → wake events.
        let handle = traced.tokio_handle(runtime_b.handle());
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

        drop(runtime_b);
        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let captured = decode_captured(&raw);
        let wake_workers: Vec<u8> = captured
            .iter()
            .filter_map(|e| match e {
                Dial9Event::WakeEvent(w) => Some(w.target_worker),
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
        use crate::telemetry::analysis_events::Dial9Event;
        use crate::telemetry::buffer::DiskBuffer;
        use crate::telemetry::format::{WorkerId, WorkerParkEvent};
        use crate::telemetry::task_metadata::TaskId;
        use proptest::prelude::*;

        /// Encode a single event into a batch and write it through the writer.
        fn write_raw_event(
            writer: &mut DiskBuffer,
            event: &dyn crate::telemetry::encoder::Encodable,
        ) -> std::io::Result<()> {
            test_util::write_event(writer, event)
        }

        #[derive(Debug, Clone)]
        enum FlushOp {
            OtherEvent { worker_id: WorkerId, tid: u32 },
            PollStart { location_idx: usize },
        }

        fn arb_flush_op() -> impl Strategy<Value = FlushOp> {
            prop_oneof![
                (prop::bool::ANY, 0u32..4,).prop_map(|(is_worker, tid)| {
                    FlushOp::OtherEvent {
                        worker_id: if is_worker {
                            WorkerId::from(0usize)
                        } else {
                            WorkerId::UNKNOWN
                        },
                        tid,
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
                        .filter(|o| matches!(o, FlushOp::OtherEvent { .. }))
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
            ew: &mut DiskBuffer,
            locations: &[&'static Location<'static>],
            timestamp: &mut u64,
            expected_raw: &mut usize,
        ) {
            for op in &round.cpu_ops {
                if let FlushOp::OtherEvent { worker_id, tid } = op {
                    write_raw_event(
                        &mut *ew,
                        &WorkerParkEvent {
                            timestamp_ns: *timestamp,
                            worker_id: *worker_id,
                            local_queue: 0,
                            cpu_time_ns: 0,
                            tid: *tid,
                        },
                    )
                    .unwrap();
                    *timestamp += 1;
                }
            }

            for op in &round.raw_ops {
                if let FlushOp::PollStart { location_idx } = op {
                    let loc = locations[*location_idx];
                    let task_id = TaskId::from_u32(*timestamp as u32);
                    let ts = *timestamp;
                    *timestamp += 1;

                    write_raw_event(
                        &mut *ew,
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

                for ev in &reader.all_events {
                    if matches!(ev, Dial9Event::PollStartEvent(_)) {
                        total_raw += 1;
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

                let writer = DiskBuffer::builder()
                    .base_path(dir.path())
                    .max_file_size(max_file_size)
                    .max_total_size(1_000_000)
                    .build()
                    .unwrap();

                let mut ew = writer;

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

    // A current-thread primary keeps these recorder tests from spawning worker
    // threads; the runtime under test is attached via `trace_runtime`.
    fn session_recorder<M: dial9_core::buffer::BufferMode>(
        writer: dial9_core::buffer::SegmentWriter<M>,
    ) -> crate::telemetry::TracedRuntimeBuilder<M> {
        recorder(writer).with_tokio(|t| {
            *t = tokio::runtime::Builder::new_current_thread();
        })
    }

    #[test]
    fn build_produces_enabled_guard() {
        let traced = session_recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .build()
            .unwrap();
        assert!(traced.is_enabled());
        traced.graceful_shutdown(Duration::from_secs(1));
    }

    #[test]
    fn trace_runtime_produces_working_runtime() {
        let traced = session_recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .build()
            .unwrap();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = traced.trace_runtime("main").build(builder).unwrap();

        runtime.block_on(async {
            let r = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(r, 42);
        });

        drop(runtime);
        traced.graceful_shutdown(Duration::from_secs(1));
    }

    #[test]
    fn task_tracking_produces_task_spawn_events() {
        let (capture, data) = CapturingProcessor::new();
        let traced = session_recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, _handle) = traced
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
        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let spawn_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::analysis_events::Dial9Event::TaskSpawnEvent(..)
                )
            })
            .count();
        assert!(
            spawn_count > 0,
            "expected TaskSpawn events when task_tracking is enabled, got none"
        );
    }

    #[test]
    fn trace_runtime_multiple_runtimes_unique_worker_ids() {
        use std::collections::HashSet;

        let (capture, data) = CapturingProcessor::new();
        let traced = session_recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(2).enable_all();
        let (runtime_a, _handle_a) = traced
            .trace_runtime("main")
            .task_tracking(true)
            .build(builder_a)
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(2).enable_all();
        let (runtime_b, _handle_b) = traced
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
        traced.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let captured = decode_captured(&raw);
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in &captured {
            if let crate::telemetry::analysis_events::Dial9Event::PollStartEvent(e) = event
                && e.worker_id != crate::telemetry::analysis_events::WorkerId::UNKNOWN
            {
                worker_ids.insert(e.worker_id.as_u64());
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
        let (capture, data) = CapturingProcessor::new();
        let traced = session_recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .with_custom_pipeline(|p| p.pipe(capture))
            .build()
            .unwrap();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(2).enable_all();
        let (runtime, handle) = traced.trace_runtime("main").build(builder).unwrap();

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
        test_util::drain_thread_local(
            &traced_handle(&traced.record_handle())
                .expect("enabled handle must yield a TracedHandle")
                .shared,
        );

        drop(runtime);
        traced.graceful_shutdown(Duration::from_secs(1));

        // Verify wake events were recorded (handle.spawn wraps with wake tracking)
        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let wake_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::analysis_events::Dial9Event::WakeEvent(..)
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
        let traced = session_recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .build()
            .unwrap();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(1).enable_all().thread_name("rt-a");
        let (rt_a, handle_a) = traced.trace_runtime("a").build(builder_a).unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(1).enable_all().thread_name("rt-b");
        let (rt_b, handle_b) = traced.trace_runtime("b").build(builder_b).unwrap();

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
        traced.graceful_shutdown(Duration::from_secs(1));
    }

    // ---------------------------------------------------------------
    // Always-present TracedRuntime / inert Dial9Handle (Phase 3)
    // ---------------------------------------------------------------

    /// Off-runtime callers should get a usable, inert handle rather
    /// than a panic.
    #[test]
    fn telemetry_handle_current_off_runtime_returns_inert_handle() {
        // We're on the test thread, which is not owned by any dial9
        // runtime. `current()` used to panic here.
        let handle = Dial9Handle::current();
        assert!(
            !handle.is_enabled(),
            "off-runtime current() must return an inert handle"
        );
        // No-op control methods must not panic.
        handle.enable();
        handle.disable();
    }

    /// `Dial9Handle::disabled` is the explicit constructor for an
    /// inert handle.
    #[test]
    fn telemetry_handle_disabled_constructor_is_inert() {
        let handle = Dial9Handle::disabled();
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
        let handle = Dial9TokioHandle::disabled();
        let result = runtime.block_on(async move {
            handle
                .spawn(async { 17u32 })
                .await
                .expect("disabled spawn must still resolve")
        });
        assert_eq!(result, 17);
    }

    /// A disabled recorder's `graceful_shutdown` must be a no-op — there is no
    /// flush thread or background worker to drain.
    #[test]
    fn disabled_session_graceful_shutdown_is_noop() {
        let traced = crate::telemetry::TracedRuntimeBuilder::<dial9_core::buffer::Disk>::disabled()
            .build()
            .unwrap();
        assert!(!traced.is_enabled());
        traced.graceful_shutdown(Duration::from_secs(1));
    }

    /// Regression test for issue #400: multi-runtime callers must be able to
    /// configure S3 upload, via `.with_s3_uploader()` on the recorder builder.
    #[cfg(feature = "worker-s3")]
    #[test]
    fn session_builder_s3_config_builds_successfully() {
        use crate::background_task::s3::S3Config;

        let s3 = S3Config::builder().bucket("b").service_name("s").build();

        let traced = session_recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .with_s3_uploader(s3)
            .build()
            .expect("recorder with s3 uploader must build");

        assert!(traced.is_enabled());
        traced.graceful_shutdown(Duration::from_secs(1));
    }
}
