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
        .scope("application/prod")
        .target(1)
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
            .with_backend(async {
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
        .with_backend(async {
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

#[cfg(feature = "dial9")]
#[test]
fn registration_disables_returned_recorder_before_runtime_cleanup() {
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
            .target(0)
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
#[case(StartupFailure::disabled(), false)]
#[case(StartupFailure::assume_population(1), true)]
fn backend_error_still_starts_application_with_fallback(
    #[case] fallback: StartupFailure,
    #[case] expected: bool,
) {
    let enabled = Sampling::builder()
        .scope("failure")
        .target(1)
        .if_failed(fallback)
        .build()
        .with_backend(async { Err::<MemoryStore, _>(std::io::Error::other("backend failed")) })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            enabled
        });
    assert_eq!(enabled, expected);
}
