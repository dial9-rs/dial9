mod handle;
mod join_set;
mod recorder_tokio;
mod runtime_context;
#[cfg(test)]
pub(crate) use dial9_core::shared_state::SharedState;
pub(crate) use dial9_core::source;

pub(crate) use runtime_context::RuntimeContext;
pub use runtime_context::current_worker_id;
#[cfg(any(feature = "taskdump", test))]
pub(crate) use runtime_context::poll_start_ts_monotonic;

pub use dial9_core::handle::Dial9Handle;
pub(crate) use handle::traced_handle;
pub use handle::{Dial9TokioHandle, block_on, spawn, spawn_in};
pub use join_set::JoinSetExt;

mod tokio_hooks;
pub use tokio_hooks::TokioHooks;

#[cfg(feature = "worker-s3")]
pub use recorder_tokio::RecorderS3ClientExt;
pub use recorder_tokio::{
    AttachedRuntime, Dial9HandleTokioExt, RecorderPipelineExt, TokioAttachOptions,
};

// Re-exports for internal test access
#[cfg(all(test, not(shuttle)))]
use handle::InstrumentedSpawnGuard;

use dial9_core::handle::{clear_tl_handle, set_tl_handle};

use crate::primitives::sync::Arc;
#[cfg(tokio_unstable)]
use crate::telemetry::task_metadata::TaskId;
#[cfg(tokio_unstable)]
use handle::INSTRUMENTED_SPAWN;
#[cfg(not(tokio_unstable))]
pub(crate) use recorder_tokio::current_runtime_ctx;
pub(crate) use runtime_context::{clear_poll_span, poll_span_open};

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
/// Closures capture `Arc<RuntimeContext>`, which owns both the runtime-specific
/// state and the handle events are recorded through.
///
/// # Worker ID resolution
///
/// `WORKER_ID` TLS is populated lazily on the first `on_thread_unpark` / `on_before_task_poll`
/// call via `RuntimeContext::resolve_worker`, not in `on_thread_start`.
/// This is intentional: `on_thread_start` fires before `RuntimeMetrics` is available, so we
/// cannot yet call `metrics.worker_thread_id(i)` to determine which worker index we are.
/// By the time any waker calls `current_worker_id()`, at least one unpark or poll has occurred
/// and TLS is guaranteed to be populated.
fn register_hooks(
    builder: &mut tokio::runtime::Builder,
    ctx: &Arc<RuntimeContext>,
    handle: &Dial9Handle,
    #[cfg_attr(not(tokio_unstable), allow(unused_variables))] task_tracking_enabled: bool,
    tokio_hooks: TokioHooks,
    #[cfg_attr(not(feature = "taskdump"), allow(unused_variables))] taskdump_config: Option<
        crate::telemetry::task_dump_config::TaskDumpConfig,
    >,
) {
    let c1 = ctx.clone();
    let c2 = ctx.clone();
    #[cfg(tokio_unstable)]
    let c3 = ctx.clone();
    #[cfg(tokio_unstable)]
    let c4 = ctx.clone();

    register_hook!(builder, on_thread_park, tokio_hooks.on_thread_park, {
        c1.record_worker_park()
    });

    register_hook!(builder, on_thread_unpark, tokio_hooks.on_thread_unpark, {
        c2.record_worker_unpark()
    });

    #[cfg(tokio_unstable)]
    register_hook!(
        meta: builder,
        on_before_task_poll,
        tokio_hooks.on_before_task_poll,
        |meta| { c3.record_poll_start(meta.spawned_at(), TaskId::from(meta.id())) }
    );

    #[cfg(tokio_unstable)]
    register_hook!(
        meta: builder,
        on_after_task_poll,
        tokio_hooks.on_after_task_poll,
        |_meta| { c4.record_poll_end() }
    );

    #[cfg(tokio_unstable)]
    if task_tracking_enabled {
        let c5 = ctx.clone();
        register_hook!(meta: builder, on_task_spawn, tokio_hooks.on_task_spawn, |meta| {
            c5.record_task_spawn(
                meta.spawned_at(),
                TaskId::from(meta.id()),
                INSTRUMENTED_SPAWN.with(|f| f.get()) > 0,
            )
        });
        let c6 = ctx.clone();
        register_hook!(
            meta: builder,
            on_task_terminate,
            tokio_hooks.on_task_terminate,
            |meta| { c6.record_task_terminate(TaskId::from(meta.id())) }
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
            runtime_context::stop_sched_sampling();
            // Blocking pool threads register with the ctimer fallback in
            // `on_thread_start` but never enroll with the sources, so they still
            // need an explicit unregister. Idempotent for worker threads.
            dial9_perf_self_profile::unregister_current_thread();
        }
    });
}

