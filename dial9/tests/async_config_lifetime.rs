#![cfg(feature = "tokio")]

use dial9::{AttachedRuntime, Dial9HandleTokioExt, TokioAttachOptions};
use std::{io, sync::Mutex, time::Duration};
use tokio::sync::oneshot;

struct NotifyOnDrop(std::sync::mpsc::Sender<()>);
impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.send(()).unwrap();
    }
}

struct State {
    start: Option<oneshot::Sender<()>>,
    result: Option<oneshot::Receiver<u32>>,
    stopped: std::sync::mpsc::Receiver<()>,
}
static STATE: Mutex<Option<State>> = Mutex::new(None);

async fn configure() -> io::Result<AttachedRuntime> {
    let (start, started) = oneshot::channel();
    let (result, completed) = oneshot::channel();
    let (stopped, stop_notice) = std::sync::mpsc::channel();
    let notice = NotifyOnDrop(stopped);
    tokio::spawn(async move {
        let _notice = notice;
        started.await.unwrap();
        result.send(42).unwrap();
        std::future::pending::<()>().await;
    });
    *STATE.lock().unwrap() = Some(State {
        start: Some(start),
        result: Some(completed),
        stopped: stop_notice,
    });
    let recorder = dial9::recorder_disabled();
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    runtime.enable_all();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(runtime, TokioAttachOptions::default())?;
    Ok((recorder, runtime))
}

async fn check_config_task() -> u32 {
    let (start, result) = {
        let mut state = STATE.lock().unwrap();
        let state = state.as_mut().unwrap();
        (state.start.take().unwrap(), state.result.take().unwrap())
    };
    // Only the application can release this task, after configure has returned.
    start
        .send(())
        .expect("configuration task must still be alive");
    tokio::time::timeout(Duration::from_secs(30), result)
        .await
        .expect("hang watchdog; no latency guarantee")
        .unwrap()
}

#[dial9::main(config_async = configure)]
async fn graceful_application() -> u32 {
    check_config_task().await
}

#[dial9::main(config_async = configure, disable_graceful_shutdown)]
async fn drop_application() -> u32 {
    check_config_task().await
}

#[dial9::main(config_async = configure)]
async fn panicking_application() {
    assert_eq!(check_config_task().await, 42);
    panic!("injected application panic");
}

fn check_task_stopped() {
    STATE
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .stopped
        .recv_timeout(Duration::from_secs(30))
        .expect("configuration task must stop when main exits");
}

#[rstest::rstest]
#[case(graceful_application)]
#[case(drop_application)]
fn config_tasks_progress_after_return_and_stop_with_main(#[case] application: fn() -> u32) {
    assert_eq!(application(), 42);
    check_task_stopped();
}

#[test]
fn config_tasks_stop_when_the_application_panics() {
    assert!(std::panic::catch_unwind(panicking_application).is_err());
    check_task_stopped();
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
fn config_tasks_stop_when_configuration_fails(#[case] panic: bool) {
    let (stopped, notice) = std::sync::mpsc::channel();
    let outcome = std::panic::catch_unwind(|| {
        dial9::__private::bootstrap_config_async(async move {
            let guard = NotifyOnDrop(stopped);
            tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            assert!(!panic, "injected configuration panic");
            Err(io::Error::other("injected configuration error"))
        })
    });
    if panic {
        assert!(outcome.is_err());
    } else {
        assert_eq!(
            outcome.unwrap().unwrap_err().to_string(),
            "injected configuration error"
        );
    }
    notice
        .recv_timeout(Duration::from_secs(30))
        .expect("configuration task must stop on initialization failure");
}
