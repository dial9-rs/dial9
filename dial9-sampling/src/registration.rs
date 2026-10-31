use dial9_core::primitives::sync::Mutex;
use std::sync::Arc;
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};

pub(crate) type DisableCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct State {
    closed: bool,
    recorders: Vec<(DisableCallback, Option<JoinHandle<()>>)>,
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
        disable: DisableCallback,
        changes: Option<watch::Receiver<bool>>,
    ) {
        let mut state = self.state.lock().expect("recorder registration lock");
        if state.closed {
            drop(state);
            disable();
            return;
        }
        let monitor = changes.map(|mut changes| {
            let disable = disable.clone();
            self.runtime
                .as_ref()
                .expect("coordinated sampling runtime")
                .spawn(async move {
                    // A trim or maintenance termination stops recording once.
                    while *changes.borrow_and_update() {
                        if changes.changed().await.is_err() {
                            break;
                        }
                    }
                    disable();
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
            disable();
        }
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test(start_paused = true)]
    async fn terminal_or_closed_notice_disables_recorder(#[case] maintenance_ended: bool) {
        let registrations = RecorderRegistrations::new(Some(Handle::current()));
        let recorder =
            dial9_core::recorder::recorder(dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap())
                .build();
        let handle = recorder.handle().clone();
        let (recording, changes) = tokio::sync::watch::channel(true);
        let (disabled, notice) = tokio::sync::oneshot::channel();
        let disabled = Mutex::new(Some(disabled));
        registrations.register(
            Arc::new(move || {
                handle.disable();
                if let Some(disabled) = disabled.lock().unwrap().take() {
                    disabled.send(()).unwrap();
                }
            }),
            Some(changes),
        );
        if maintenance_ended {
            drop(recording);
        } else {
            recording.send(false).unwrap();
        }
        notice.await.unwrap();
        assert!(!recorder.handle().is_enabled());
        recorder.enable();
        assert!(recorder.handle().is_enabled());
        registrations.stop();
        assert!(!recorder.handle().is_enabled());
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use dial9_core::primitives::sync::atomic::{AtomicUsize, Ordering};

    dial9_core::shuttle_test! {
        default;
        fn shuttle_registration_racing_cleanup_stops_recording() {
            let registrations = Arc::new(RecorderRegistrations::new(None));
            let calls = Arc::new(AtomicUsize::new(0));
            let notice = calls.clone();
            let disable: DisableCallback = Arc::new(move || { notice.fetch_add(1, Ordering::SeqCst); });
            let other = registrations.clone();
            let worker = shuttle::thread::spawn(move || {
                other.register(disable, None);
            });
            registrations.stop();
            worker.join().unwrap();
            registrations.stop();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
