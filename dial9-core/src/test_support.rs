//! Shared helpers for this crate's own `#[cfg(test)]` shuttle scenarios.

/// Pins the ambient clock to a fixed instant for the guard's lifetime, so a
/// scenario's rotation/seal-time checks read reproducible time instead of
/// real elapsed time, which can differ between a shuttle replay's record
/// and re-run passes.
pub(crate) fn pin_fixed_clock() -> metrique_timesource::ThreadLocalTimeSourceGuard {
    metrique_timesource::set_time_source(metrique_timesource::TimeSource::custom(
        metrique_timesource::fakes::StaticTimeSource::at_time(std::time::UNIX_EPOCH),
    ))
}

/// Call `f` with each sealed segment's raw bytes, draining `fs` until empty.
/// The in-memory backend pops one sealed segment per `take_files()` call, so
/// this keeps calling it until none are left rather than assuming one call
/// drains everything.
pub(crate) fn for_each_sealed_segment(fs: &crate::fs::Fs, mut f: impl FnMut(Vec<u8>)) {
    loop {
        let taken = fs.take_files();
        if taken.segments.is_empty() {
            break;
        }
        for seg in taken.segments {
            let (_seg_ref, payload, _accounting) = seg.load().unwrap();
            f(payload.into_vec());
        }
    }
}
