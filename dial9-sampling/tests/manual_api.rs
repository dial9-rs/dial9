#![cfg(feature = "simulation")]
use dial9_sampling::{
    Sampling, StartupFailure,
    backend::{
        ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture, StoredObject,
    },
    simulation::MemoryStore,
};
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, oneshot};

fn policy() -> Sampling {
    Sampling::builder()
        .scope("manual/prod")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build()
}

fn bootstrap() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn session_owns_executor_and_sync_drop_releases_capacity() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<dial9_sampling::SamplingSession>();
    let store = Arc::new(MemoryStore::new());
    let (ping, request) = oneshot::channel();
    let (reply, pong) = oneshot::channel();
    // A borrowed non-Send factory remains supported.
    let factory_polled = Rc::new(Cell::new(false));
    let sampler = {
        let bootstrap = bootstrap();
        bootstrap.block_on(async {
            let sampler = policy()
                .prepare(async {
                    factory_polled.set(true);
                    assert_eq!(
                        tokio::runtime::Handle::current().runtime_flavor(),
                        tokio::runtime::RuntimeFlavor::MultiThread
                    );
                    tokio::spawn(async move {
                        request.await.unwrap();
                        reply.send(()).unwrap();
                    });
                    Ok(store.clone())
                })
                .await;
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::CurrentThread
            );
            sampler
        })
    };
    assert!(factory_polled.get());
    assert!(sampler.enabled());
    ping.send(()).unwrap();
    pong.blocking_recv().unwrap(); // Factory-spawned work survives bootstrap teardown.
    drop(sampler); // Synchronous Drop waits for bounded cleanup on the private executor.
    let bootstrap = bootstrap();
    let replacement = bootstrap.block_on(policy().prepare(async { Ok(store) }));
    assert!(replacement.enabled());
    drop(replacement);
}

#[derive(Debug)]
struct GatedRelease {
    store: MemoryStore,
    started: Semaphore,
    allow: Semaphore,
    finished: Semaphore,
}
impl GatedRelease {
    fn new() -> Self {
        Self {
            store: MemoryStore::new(),
            started: Semaphore::new(0),
            allow: Semaphore::new(0),
            finished: Semaphore::new(0),
        }
    }
}
impl SamplingStore for GatedRelease {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        self.store.get(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            let release = request.key().contains("/slots/")
                && serde_json::from_slice::<serde_json::Value>(request.body()).unwrap()["owner"]
                    .is_null();
            if release {
                self.started.add_permits(1);
                self.allow.acquire().await.unwrap().forget();
            }
            let result = self.store.put(request).await;
            if release {
                self.finished.add_permits(1);
            }
            result
        })
    }
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.store.list(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.store.delete(key)
    }
}

#[test]
fn async_drop_cleanup_survives_caller_executor_teardown() {
    let store = Arc::new(GatedRelease::new());
    {
        let bootstrap = bootstrap();
        bootstrap.block_on(async {
            let sampler = policy().prepare(async { Ok(store.clone()) }).await;
            assert!(sampler.enabled());
            drop(sampler);
            // Cleanup is pending, so Drop demonstrably didn't block the caller.
            tokio::time::timeout(Duration::from_secs(5), store.started.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        });
    }
    store.allow.add_permits(1);
    let bootstrap = bootstrap();
    bootstrap.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), store.finished.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let replacement = policy().prepare(async { Ok(store.clone()) }).await;
        assert!(replacement.enabled());
        store.allow.add_permits(1);
        replacement.shutdown(Duration::from_secs(1)).await;
    });
}

#[tokio::test]
async fn canceling_explicit_shutdown_does_not_cancel_release() {
    let store = Arc::new(GatedRelease::new());
    let sampler = policy().prepare(async { Ok(store.clone()) }).await;
    assert!(sampler.enabled());
    let shutdown = tokio::spawn(sampler.shutdown(Duration::from_secs(5)));
    tokio::time::timeout(Duration::from_secs(5), store.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    shutdown.abort();
    assert!(shutdown.await.unwrap_err().is_cancelled());
    store.allow.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), store.finished.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let replacement = policy().prepare(async { Ok(store.clone()) }).await;
    assert!(replacement.enabled());
    store.allow.add_permits(1);
    replacement.shutdown(Duration::from_secs(1)).await;
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn canceled_or_panicking_factory_shuts_down_private_runtime(#[case] panic: bool) {
    struct SignalDrop(Option<oneshot::Sender<()>>);
    impl Drop for SignalDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    let (started, ready) = oneshot::channel();
    let (dropped, ended) = oneshot::channel();
    let preparation = tokio::spawn(async move {
        policy()
            .prepare(async {
                let signal = SignalDrop(Some(dropped));
                tokio::spawn(async move {
                    let _signal = signal;
                    std::future::pending::<()>().await;
                });
                started.send(()).unwrap();
                assert!(!panic, "factory panic");
                std::future::pending::<()>().await;
                Ok::<_, std::io::Error>(MemoryStore::new())
            })
            .await
    });
    ready.await.unwrap();
    if !panic {
        preparation.abort();
    }
    let error = preparation.await.unwrap_err();
    assert_eq!(error.is_panic(), panic);
    tokio::time::timeout(Duration::from_secs(5), ended)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn dropping_sampler_on_its_own_single_worker_does_not_block_cleanup() {
    let store = Arc::new(GatedRelease::new());
    let holder = Arc::new(std::sync::Mutex::new(
        None::<dial9_sampling::SamplingSession>,
    ));
    let job_holder = holder.clone();
    let (drop_request, requested) = oneshot::channel();
    let (dropped, drop_completed) = oneshot::channel();
    let bootstrap = bootstrap();
    let sampler = bootstrap.block_on(policy().prepare(async {
        tokio::spawn(async move {
            requested.await.unwrap();
            let sampler = job_holder.lock().unwrap().take().unwrap();
            drop(sampler);
            dropped.send(()).unwrap();
        });
        Ok(store.clone())
    }));
    assert!(sampler.enabled());
    *holder.lock().unwrap() = Some(sampler);
    drop_request.send(()).unwrap();
    bootstrap.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), drop_completed)
            .await
            .unwrap()
            .unwrap();
        // The sole worker is available to drive cleanup after dropping its owner.
        tokio::time::timeout(Duration::from_secs(5), store.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        store.allow.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), store.finished.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let replacement = policy().prepare(async { Ok(store.clone()) }).await;
        assert!(replacement.enabled());
        store.allow.add_permits(1);
        replacement.shutdown(Duration::from_secs(1)).await;
    });
}
