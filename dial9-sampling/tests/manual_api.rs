//! Public startup API and application lifecycle regressions.
#![allow(clippy::unwrap_used, reason = "Test assertions fail immediately.")]

use dial9_sampling::{Sampling, SamplingStartup, SamplingStatus, StartupFailure, backend};
// Fault injection is covered by the sampler's unit tests.
#[allow(dead_code)]
#[path = "../src/test_store.rs"]
mod test_store;
use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use test_store::MemoryStore;
use tokio::sync::oneshot;

fn policy() -> Sampling {
    Sampling::builder()
        .num_target_profilers(1)
        .if_failed(StartupFailure::assume_population(1))
        .build()
}

struct SignalDrop(Option<oneshot::Sender<()>>);

impl Drop for SignalDrop {
    fn drop(&mut self) {
        if self.0.take().unwrap().send(()).is_err() {
            tracing::debug!("test observer dropped before sampler task ended");
        }
    }
}

fn assert_task_stopped(ended: oneshot::Receiver<()>) {
    let verifier = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    verifier
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), ended).await })
        .unwrap()
        .unwrap();
}

#[test]
fn unused_startup_never_polls_factory() {
    let polled = Cell::new(false);
    let startup: SamplingStartup<_> = policy().with_storage(async {
        polled.set(true);
        Ok(MemoryStore::new())
    });
    assert!(!polled.get());
    drop(startup);
    assert!(!polled.get());
}

#[test]
fn application_owns_its_runtime_while_sampler_survives_status_drop() {
    let store = Arc::new(MemoryStore::new());
    let factory_polled = Rc::new(Cell::new(false));
    let (ping, request) = oneshot::channel();
    let (reply, pong) = oneshot::channel();
    let policy = policy();
    let value = String::from("borrowed result");
    let result = policy
        .with_storage(async {
            factory_polled.set(true);
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            );
            tokio::spawn(async move {
                request.await.unwrap();
                reply.send(()).unwrap();
            });
            // A timer and spawned future are driven by the sampler's runtime.
            tokio::time::sleep(Duration::ZERO).await;
            Ok(store.clone())
        })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(enabled);
            assert!(factory_polled.get());
            assert!(tokio::runtime::Handle::try_current().is_err());
            let app = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            app.block_on(async {
                ping.send(()).unwrap();
                pong.await.unwrap();
            });
            drop(app);
            &value
        });
    assert!(std::ptr::eq(result, &value));
}

#[derive(Debug)]
enum Exit {
    Success,
    Error,
    Panic,
}

struct ApplicationTeardown<'a> {
    backend_alive: &'a AtomicBool,
    retained: &'a AtomicBool,
}
impl Drop for ApplicationTeardown<'_> {
    fn drop(&mut self) {
        self.retained
            .store(self.backend_alive.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

struct BackendTaskLifetime(Arc<AtomicBool>);
impl Drop for BackendTaskLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[rstest::rstest]
#[case(Exit::Success)]
#[case(Exit::Error)]
#[case(Exit::Panic)]
fn cleanup_follows_application_teardown_on_return_error_and_unwind(#[case] exit: Exit) {
    let store = Arc::new(MemoryStore::new());
    let policy = policy();
    let retained = AtomicBool::new(false);
    let backend_alive = Arc::new(AtomicBool::new(true));
    let value = Rc::new(String::from("borrowed error or result"));
    let (dropped, ended) = oneshot::channel();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        policy
            .with_storage(async {
                let notice = SignalDrop(Some(dropped));
                let lifetime = BackendTaskLifetime(backend_alive.clone());
                tokio::spawn(async move {
                    let _notice = notice;
                    let _lifetime = lifetime;
                    std::future::pending::<()>().await;
                });
                Ok(store.clone())
            })
            .start_application(|status| {
                let SamplingStatus { enabled, .. } = status;
                assert!(enabled);
                let _recorder = {
                    let recorder = dial9_core::recorder::recorder(
                        dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
                    )
                    .build();
                    status.register_recorder(&recorder);
                    recorder
                };
                let _teardown = ApplicationTeardown {
                    backend_alive: &backend_alive,
                    retained: &retained,
                };
                match exit {
                    Exit::Success => Ok(&value),
                    Exit::Error => Err(&value),
                    Exit::Panic => panic!("application panic"),
                }
            })
    }));
    match exit {
        Exit::Success => assert!(std::ptr::eq(outcome.unwrap().unwrap(), &value)),
        Exit::Error => assert!(std::ptr::eq(outcome.unwrap().unwrap_err(), &value)),
        Exit::Panic => assert!(outcome.is_err()),
    }
    assert!(retained.load(Ordering::SeqCst));
    assert_task_stopped(ended);
    assert!(!backend_alive.load(Ordering::SeqCst));
}