/// Register telemetry hooks and return a runtime context.
/// Worker IDs are reserved lazily on the first poll.
fn register_runtime_hooks(
    builder: &mut tokio::runtime::Builder,
    runtime_name: Option<String>,
    handle: &Dial9Handle,
    worker_ids: runtime_context::WorkerIdCounter,
    task_tracking_enabled: bool,
    tokio_hooks: TokioHooks,
    taskdump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
) -> Arc<RuntimeContext> {
    let ctx = Arc::new(RuntimeContext::new(
        runtime_name,
        handle.clone(),
        worker_ids,
    ));
    register_hooks(
        builder,
        &ctx,
        handle,
        task_tracking_enabled,
        tokio_hooks,
        taskdump_config,
    );
    ctx
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
    use std::time::Duration;

    use super::handle::INSTRUMENTED_SPAWN;

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
    fn runtime_hooks_do_not_publish_before_build() {
        clear_tl_handle();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap()).build();
        let state = recorder_tokio::tokio_attach_state(rec.handle()).unwrap();
        let registry = state.registry;
        let mut builder = tokio::runtime::Builder::new_current_thread();

        let ctx = register_runtime_hooks(
            &mut builder,
            Some("aborted".to_string()),
            rec.handle(),
            state.worker_ids,
            false,
            TokioHooks::default(),
            None,
        );
        let weak = Arc::downgrade(&ctx);

        assert!(registry.lock().unwrap().is_empty());
        assert!(!Dial9Handle::current().is_enabled());

        drop(builder);
        drop(ctx);
        assert!(weak.upgrade().is_none());

        rec.graceful_shutdown(Duration::from_secs(1));
    }

    #[test]
    fn current_thread_runtime_resolves_worker_ids() {
        let (capture, data) = CapturingProcessor::new();

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let rt = rec
            .handle()
            .attach_tokio_runtime(builder, TokioAttachOptions::default())
            .unwrap();

        rt.block_on(async {
            crate::telemetry::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(1));

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
        #[cfg(tokio_unstable)]
        let on_before_poll_calls = hook_calls.clone();

        let mut hooks = TokioHooks::default();
        hooks.on_thread_start(move || {
            on_thread_start_calls.fetch_add(1, Ordering::Relaxed);
        });
        #[cfg(tokio_unstable)]
        hooks.on_before_task_poll(move |_meta| {
            on_before_poll_calls.fetch_add(1, Ordering::Relaxed);
        });

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(2);
        let rt = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder()
                    .tokio_instrumentation_enabled(false)
                    .task_tracking_enabled(true)
                    .tokio_hooks(hooks)
                    .build(),
            )
            .unwrap();

        assert!(rec.handle().is_enabled());
        let runtime_meta = rec
            .shared()
            .unwrap()
            .with_sources_mut(source::collect_segment_metadata)
            .unwrap();
        assert!(
            !runtime_meta.iter().any(|(k, _)| k.starts_with("runtime.")),
            "disabled Tokio instrumentation should not produce runtime metadata"
        );

        rt.block_on(async {
            for _ in 0..8 {
                tokio::spawn(async {
                    tokio::task::yield_now().await;
                })
                .await
                .unwrap();
            }
        });

        let runtime_meta = rec
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

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(1));

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
                    | crate::telemetry::analysis_events::Dial9Event::RuntimeMetricsEvent(..)
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

    /// A paused recorder does none of the work a recorded poll needs: no worker
    /// ID reserved, no poll span left open. Building the event before the
    /// enabled check would do both with nothing to show for it.
    #[test]
    fn paused_recorder_does_no_poll_work() {
        use std::collections::HashSet;

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap()).build();
        rec.disable();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(1);
        let rt = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder()
                    .runtime_name("paused")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        // Read the marker on the worker thread itself: it is thread-local.
        let span_open = rt.block_on(async {
            crate::telemetry::spawn(async { poll_span_open() })
                .await
                .unwrap()
        });
        assert!(!span_open, "a paused recorder should not open a poll span");

        let reserved: HashSet<u64> = {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            registry
                .iter()
                .find(|c| c.runtime_name.as_deref() == Some("paused"))
                .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                .unwrap_or_default()
        };
        assert!(
            reserved.is_empty(),
            "a paused recorder should not resolve workers: {reserved:?}"
        );

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// A disabled recorder attaches inertly: the runtime you build is a plain
    /// Tokio runtime and every recorder method is a safe no-op.
    #[test]
    fn disabled_recorder_attach_produces_working_runtime() {
        let rec = dial9_core::recorder::recorder_disabled();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        let rt = rec
            .handle()
            .attach_tokio_runtime(builder, TokioAttachOptions::default())
            .unwrap();

        // Recorder methods should be safe no-ops.
        rec.enable();
        rec.disable();
        let handle =
            Dial9TokioHandle::for_runtime(rt.handle().clone(), traced_handle(rec.handle()));
        let _start = rec.start_time();

        // Runtime should work normally, including handle.spawn
        rt.block_on(async {
            let result = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(result, 42);

            let traced = handle.spawn(async { 7 }).await.unwrap();
            assert_eq!(traced, 7);
        });

        assert!(!rec.handle().is_enabled());
        assert!(
            rec.shared().is_none(),
            "a disabled recorder has no state to attach to"
        );

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(1));
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
    fn attach_adds_a_second_runtime() {
        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.enable_all();
        let runtime_a = rec
            .handle()
            .attach_tokio_runtime(builder_a, TokioAttachOptions::default())
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.enable_all();
        let runtime_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder()
                    .runtime_name("attached")
                    .build(),
            )
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

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// Verify that attaching a second runtime propagates its metadata (runtime
    /// name → worker ID mapping) into the trace file's segment metadata.
    #[test]
    fn attach_propagates_second_runtime_metadata() {
        use crate::telemetry::analysis_events::Dial9Event;

        let dir = tempfile::TempDir::new().unwrap();

        let writer = crate::telemetry::buffer::DiskBuffer::builder()
            .base_path(dir.path())
            .max_file_size(1024 * 1024)
            .max_total_size(10 * 1024 * 1024)
            .build()
            .unwrap();

        let rec = recorder(writer).build();

        let mut builder_a = tokio::runtime::Builder::new_current_thread();
        builder_a.enable_all();
        let runtime_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder().runtime_name("main").build(),
            )
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_current_thread();
        builder_b.enable_all();
        let runtime_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder().runtime_name("io").build(),
            )
            .unwrap();

        // Drive each runtime so its worker resolves an identity.
        for rt in [&runtime_a, &runtime_b] {
            rt.block_on(async {
                crate::telemetry::spawn(async {
                    tokio::task::yield_now().await;
                })
                .await
                .unwrap();
            });
        }

        // Give the flush thread time to run (it cycles every 5ms and merges
        // runtime metadata into the writer on each cycle).
        std::thread::sleep(std::time::Duration::from_millis(50));

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));

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

        // At least one segment's metadata should map both runtimes to two
        // workers each, with no ID shared between them. Which runtime gets the
        // lower block is not fixed: blocks are reserved when a runtime's workers
        // first resolve, so the two runtimes race at startup.
        let ids = |entries: &std::collections::HashMap<String, String>, key: &str| {
            entries.get(key).map(|v| {
                v.split(',')
                    .map(|id| id.parse::<u64>().unwrap())
                    .collect::<std::collections::HashSet<_>>()
            })
        };
        let has_both = all_metadata.iter().any(|entries| {
            match (ids(entries, "runtime.main"), ids(entries, "runtime.io")) {
                (Some(main), Some(io)) => main.len() == 1 && io.len() == 1 && main.is_disjoint(&io),
                _ => false,
            }
        });
        assert!(
            has_both,
            "expected segment metadata to map runtime.main and runtime.io to \
             one disjoint worker ID each, got: {all_metadata:?}"
        );
    }

    /// End-to-end: a runtime attached to an existing recorder has its
    /// self-detected segment metadata (the runtime→worker mapping) written into
    /// a sealed segment that decodes back. Exercises the full wiring:
    /// `attach → TokioRuntimesSource::segment_metadata → writer → encode → decode`.
    ///
    /// Fully deterministic, with no `sleep`: the only synchronization is
    /// `graceful_shutdown`, which blocks until the flush thread runs its final
    /// source poll, writes the segment metadata, and seals the segment. Worker
    /// IDs resolve on first poll, so each runtime is driven once before it is
    /// dropped.
    ///
    /// The narrower "re-emit only after the runtime/worker count actually grows"
    /// logic is unit-tested deterministically in
    /// `runtime_context::tests::segment_metadata_only_rebuilds_after_a_change`.
    #[cfg(tokio_unstable)]
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

        let rec = recorder(writer).build();

        let mut builder_a = tokio::runtime::Builder::new_current_thread();
        builder_a.enable_all();
        let runtime_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder()
                    .runtime_name("first")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        // Drive a little real work so the final segment is sealed rather than
        // discarded: `finalize()` removes a segment that holds only header +
        // metadata (no real events). Spawning a tracked task emits real events
        // synchronously — no timing wait.
        runtime_a.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        // Attach B to the same recorder and drive it once, so its worker
        // resolves its ID and the runtime→worker mapping is populated.
        let mut builder_b = tokio::runtime::Builder::new_current_thread();
        builder_b.enable_all();
        let runtime_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder().runtime_name("second").build(),
            )
            .unwrap();
        runtime_b.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        drop(runtime_a);
        drop(runtime_b);
        // Blocks until the flush thread polls every source one final time, writes
        // the segment metadata, and seals the segment, so both runtimes are
        // guaranteed to be in the sealed trace once this returns.
        rec.graceful_shutdown(Duration::from_secs(1));

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

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.enable_all().worker_threads(2);
        let runtime_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder()
                    .runtime_name("main")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.enable_all().worker_threads(2);
        let runtime_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder()
                    .runtime_name("attached")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        // Drive A too, so both runtimes reserve a worker-ID block.
        runtime_a.block_on(async {
            tokio::spawn(async {
                tokio::task::yield_now().await;
            })
            .await
            .unwrap();
        });

        // Spawn on runtime B with wake-tracked wrapping → wake events.
        let handle =
            Dial9TokioHandle::for_runtime(runtime_b.handle().clone(), traced_handle(rec.handle()));
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

        // Blocks are reserved when a runtime's workers first resolve, so which
        // runtime holds the lower block is not fixed.
        let (main_ids, attached_ids) = {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            let block = |name: &str| -> std::collections::HashSet<u64> {
                registry
                    .iter()
                    .find(|c| c.runtime_name.as_deref() == Some(name))
                    .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                    .unwrap_or_default()
            };
            (block("main"), block("attached"))
        };
        assert!(
            main_ids.is_disjoint(&attached_ids),
            "runtimes must not share worker IDs: main={main_ids:?} attached={attached_ids:?}"
        );

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));

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

        // All wakes were issued on runtime B, so they must carry B's global
        // worker IDs — never a local index that collides with runtime A's block.
        let all_from_b = wake_workers
            .iter()
            .all(|&w| w == 255 || attached_ids.contains(&(w as u64)));
        assert!(
            all_from_b,
            "expected wake events from runtime B to use its global worker IDs \
             ({attached_ids:?}), but got: {wake_workers:?}"
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

    #[test]
    fn build_produces_enabled_recorder() {
        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();
        assert!(rec.handle().is_enabled());
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    #[test]
    fn attach_produces_working_runtime() {
        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(2);
        let runtime = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder().runtime_name("main").build(),
            )
            .unwrap();

        runtime.block_on(async {
            let r = tokio::spawn(async { 42 }).await.unwrap();
            assert_eq!(r, 42);
        });

        drop(runtime);
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    #[cfg(tokio_unstable)]
    #[test]
    fn task_tracking_produces_task_spawn_events() {
        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(2);
        let runtime = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder()
                    .runtime_name("main")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        runtime.block_on(async {
            tokio::spawn(async { tokio::task::yield_now().await })
                .await
                .unwrap();
        });

        drop(runtime);
        rec.graceful_shutdown(Duration::from_secs(1));

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
    fn multiple_runtimes_get_unique_worker_ids() {
        use std::collections::HashSet;

        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.enable_all().worker_threads(2);
        let runtime_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder()
                    .runtime_name("main")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.enable_all().worker_threads(2);
        let runtime_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder()
                    .runtime_name("io")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        let worker_id = |runtime: &tokio::runtime::Runtime| {
            runtime.block_on(async {
                crate::telemetry::spawn(async { current_worker_id() })
                    .await
                    .unwrap()
            })
        };
        let runtime_a_id = worker_id(&runtime_a);
        let runtime_b_id = worker_id(&runtime_b);
        assert_ne!(runtime_a_id, crate::telemetry::format::WorkerId::UNKNOWN);
        assert_ne!(runtime_b_id, crate::telemetry::format::WorkerId::UNKNOWN);
        assert_ne!(runtime_a_id, runtime_b_id);

        // Read each runtime's worker-id block from the context registry instead
        // of assuming absolute ranges: IDs are reserved when a runtime's workers
        // first poll, so the blocks depend on drive order.
        let (main_ids, io_ids) = {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            let block = |name: &str| -> HashSet<u64> {
                registry
                    .iter()
                    .find(|c| c.runtime_name.as_deref() == Some(name))
                    .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                    .unwrap_or_default()
            };
            (block("main"), block("io"))
        };
        assert!(
            !main_ids.is_empty() && !io_ids.is_empty(),
            "each runtime should reserve worker IDs: main={main_ids:?} io={io_ids:?}"
        );
        assert!(
            main_ids.is_disjoint(&io_ids),
            "runtimes must not share worker IDs: main={main_ids:?} io={io_ids:?}"
        );

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));

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

        let has_runtime_a = worker_ids.iter().any(|id| main_ids.contains(id));
        let has_runtime_b = worker_ids.iter().any(|id| io_ids.contains(id));
        assert!(
            has_runtime_a && has_runtime_b,
            "expected worker IDs from both runtimes; observed={worker_ids:?} main={main_ids:?} io={io_ids:?}"
        );
    }

    /// A dial9-spawned future on a runtime dial9 never attached to records no
    /// polls: the thread has no runtime context, so there is nothing to
    /// attribute them to and the wrapper stays out of it.
    #[test]
    fn wrapper_leaves_unattached_runtimes_alone() {
        use crate::telemetry::recorder::{Dial9TokioHandle, traced_handle};

        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle =
            Dial9TokioHandle::for_runtime(runtime.handle().clone(), traced_handle(rec.handle()));
        runtime.block_on(async {
            handle.spawn(async {}).await.unwrap();
        });

        drop(runtime);
        rec.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let polls = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::telemetry::analysis_events::Dial9Event::PollStartEvent(..)
                        | crate::telemetry::analysis_events::Dial9Event::PollEndEvent(..)
                )
            })
            .count();
        assert_eq!(
            polls, 0,
            "an unattached runtime has no worker identity, so its polls stay out of the trace"
        );
    }

    #[test]
    fn block_in_place_keeps_worker_ids_bounded() {
        use crate::telemetry::analysis_events::Dial9Event;
        use crate::telemetry::format::WorkerId;
        use std::collections::HashSet;

        const WORKERS: usize = 2;
        const HANDOFFS: usize = 20;

        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(WORKERS);
        let rt = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder().runtime_name("main").build(),
            )
            .unwrap();

        rt.block_on(async {
            crate::telemetry::spawn(async {
                for _ in 0..HANDOFFS {
                    tokio::task::block_in_place(|| {
                        std::thread::sleep(Duration::from_millis(1));
                    });
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });

        // Read the worker set before shutdown: the registry goes away with the
        // recorder.
        let enrolled: HashSet<u64> = {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            registry
                .iter()
                .find(|c| c.runtime_name.as_deref() == Some("main"))
                .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                .unwrap_or_default()
        };

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(2));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let mut starts = 0usize;
        let mut ends = 0usize;
        let mut polled_by: HashSet<u64> = HashSet::new();
        for e in &events {
            match e {
                Dial9Event::PollStartEvent(p) => {
                    starts += 1;
                    polled_by.insert(p.worker_id.as_u64());
                }
                Dial9Event::PollEndEvent(p) => {
                    ends += 1;
                    polled_by.insert(p.worker_id.as_u64());
                }
                _ => {}
            }
        }

        assert!(starts > 0, "expected poll events from the spawned task");
        assert_eq!(
            starts, ends,
            "every PollStart needs a PollEnd across the core handoff"
        );
        assert!(
            !polled_by.contains(&WorkerId::UNKNOWN.as_u64()),
            "a thread running the worker loop must resolve an identity: {polled_by:?}"
        );
        assert!(
            polled_by.is_subset(&enrolled),
            "polls came from workers the runtime never enrolled: polled={polled_by:?} enrolled={enrolled:?}"
        );
        assert!(
            enrolled.len() < HANDOFFS,
            "worker IDs must be bounded by threads, not by handoffs: {enrolled:?}"
        );

        #[cfg(tokio_unstable)]
        assert!(
            enrolled.len() <= WORKERS,
            "worker_index() puts the migrant back in the runtime's own block: {enrolled:?}"
        );
        #[cfg(not(tokio_unstable))]
        assert!(
            enrolled.len() >= WORKERS,
            "each thread that runs the worker loop claims its own ID: {enrolled:?}"
        );
    }

    /// Attach two runtimes from one thread, then drive them. Each task's polls
    /// resolve the runtime they actually run on, so attach order cannot file
    /// one runtime's work under the other.
    #[cfg(not(tokio_unstable))]
    #[test]
    fn attach_order_does_not_misattribute_polls() {
        use std::collections::HashSet;

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap()).build();
        let attach = |name: &str| {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            builder.enable_all();
            rec.handle()
                .attach_tokio_runtime(
                    builder,
                    TokioAttachOptions::builder().runtime_name(name).build(),
                )
                .unwrap()
        };

        let runtime_a = attach("main");
        let runtime_b = attach("io");
        runtime_a.block_on(async {
            crate::telemetry::spawn(async {}).await.unwrap();
        });

        let block = |name: &str| -> HashSet<u64> {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            registry
                .iter()
                .find(|c| c.runtime_name.as_deref() == Some(name))
                .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                .unwrap_or_default()
        };
        assert!(
            !block("main").is_empty(),
            "the runtime that ran the task must own the worker: main={:?} io={:?}",
            block("main"),
            block("io")
        );
        assert!(
            block("io").is_empty(),
            "the idle runtime must own no workers: io={:?}",
            block("io")
        );

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// One thread driving two `current_thread` runtimes in turn. The worker
    /// caches are keyed by runtime, so the second runtime enrolls this thread
    /// under its own worker ID instead of short-circuiting on the first one's.
    #[cfg(not(tokio_unstable))]
    #[test]
    fn one_thread_driving_two_runtimes_enrolls_with_both() {
        use std::collections::HashSet;

        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap()).build();
        let attach = |name: &str| {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            builder.enable_all();
            rec.handle()
                .attach_tokio_runtime(
                    builder,
                    TokioAttachOptions::builder().runtime_name(name).build(),
                )
                .unwrap()
        };

        // Attach and drive in turn: attaching installs this thread's runtime
        // context, so each drive claims under the runtime it belongs to.
        // Several rounds, because a thread that only remembers its latest
        // runtime would claim a new ID on every switch.
        let runtime_a = attach("main");
        let runtime_b = attach("io");
        for _ in 0..3 {
            for runtime in [&runtime_a, &runtime_b] {
                runtime.block_on(async {
                    crate::telemetry::spawn(async {}).await.unwrap();
                });
            }
        }

        let (main_ids, io_ids) = {
            let registry = recorder_tokio::tokio_attach_state(rec.handle())
                .map(|s| s.registry)
                .expect("enabled recorder has a context registry");
            let registry = registry.lock().unwrap();
            let block = |name: &str| -> HashSet<u64> {
                registry
                    .iter()
                    .find(|c| c.runtime_name.as_deref() == Some(name))
                    .map(|c| c.worker_ids.lock().unwrap().iter().copied().collect())
                    .unwrap_or_default()
            };
            (block("main"), block("io"))
        };
        assert_eq!(
            (main_ids.len(), io_ids.len()),
            (1, 1),
            "one driving thread is one worker per runtime, however often it switches: \
             main={main_ids:?} io={io_ids:?}"
        );
        assert!(
            main_ids.is_disjoint(&io_ids),
            "runtimes must not share worker IDs: main={main_ids:?} io={io_ids:?}"
        );

        drop(runtime_a);
        drop(runtime_b);
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// A panicking poll still closes its span: PollEnd comes from a drop
    /// guard, so every PollStart has a matching PollEnd even when the future
    /// unwinds.
    #[cfg(not(tokio_unstable))]
    #[test]
    fn panicking_poll_still_emits_poll_end() {
        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let runtime = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder().runtime_name("main").build(),
            )
            .unwrap();

        runtime.block_on(async {
            crate::telemetry::spawn(async {}).await.unwrap();
            let panicked = crate::telemetry::spawn(async { panic!("poll panic") }).await;
            assert!(panicked.is_err(), "the panicking task must fail its join");
        });

        drop(runtime);
        rec.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let count = |want: fn(&crate::telemetry::analysis_events::Dial9Event) -> bool| {
            events.iter().filter(|e| want(e)).count()
        };
        let starts = count(|e| {
            matches!(
                e,
                crate::telemetry::analysis_events::Dial9Event::PollStartEvent(..)
            )
        });
        let ends = count(|e| {
            matches!(
                e,
                crate::telemetry::analysis_events::Dial9Event::PollEndEvent(..)
            )
        });
        assert!(starts > 0, "expected poll events from the spawned tasks");
        assert_eq!(
            starts, ends,
            "every PollStart needs a PollEnd, panic or not"
        );
    }

    // The public `handle.attach_tokio_runtime(..)` flow: one recorder, two runtimes
    // attached as feeds, driven and shut down by the caller. Both runtimes'
    // polls land in one trace.
    #[test]
    fn attach_runtime_self_managed_runtimes() {
        use std::collections::HashSet;

        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.enable_all().worker_threads(2);
        let rt_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder()
                    .runtime_name("a")
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.enable_all().worker_threads(2);
        let rt_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder().runtime_name("b").build(),
            )
            .unwrap();

        for rt in [&rt_a, &rt_b] {
            rt.block_on(async {
                let mut handles = Vec::new();
                for _ in 0..50 {
                    handles.push(crate::telemetry::spawn(async {
                        tokio::task::yield_now().await;
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        }

        drop(rt_a);
        drop(rt_b);
        rec.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let mut worker_ids: HashSet<u64> = HashSet::new();
        for event in &events {
            if let crate::telemetry::analysis_events::Dial9Event::PollStartEvent(e) = event
                && e.worker_id != crate::telemetry::analysis_events::WorkerId::UNKNOWN
            {
                worker_ids.insert(e.worker_id.as_u64());
            }
        }
        // Two 2-worker runtimes reserve disjoint blocks; each runs work, so at
        // least one worker from each contributes a poll.
        assert!(
            worker_ids.len() >= 2,
            "expected polls from both attached runtimes, got: {worker_ids:?}"
        );
    }

    // `spawn_in` instruments a task even when called from a thread outside any
    // dial9 runtime: the handle resolves lazily on the target runtime's worker
    // at first poll, so wake events are recorded for the spawned task.
    #[test]
    fn spawn_in_from_outside_runtime_records_wakes() {
        use crate::telemetry::task_metadata::TaskId;

        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(2);
        let rt = rec
            .handle()
            .attach_tokio_runtime(
                builder,
                TokioAttachOptions::builder()
                    .task_tracking_enabled(true)
                    .build(),
            )
            .unwrap();

        let spawned_id: Arc<std::sync::Mutex<Option<TaskId>>> =
            Arc::new(std::sync::Mutex::new(None));
        let spawned_write = spawned_id.clone();

        // Spawned from this test thread, which is not one of `rt`'s workers. The
        // task yields repeatedly; each yield self-wakes through the instrumented
        // waker on the worker, so WakeEvents are recorded iff lazy resolution
        // instrumented the task.
        let join = spawn_in(rt.handle(), async move {
            *spawned_write.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        });
        rt.block_on(async move { join.await.unwrap() });

        drop(rt);
        rec.graceful_shutdown(Duration::from_secs(1));

        let raw = data.lock().unwrap();
        let events = decode_captured(&raw);
        let expected = spawned_id
            .lock()
            .unwrap()
            .expect("spawn_in task should have run and recorded its id");
        let recorded_wake = events.iter().any(|e| {
            matches!(
                e,
                crate::telemetry::analysis_events::Dial9Event::WakeEvent(w)
                    if TaskId(w.woken_task_id) == expected
            )
        });
        assert!(
            recorded_wake,
            "spawn_in task's polls should be instrumented via lazy resolution, \
             but no WakeEvent for {expected:?} was recorded"
        );
    }

    /// Repeated `attach_tokio_runtime` calls share one runtime-context source. A second
    /// source would double-count the queue depth, so the count must stay at one
    /// however many runtimes are attached.
    #[test]
    fn repeated_attach_installs_one_source() {
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap()).build();
        // Owned clone so `source_count` doesn't borrow `rec`, which
        // `graceful_shutdown` moves.
        let shared = rec.shared().expect("live recorder").clone();

        let source_count = || {
            shared
                .with_sources_mut(|sources: &mut [Box<dyn source::Source>]| {
                    sources
                        .iter()
                        .filter(|s| s.name() == "tokio_runtimes")
                        .count()
                })
                .unwrap()
        };

        for _ in 0..3 {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            builder.enable_all();
            let runtime = rec
                .handle()
                .attach_tokio_runtime(builder, TokioAttachOptions::default())
                .unwrap();
            drop(runtime);
        }

        assert_eq!(source_count(), 1, "every attach shares one source");
        let registry = recorder_tokio::tokio_attach_state(rec.handle())
            .map(|s| s.registry)
            .expect("registry");
        assert_eq!(
            registry.lock().unwrap().len(),
            3,
            "each attach registers its own runtime context"
        );

        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// A [`Dial9TokioHandle`] bound to an attached runtime wraps spawned futures
    /// with wake tracking.
    #[test]
    fn tokio_handle_spawn_records_wakes() {
        let (capture, data) = CapturingProcessor::new();
        let rec = recorder(MemoryBuffer::new(CAPTURE_SIZE).unwrap())
            .pipe(capture)
            .build();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().worker_threads(2);
        let runtime = rec
            .handle()
            .attach_tokio_runtime(builder, TokioAttachOptions::default())
            .unwrap();

        let handle =
            Dial9TokioHandle::for_runtime(runtime.handle().clone(), traced_handle(rec.handle()));

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
            traced_handle(rec.handle())
                .expect("enabled recorder must yield a handle")
                .shared()
                .unwrap(),
        );

        drop(runtime);
        rec.graceful_shutdown(Duration::from_secs(1));

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

    /// A per-runtime [`Dial9TokioHandle`] must spawn on the runtime it was bound
    /// to, even when called from outside any runtime context.
    #[test]
    fn tokio_handle_spawns_on_correct_runtime_from_outside() {
        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();

        let mut builder_a = tokio::runtime::Builder::new_multi_thread();
        builder_a.worker_threads(1).enable_all().thread_name("rt-a");
        let rt_a = rec
            .handle()
            .attach_tokio_runtime(
                builder_a,
                TokioAttachOptions::builder().runtime_name("a").build(),
            )
            .unwrap();

        let mut builder_b = tokio::runtime::Builder::new_multi_thread();
        builder_b.worker_threads(1).enable_all().thread_name("rt-b");
        let rt_b = rec
            .handle()
            .attach_tokio_runtime(
                builder_b,
                TokioAttachOptions::builder().runtime_name("b").build(),
            )
            .unwrap();

        let handle_a =
            Dial9TokioHandle::for_runtime(rt_a.handle().clone(), traced_handle(rec.handle()));
        let handle_b =
            Dial9TokioHandle::for_runtime(rt_b.handle().clone(), traced_handle(rec.handle()));

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
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    // ---------------------------------------------------------------
    // Inert Dial9Handle
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
    fn disabled_recorder_graceful_shutdown_is_noop() {
        let rec = dial9_core::recorder::recorder_disabled();
        assert!(!rec.handle().is_enabled());
        rec.graceful_shutdown(Duration::from_secs(1));
    }

    /// Regression test for issue #400: multi-runtime callers must be able to
    /// configure S3 upload, via `.with_s3_uploader()` on the recorder builder.
    #[cfg(feature = "worker-s3")]
    #[test]
    fn recorder_builder_s3_config_builds_successfully() {
        use crate::telemetry::RecorderPipelineExt;
        use dial9_destinations_s3::S3Config;

        let s3 = S3Config::builder().bucket("b").service_name("s").build();

        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap())
            .with_s3_uploader(s3)
            .build();

        assert!(rec.handle().is_enabled());
        rec.graceful_shutdown(Duration::from_secs(1));
    }
}
