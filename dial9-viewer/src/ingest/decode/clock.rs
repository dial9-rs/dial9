//! Clock-domain newtypes and conversion boundary.
//!
//! Internally, trace events arrive in the monotonic clock domain (CLOCK_MONOTONIC
//! on the producer). To produce wall-clock timestamps for Parquet output, we add
//! a `ClockOffset` derived from a `ClockSyncEvent`. This module provides:
//!
//! - [`MonoNs`]: a monotonic-clock timestamp (nanoseconds).
//! - [`WallNs`]: a wall-clock (Unix epoch) timestamp (nanoseconds).
//! - [`DurationNs`]: an elapsed duration (nanoseconds).
//! - [`ClockOffset`]: the signed offset from monotonic to wall-clock.
//!
//! The newtypes prevent accidental mixing of clock domains. Raw `u64` is used
//! only at wire/public/Parquet boundaries.

/// A timestamp in the monotonic clock domain (nanoseconds since an unspecified
/// epoch, typically CLOCK_MONOTONIC on the producer host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MonoNs(pub(crate) u64);

/// A timestamp in the wall-clock domain (nanoseconds since Unix epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WallNs(pub(crate) u64);

/// A duration in nanoseconds. Always non-negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DurationNs(pub(crate) u64);

/// Signed offset from monotonic to wall-clock: `wall = mono + offset`.
///
/// Stored as i128 to handle the full range of `u64` timestamps without overflow.
/// Derived from `ClockSyncEvent`: `offset = realtime_ns - timestamp_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClockOffset(pub(crate) i128);

impl MonoNs {
    /// Convert to wall-clock using an offset previously validated for this
    /// trace's timestamp range.
    #[inline]
    fn to_wall(self, offset: ClockOffset) -> WallNs {
        offset
            .checked_to_wall(self)
            .expect("clock offset must be validated before conversion")
    }

    /// Convert to wall-clock if offset is available, otherwise pass through
    /// the raw value as a wall-clock timestamp (best-effort for traces without
    /// ClockSync events).
    #[inline]
    pub(crate) fn to_wall_or_raw(self, offset: Option<ClockOffset>) -> WallNs {
        match offset {
            Some(off) => self.to_wall(off),
            None => WallNs(self.0),
        }
    }

    /// Saturating subtraction in the monotonic clock domain.
    #[inline]
    pub(crate) fn saturating_sub(self, other: MonoNs) -> DurationNs {
        DurationNs(self.0.saturating_sub(other.0))
    }

    /// Raw u64 value (for wire/public boundaries only).
    #[inline]
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

impl WallNs {
    /// Raw u64 value (for Parquet/public output).
    #[inline]
    pub(crate) fn raw(self) -> u64 {
        self.0
    }

    /// Saturating subtraction producing a duration.
    #[inline]
    pub(crate) fn saturating_sub(self, other: WallNs) -> DurationNs {
        DurationNs(self.0.saturating_sub(other.0))
    }
}

impl DurationNs {
    pub(crate) const ZERO: Self = Self(0);

    #[inline]
    pub(crate) fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Raw u64 value.
    #[inline]
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

impl ClockOffset {
    /// Compute offset from a ClockSync event's fields.
    ///
    /// `realtime_ns` is wall-clock, `timestamp_ns` is monotonic.
    pub(crate) fn from_clock_sync(realtime_ns: u64, timestamp_ns: u64) -> Self {
        Self(realtime_ns as i128 - timestamp_ns as i128)
    }

    #[inline]
    pub(crate) fn checked_to_wall(self, timestamp: MonoNs) -> Option<WallNs> {
        let wall = timestamp.0 as i128 + self.0;
        u64::try_from(wall).ok().map(WallNs)
    }

    pub(crate) fn is_valid_for(self, min: MonoNs, max: MonoNs) -> bool {
        self.checked_to_wall(min).is_some() && self.checked_to_wall(max).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_wall_positive_offset() {
        let offset = ClockOffset::from_clock_sync(1_700_000_000_000_000_100, 100);
        let mono = MonoNs(500);
        let wall = offset.checked_to_wall(mono).unwrap();
        assert_eq!(wall.raw(), 1_700_000_000_000_000_500);
    }

    #[test]
    fn mono_to_wall_no_offset_passthrough() {
        let mono = MonoNs(42);
        let wall = mono.to_wall_or_raw(None);
        assert_eq!(wall.raw(), 42);
    }

    #[test]
    fn wall_saturating_sub() {
        let a = WallNs(100);
        let b = WallNs(30);
        assert_eq!(a.saturating_sub(b).raw(), 70);
        // Underflow saturates to 0
        assert_eq!(b.saturating_sub(a).raw(), 0);
    }

    #[test]
    fn negative_conversion_is_rejected() {
        let offset = ClockOffset(-(200i128));
        let mono = MonoNs(100);
        assert_eq!(offset.checked_to_wall(mono), None);
    }

    #[test]
    fn overflow_conversion_is_rejected() {
        let offset = ClockOffset(u64::MAX as i128);
        let mono = MonoNs(u64::MAX);
        assert_eq!(offset.checked_to_wall(mono), None);
    }

    #[test]
    fn offset_validation_checks_both_ends_of_trace_range() {
        assert!(ClockOffset(100).is_valid_for(MonoNs(0), MonoNs(1000)));
        assert!(!ClockOffset(-100).is_valid_for(MonoNs(0), MonoNs(1000)));
        assert!(!ClockOffset(u64::MAX as i128).is_valid_for(MonoNs(0), MonoNs(1000)));
    }
}