#[test]
fn returned_status_retains_no_runtime() {
    let store = Arc::new(MemoryStore::new());
    let (dropped, ended) = oneshot::channel();
    let status = policy()
        .with_storage(async {
            let notice = SignalDrop(Some(dropped));
            tokio::spawn(async move {
                let _notice = notice;
                std::future::pending::<()>().await;
            });
            Ok(store.clone())
        })
        .start_application(|status| status);
    assert!(status.enabled);
    assert_task_stopped(ended);
}

#[test]
fn registration_disables_returned_recorder_before_runtime_cleanup() {
    let store = Arc::new(MemoryStore::new());
    let recorder = policy()
        .with_storage(async { Ok(store.clone()) })
        .start_application(|status| {
            assert!(status.enabled);
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            drop(status);
            assert!(recorder.handle().is_enabled());
            recorder
        });
    assert!(!recorder.handle().is_enabled());
    recorder.graceful_shutdown(Duration::ZERO);
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn registration_rejects_disabled_or_ended_invocation(#[case] ended: bool) {
    let recorder =
        dial9_core::recorder::recorder(dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap())
            .build();
    let sampling = policy();
    if ended {
        let status = sampling
            .with_storage(async { Ok(MemoryStore::new()) })
            .start_application(|status| status);
        assert!(status.enabled);
        status.register_recorder(&recorder);
    } else {
        Sampling::builder()
            .num_target_profilers(0)
            .if_failed(StartupFailure::disabled())
            .build()
            .with_storage(async { Ok(MemoryStore::new()) })
            .start_application(|status| {
                assert!(!status.enabled);
                status.register_recorder(&recorder);
                assert!(!recorder.handle().is_enabled());
            });
    }
    assert!(!recorder.handle().is_enabled());
}

#[test]
fn panicking_factory_shuts_down_its_spawned_tasks_without_starting_application() {
    let (dropped, ended) = oneshot::channel();
    let application_called = Cell::new(false);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        policy()
            .with_storage(async {
                let signal = SignalDrop(Some(dropped));
                tokio::spawn(async move {
                    let _signal = signal;
                    std::future::pending::<()>().await;
                });
                panic!("factory panic");
                #[allow(unreachable_code)]
                Ok(MemoryStore::new())
            })
            .start_application(|_| application_called.set(true))
    }));
    assert!(outcome.is_err());
    assert!(!application_called.get());
    assert_task_stopped(ended);
}

#[tokio::test]
async fn entered_runtime_is_rejected_before_polling_or_application() {
    let factory_called = Cell::new(false);
    let application_called = Cell::new(false);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        policy()
            .with_storage(async {
                factory_called.set(true);
                Ok(MemoryStore::new())
            })
            .start_application(|_| application_called.set(true))
    }));
    assert!(outcome.is_err());
    assert!(!factory_called.get());
    assert!(!application_called.get());
}

#[rstest::rstest]
#[case(StartupFailure::disabled(), false)]
#[case(StartupFailure::assume_population(1), true)]
fn backend_error_still_starts_application_with_fallback(
    #[case] fallback: StartupFailure,
    #[case] expected: bool,
) {
    let enabled = Sampling::builder()
        .num_target_profilers(1)
        .if_failed(fallback)
        .build()
        .with_storage(async { Err::<MemoryStore, _>(std::io::Error::other("backend failed")) })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            enabled
        });
    assert_eq!(enabled, expected);
}

