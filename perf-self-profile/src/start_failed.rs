//! [`StartFailed`], the placeholder registered for a profiler that failed to
//! start.

/// Registered in place of a profiling source `T` that failed to start, so
/// [`Dial9Handle::with_source`](dial9_core::handle::Dial9Handle::with_source)
/// can tell "configured but failed" apart from "not configured":
///
/// ```no_run
/// # fn f(handle: dial9_core::handle::Dial9Handle) {
/// use dial9_perf_self_profile::{CpuProfiler, StartFailed};
///
/// let error = handle.with_source(|f: &mut StartFailed<CpuProfiler>| f.message().to_string());
/// if let Some(error) = error {
///     eprintln!("CPU profiling failed to start: {error}");
/// }
/// # }
/// ```
///
/// Records nothing; it only reports `cpu.profile.start_error` or
/// `sched.profile.start_error` as segment metadata.
pub struct StartFailed<T: 'static> {
    kind: std::io::ErrorKind,
    message: String,
    metadata_emitted: bool,
    _source: std::marker::PhantomData<fn() -> T>,
}

/// The profilers a [`StartFailed`] can stand in for.
mod start_failed_sealed {
    pub trait Profiler: 'static {
        /// Prefix of the profiler's segment metadata keys.
        const METADATA_PREFIX: &'static str;
        /// [`Source::name`](dial9_core::source::Source::name) of the placeholder.
        const FAILED_SOURCE_NAME: &'static str;
    }

    impl Profiler for crate::CpuProfiler {
        const METADATA_PREFIX: &'static str = "cpu.profile";
        const FAILED_SOURCE_NAME: &'static str = "cpu_profile_start_failed";
    }

    impl Profiler for crate::SchedProfiler {
        const METADATA_PREFIX: &'static str = "sched.profile";
        const FAILED_SOURCE_NAME: &'static str = "sched_start_failed";
    }
}

impl<T: start_failed_sealed::Profiler> StartFailed<T> {
    pub(crate) fn new(error: &std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
            metadata_emitted: false,
            _source: std::marker::PhantomData,
        }
    }

    /// Kind of the start error. On Linux, blocked perf can also report
    /// [`Unsupported`](std::io::ErrorKind::Unsupported), so the kind alone
    /// doesn't say whether the platform has a backend.
    pub fn kind(&self) -> std::io::ErrorKind {
        self.kind
    }

    /// The start error's message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl<T: 'static> std::fmt::Debug for StartFailed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartFailed")
            .field("source", &std::any::type_name::<T>())
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish()
    }
}

impl<T: start_failed_sealed::Profiler> dial9_core::source::Source for StartFailed<T> {
    fn flush(&mut self, _ctx: &dial9_core::source::FlushContext<'_>) {}

    fn name(&self) -> &'static str {
        T::FAILED_SOURCE_NAME
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        if self.metadata_emitted {
            return;
        }
        self.metadata_emitted = true;
        out.push((
            format!("{}.start_error", T::METADATA_PREFIX),
            self.message.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::StartFailed;
    use crate::CpuProfiler;
    use dial9_core::source::Source;

    #[test]
    fn writes_the_start_error_once() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let mut failed = StartFailed::<CpuProfiler>::new(&error);
        assert_eq!(failed.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(failed.message(), "denied");
        let mut first = Vec::new();
        failed.segment_metadata(&mut first);
        assert_eq!(
            first,
            [("cpu.profile.start_error".to_string(), "denied".to_string())]
        );
        let mut second = Vec::new();
        failed.segment_metadata(&mut second);
        assert!(
            second.is_empty(),
            "emitted once; the writer keeps it for later segments"
        );
    }

    /// Each profiler's placeholder has its own metadata key and name.
    #[test]
    fn keys_and_names_follow_the_profiler() {
        let error = std::io::Error::other("denied");
        let mut sched = StartFailed::<crate::SchedProfiler>::new(&error);
        let mut out = Vec::new();
        sched.segment_metadata(&mut out);
        assert_eq!(
            out,
            [(
                "sched.profile.start_error".to_string(),
                "denied".to_string()
            )]
        );
        assert_eq!(sched.name(), "sched_start_failed");
        assert_eq!(
            StartFailed::<CpuProfiler>::new(&error).name(),
            "cpu_profile_start_failed"
        );
    }
}
