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
        .scope("application/prod")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build()
}

fn selected(policy: &Sampling, store: &Arc<MemoryStore>) -> bool {
    policy
        .with_backend(async { Ok(store.clone()) })
        .start_application(|SamplingStatus { enabled, .. }| enabled)
}

struct SignalDrop(Option<oneshot::Sender<()>>);

impl Drop for SignalDrop {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(());
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
    let startup: SamplingStartup<_> = policy().with_backend(async {
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
        .with_backend(async {
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
            assert!(
                !selected(&policy, &store),
                "status drop must not release the lease"
            );
            let app = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            app.block_on(async {
                ping.send(()).unwrap();
                pong.await.unwrap();
            });
            drop(app);
            assert!(!selected(&policy, &store));
            &value
        });
    assert!(std::ptr::eq(result, &value));
    assert!(selected(&policy, &store), "return must release capacity");
}

#[derive(Debug)]
enum Exit {
    Success,
    Error,
    Panic,
}

struct ApplicationTeardown<'a> {
    policy: &'a Sampling,
    store: &'a Arc<MemoryStore>,
    retained: &'a AtomicBool,
}
impl Drop for ApplicationTeardown<'_> {
    fn drop(&mut self) {
        self.retained
            .store(!selected(self.policy, self.store), Ordering::SeqCst);
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
    let value = Rc::new(String::from("borrowed error or result"));
    let (dropped, ended) = oneshot::channel();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        policy
            .with_backend(async {
                let notice = SignalDrop(Some(dropped));
                tokio::spawn(async move {
                    let _notice = notice;
                    std::future::pending::<()>().await;
                });
                Ok(store.clone())
            })
            .start_application(|status| {
                let SamplingStatus { enabled, .. } = status;
                assert!(enabled);
                #[cfg(feature = "dial9")]
                let _recorder = {
                    let recorder = dial9_core::recorder::recorder(
                        dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
                    )
                    .build();
                    status.register_recorder(&recorder);
                    recorder
                };
                let _teardown = ApplicationTeardown {
                    policy: &policy,
                    store: &store,
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
    assert!(selected(&policy, &store));
}

#[test]
fn returned_status_retains_no_capacity() {
    let store = Arc::new(MemoryStore::new());
    let status = policy()
        .with_backend(async { Ok(store.clone()) })
        .start_application(|status| status);
    assert!(status.enabled);
    assert!(selected(&policy(), &store));
}

#[cfg(feature = "dial9")]
#[test]
fn registration_disables_returned_recorder_before_releasing_capacity() {
    let store = Arc::new(MemoryStore::new());
    let recorder = policy()
        .with_backend(async { Ok(store.clone()) })
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
    assert!(selected(&policy(), &store));
    recorder.graceful_shutdown(Duration::ZERO);
}

#[cfg(feature = "dial9")]
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
            .with_backend(async { Ok(MemoryStore::new()) })
            .start_application(|status| status);
        assert!(status.enabled);
        status.register_recorder(&recorder);
    } else {
        Sampling::builder()
            .scope("disabled-registration")
            .exact_target(0)
            .if_failed(StartupFailure::disabled())
            .build()
            .with_backend(async { Ok(MemoryStore::new()) })
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
            .with_backend(async {
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
            .with_backend(async {
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
#[case(0.0, false)]
#[case(1.0, true)]
fn backend_error_still_starts_application_with_fallback(
    #[case] probability: f64,
    #[case] expected: bool,
) {
    let enabled = Sampling::builder()
        .scope("failure")
        .exact_target(1)
        .if_failed(StartupFailure::enable_with_fixed_probability(probability))
        .build()
        .with_backend(async { Err::<MemoryStore, _>(std::io::Error::other("backend failed")) })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            enabled
        });
    assert_eq!(enabled, expected);
}