#[rstest::rstest]
#[case(StartupFailure::assume_population(1), true)]
#[case(StartupFailure::disabled(), false)]
fn fallback_recorder_controls_and_cleanup(#[case] failure: StartupFailure, #[case] enabled: bool) {
    let (status, recorder) = Sampling::builder()
        .num_target_profilers(1)
        .startup_timeout(Duration::ZERO)
        .if_failed(failure)
        .build()
        .with_storage(async {
            panic!("zero budget must not poll the backend");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application(|status| {
            assert_eq!(status.enabled, enabled);
            assert_eq!(status.initial_expansion_weight, enabled.then_some(1.0));
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            assert_eq!(recorder.handle().is_enabled(), enabled);
            recorder.enable();
            assert!(recorder.handle().is_enabled());
            (status, recorder)
        });
    assert_eq!(
        status.enabled, enabled,
        "the public startup choice is frozen"
    );
    assert!(!recorder.handle().is_enabled());
    recorder.enable();
    assert!(recorder.handle().is_enabled());
}

#[rstest::rstest]
#[case(false, "1")]
#[case(true, "0")]
fn registered_recorder_emits_evaluated_weight_and_existing_trace_metadata(
    #[case] after_cleanup: bool,
    #[case] expected_weight: &str,
) {
    #[derive(dial9::format::TraceEvent)]
    struct SamplingMarker {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
    }

    let path = std::env::temp_dir().join(format!("sampling-public-{}.bin", ulid::Ulid::new()));
    let sealed = path.with_extension("0.bin");
    let recorder = Sampling::builder()
        .add_dimension("environment", "customer/prod")
        .num_target_profilers(1)
        .startup_timeout(Duration::ZERO)
        .if_failed(StartupFailure::assume_population(1))
        .build()
        .with_storage(async { Ok(MemoryStore::new()) })
        .start_application(|status| {
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::DiskBuffer::single_file(&path).unwrap(),
            )
            .segment_metadata([("boot_id".into(), "existing-boot-id".into())])
            .build();
            status.register_recorder(&recorder);
            status.register_recorder(&recorder);
            assert!(recorder.handle().is_enabled(), "registration is idempotent");
            recorder.handle().record_event(SamplingMarker {
                timestamp_ns: dial9_core::clock::clock_monotonic_ns(),
            });
            if after_cleanup {
                Some(recorder)
            } else {
                recorder.graceful_shutdown(Duration::ZERO);
                None
            }
        });
    if let Some(recorder) = recorder {
        recorder.graceful_shutdown(Duration::ZERO);
    }
    let bytes = std::fs::read(&sealed).unwrap();
    std::fs::remove_file(sealed).unwrap();
    let mut metadata = std::collections::HashMap::new();
    let mut clock_syncs = 0;
    dial9::format::Decoder::new(&bytes)
        .unwrap()
        .for_each_event(|event| {
            if event.name == "SegmentMetadataEvent"
                && let Some(dial9::format::types::FieldValueRef::StringMap(entries)) =
                    event.fields.first()
            {
                metadata.extend(
                    entries
                        .iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned())),
                );
            } else if event.name == "ClockSyncEvent" {
                clock_syncs += 1;
            }
        })
        .unwrap();
    assert_eq!(metadata["dial9.sampling.weight"], expected_weight);
    assert_eq!(
        metadata["dial9.sampling.pool"],
        "environment=customer%2Fprod/"
    );
    assert_eq!(metadata["boot_id"], "existing-boot-id");
    assert_eq!(
        metadata
            .keys()
            .filter(|key| key.starts_with("dial9.sampling."))
            .count(),
        2
    );
    assert!(clock_syncs > 0);
}

#[test]
fn recorder_cannot_be_registered_to_two_live_sampling_invocations() {
    let sampling = policy();
    sampling
        .clone()
        .with_storage(async { Ok(MemoryStore::new()) })
        .start_application(|first| {
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            first.register_recorder(&recorder);
            assert!(recorder.handle().is_enabled());
            sampling
                .with_storage(async { Ok(MemoryStore::new()) })
                .start_application(|second| {
                    second.register_recorder(&recorder);
                    assert!(!recorder.handle().is_enabled());
                });
            recorder.enable();
            assert!(
                recorder.handle().is_enabled(),
                "normal enable remains available"
            );
        });
}

