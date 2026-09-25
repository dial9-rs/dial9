#[cfg(feature = "taskdump")]
mod capture;
#[cfg(feature = "taskdump")]
mod legacy;
#[cfg(feature = "taskdump")]
mod sampled;
mod sampler;
#[cfg(any(feature = "taskdump", all(test, shuttle)))]
pub(crate) mod worker;

#[cfg(feature = "taskdump")]
pub(crate) use legacy::{clear_taskdump_config, set_taskdump_config};
#[cfg(feature = "taskdump")]
pub(crate) use sampled::{clear_worker_sampler, set_worker_sampler};

#[cfg(feature = "taskdump")]
pub(crate) type TaskDumped<F> =
    futures_util::future::Either<legacy::TaskDumped<F>, sampled::TaskSampled<F>>;

#[cfg(feature = "taskdump")]
crate::primitives::thread_local! {
    static SAMPLING_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Set the runtime's policy even while recording is paused and no sampler exists.
#[cfg(feature = "taskdump")]
pub(crate) fn set_capture_config(config: Option<crate::telemetry::TaskDumpConfig>, sampled: bool) {
    match config {
        Some(config) => set_taskdump_config(config),
        None => clear_taskdump_config(),
    }
    SAMPLING_ENABLED.with(|enabled| enabled.set(sampled));
}

/// Select the capture policy on the task's first poll, inside its runtime.
#[cfg(feature = "taskdump")]
pub(crate) fn wrap<F>(
    inner: F,
    handle: dial9_core::handle::Dial9Handle,
    task_id: crate::telemetry::task_metadata::TaskId,
) -> TaskDumped<F> {
    if SAMPLING_ENABLED.with(|enabled| enabled.get()) {
        futures_util::future::Either::Right(sampled::TaskSampled::new(inner, handle, task_id))
    } else {
        futures_util::future::Either::Left(legacy::TaskDumped::new(inner, handle, task_id))
    }
}
