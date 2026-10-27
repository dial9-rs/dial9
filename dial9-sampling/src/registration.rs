use dial9_core::primitives::sync::Mutex;
use std::{future::Future, sync::Arc};
use tokio::{runtime::Handle, task::JoinHandle};

type DisableCallback = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub(crate) struct StopAction(Arc<Mutex<Option<DisableCallback>>>);

impl StopAction {
    pub(crate) fn new(disable: impl FnOnce() + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(disable)))))
    }

    pub(crate) fn run(&self) {
        let disable = self.0.lock().expect("recorder stop lock").take();
        if let Some(disable) = disable {
            disable();
        }
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    recorders: Vec<(StopAction, Option<JoinHandle<()>>)>,
}

pub(crate) struct RecorderRegistrations {
    runtime: Option<Handle>,
    state: Mutex<State>,
}

impl RecorderRegistrations {
    pub(crate) fn new(runtime: Option<Handle>) -> Self {
        Self {
            runtime,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn register(
        &self,
        disable: StopAction,
        stopped: Option<impl Future<Output = ()> + Send + 'static>,
    ) {
        let mut state = self.state.lock().expect("recorder registration lock");
        if state.closed {
            drop(state);
            disable.run();
            return;
        }
        let monitor = stopped.map(|stopped| {
            let disable = disable.clone();
            self.runtime
                .as_ref()
                .expect("coordinated sampling runtime")
                .spawn(async move {
                    stopped.await;
                    disable.run();
                })
        });
        state.recorders.push((disable, monitor));
    }

    pub(crate) fn stop(&self) {
        let recorders = {
            let mut state = self.state.lock().expect("recorder registration lock");
            state.closed = true;
            std::mem::take(&mut state.recorders)
        };
        for (disable, monitor) in recorders {
            if let Some(monitor) = monitor {
                monitor.abort();
            }
            disable.run();
        }
    }

    #[cfg(all(test, not(shuttle)))]
    pub(crate) async fn wait_for_stops(&self) {
        let monitors: Vec<_> = self
            .state
            .lock()
            .expect("recorder registration lock")
            .recorders
            .iter_mut()
            .filter_map(|(_, monitor)| monitor.take())
            .collect();
        for monitor in monitors {
            monitor.await.expect("recorder stop monitor");
        }
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use dial9_core::primitives::sync::atomic::{AtomicUsize, Ordering};

    dial9_core::shuttle_test! {
        default;
        fn shuttle_concurrent_stop_actions_run_once() {
            let calls = Arc::new(AtomicUsize::new(0));
            let notice = calls.clone();
            let disable = StopAction::new(move || { notice.fetch_add(1, Ordering::SeqCst); });
            let other = disable.clone();
            let worker = shuttle::thread::spawn(move || other.run());
            disable.run();
            worker.join().unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    dial9_core::shuttle_test! {
        default;
        fn shuttle_registration_racing_cleanup_stops_recording() {
            let registrations = Arc::new(RecorderRegistrations::new(None));
            let calls = Arc::new(AtomicUsize::new(0));
            let notice = calls.clone();
            let disable = StopAction::new(move || { notice.fetch_add(1, Ordering::SeqCst); });
            let other = registrations.clone();
            let worker = shuttle::thread::spawn(move || {
                other.register(disable, None::<std::future::Ready<()>>);
            });
            registrations.stop();
            worker.join().unwrap();
            registrations.stop();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