#[rstest::rstest]
#[case("valid", 0, Duration::from_millis(1000))]
#[case("", 1, Duration::from_millis(1000))]
#[case("valid", 1, Duration::MAX)]
#[case("valid", 1, Duration::ZERO)]
fn zero_or_invalid_startup_does_not_poll_storage(
    #[case] dimension: &str,
    #[case] target: u32,
    #[case] timeout: Duration,
) {
    let enabled = Sampling::builder()
        .add_dimension("test", dimension)
        .num_target_profilers(target)
        .startup_timeout(timeout)
        .if_failed(StartupFailure::disabled())
        .build()
        .with_storage(async {
            panic!("preflight or exhausted budget must not initialize storage");
            #[allow(unreachable_code)]
            Ok(MemoryStore::new())
        })
        .start_application(|SamplingStatus { enabled, .. }| enabled);
    assert!(!enabled);
}

#[test]
fn cold_disabled_recorder_stays_off_when_later_startups_use_the_live_census() {
    use backend::{
        ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture, StoredObject,
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct ObservePopulation {
        inner: Arc<MemoryStore>,
        ready: Mutex<Option<oneshot::Sender<()>>>,
    }
    impl SamplingStore for ObservePopulation {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.inner.get(key)
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            Box::pin(async move {
                let has_population = if request.key().ends_with("/roster.json") {
                    serde_json::from_slice::<serde_json::Value>(request.body()).unwrap()["population"]
                        .is_object()
                } else {
                    assert!(request.key().starts_with("v3/members/"));
                    false
                };
                let outcome = self.inner.put(request).await?;
                if has_population
                    && matches!(outcome, PutOutcome::Stored { .. })
                    && let Some(ready) = self.ready.lock().unwrap().take()
                {
                    ready.send(()).unwrap();
                }
                Ok(outcome)
            })
        }
        fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.inner.list_page(request)
        }
        fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
            self.inner.delete(key)
        }
    }

    let store = Arc::new(MemoryStore::new());
    let sampling = Sampling::builder()
        .num_target_profilers(1)
        .poll_interval(Duration::from_millis(100))
        .bucket_width(Duration::from_secs(1))
        .max_clock_skew(Duration::ZERO)
        .if_failed(StartupFailure::disabled())
        .build();
    let (ready, population) = oneshot::channel();
    sampling
        .clone()
        .with_storage(async {
            Ok(ObservePopulation {
                inner: store.clone(),
                ready: Mutex::new(Some(ready)),
            })
        })
        .start_application(|status| {
            assert!(!status.enabled);
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            drop(status);
            let app = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Wait for an actual published census, rather than advancing a
            // fabricated session or assuming that a fixed delay was sufficient.
            app.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), population)
                    .await
                    .unwrap()
                    .unwrap();
            });
            sampling
                .with_storage(async { Ok(store.clone()) })
                .start_application(|SamplingStatus { enabled, .. }| assert!(enabled));
            assert!(!recorder.handle().is_enabled());
        });
}

#[test]
fn maintenance_termination_disables_registered_recorder_after_status_drop() {
    use backend::{
        ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture, StoredObject,
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct EndMaintenance {
        inner: MemoryStore,
        trigger: Mutex<Option<oneshot::Receiver<()>>>,
    }
    impl SamplingStore for EndMaintenance {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.inner.get(key)
        }
        fn put(&self, _: PutRequest) -> StoreFuture<'_, PutOutcome> {
            Box::pin(async move {
                let trigger = self.trigger.lock().unwrap().take().unwrap();
                trigger.await.unwrap();
                panic!("terminate the maintenance task after public registration")
            })
        }
        fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.inner.list_page(request)
        }
        fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
            self.inner.delete(key)
        }
    }

    let (trigger, end) = oneshot::channel();
    let recorder = policy()
        .with_storage(async {
            Ok(EndMaintenance {
                inner: MemoryStore::new(),
                trigger: Mutex::new(Some(end)),
            })
        })
        .start_application(|status| {
            assert!(status.enabled);
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            drop(status);
            assert!(recorder.handle().is_enabled());
            trigger.send(()).unwrap();
            let app = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            app.block_on(async {
                // Await the recorder's public state; the timeout is only a
                // watchdog, not an assumed maintenance or notification delay.
                tokio::time::timeout(Duration::from_secs(5), async {
                    while recorder.handle().is_enabled() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            });
            recorder.enable();
            assert!(recorder.handle().is_enabled());
            recorder
        });
    assert!(
        !recorder.handle().is_enabled(),
        "application cleanup still disables"
    );
}
