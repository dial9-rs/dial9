//! Common ResolvedSpan construction and accounting.
//!
//! Both modern and legacy span adapters produce the same output type
//! ([`ResolvedSpan`]) but differ in how they supply evidence. This module
//! provides [`SpanCandidate`] — a builder-like evidence struct that adapters
//! populate — and [`SpanCandidate::finalize`] which produces the final
//! [`ResolvedSpan`] with checked accounting and consistent quality/completeness
//! semantics.
//!
//! The five-way time attribution invariant is:
//! ```text
//! elapsed_ns = on_cpu_ns_est + blocked_ns_est + async_wait_ns + scheduler_delay_ns + unknown_ns
//! ```
//! (nullable estimates contribute zero when null)

use super::ResolvedSpan;
use super::clock::{ClockOffset, MonoNs};
use super::interval_pairing::{self, MonoInterval};

/// Evidence for building a [`ResolvedSpan`]. Adapters (modern/legacy) fill in
/// what they can resolve; [`finalize`](Self::finalize) computes derived fields
/// and enforces invariants.
pub(crate) struct SpanCandidate {
    // ── Identity ─────────────────────────────────────────────────────────
    pub(crate) boot_id: String,
    pub(crate) instance_id: u64,
    pub(crate) kind: &'static str,
    /// Display/runtime name written to the public row.
    pub(crate) name: String,
    /// Optional schema-level discriminator used only for span_type_uid. This
    /// keeps struct-derived `SpanEnter__Type` schemas distinct even when they
    /// share a dynamic runtime span name.
    pub(crate) type_name: Option<String>,
    pub(crate) target: String,
    pub(crate) callsite_file: Option<String>,
    pub(crate) callsite_line: Option<u32>,

    // ── Lifecycle timestamps (monotonic) ─────────────────────────────────
    pub(crate) start_mono: MonoNs,
    pub(crate) end_mono: MonoNs,

    // ── Interval evidence ────────────────────────────────────────────────
    /// Locally observed entered intervals (monotonic ns). May be empty if the
    /// span was never entered in this file.
    pub(crate) intervals: Vec<MonoInterval>,

    /// Producer-reported active_ns (from SpanCloseSummary). Zero means not
    /// reported.
    pub(crate) producer_active_ns: u64,

    // ── Unbalanced/quality evidence ──────────────────────────────────────
    /// Unmatched exit events (exits with no corresponding enter).
    pub(crate) unmatched_exits: u32,
    /// Unmatched enter events (enters never popped by exit) — includes both
    /// locally-detected leftovers and producer-reported unbalanced_enters.
    pub(crate) unmatched_enters: u32,
    /// Whether the producer reports loss is observable for this span.
    pub(crate) loss_observable: bool,
    /// Whether active_ns saturated (hit u64::MAX).
    pub(crate) saturated: bool,
    /// Whether concurrent/re-entrant execution was detected.
    pub(crate) concurrent: bool,

    // ── Lifecycle boundary ───────────────────────────────────────────────
    /// Monotonic timestamp of the first ClockSync in this file (file boundary).
    pub(crate) first_clock_sync_mono: Option<MonoNs>,

    // ── Identity quality ─────────────────────────────────────────────────
    pub(crate) identity_quality: &'static str,

    // ── Five-way attribution (optional, from adapter) ────────────────────
    pub(crate) on_cpu_ns_est: Option<u64>,
    pub(crate) blocked_ns_est: Option<u64>,
    pub(crate) async_wait_ns: Option<u64>,
    pub(crate) scheduler_delay_ns: Option<u64>,
    /// Attribution flags (bitfield). Set bits that the adapter cannot resolve.
    pub(crate) attribution_flags: u32,

    // ── Relationships ────────────────────────────────────────────────────
    pub(crate) parent_span_uid: Option<[u8; 16]>,
    pub(crate) attributes: Vec<(String, String)>,

    // ── Source metadata ──────────────────────────────────────────────────
    pub(crate) source_key: String,
    pub(crate) host: String,
    pub(crate) service: String,
    pub(crate) date: String,
}

impl SpanCandidate {
    /// Finalize the candidate into a [`ResolvedSpan`] with checked accounting.
    ///
    /// Computes:
    /// - `span_uid` and `span_type_uid`
    /// - Wall-clock timestamps from monotonic + offset
    /// - `observed_active_wall_ns` from interval union
    /// - `detail_coverage_ns`
    /// - `details_complete` (structural completeness)
    /// - `unknown_ns` (five-way invariant)
    /// - `active_ns` (producer or locally computed)
    ///
    /// Invalid classification (overflow or classified time greater than elapsed)
    /// is discarded and marked degraded so the five-way equality always holds.
    pub(crate) fn finalize(self, clock_offset: Option<ClockOffset>) -> ResolvedSpan {
        let span_uid = compute_span_uid(&self.boot_id, self.instance_id);
        let span_type_uid = match self.type_name.as_deref() {
            Some(schema_name) => compute_span_type_uid_with_schema_name(
                self.kind,
                &self.target,
                &self.name,
                Some(schema_name),
                self.callsite_file.as_deref(),
                self.callsite_line,
            ),
            None => compute_span_type_uid(
                self.kind,
                &self.target,
                &self.name,
                self.callsite_file.as_deref(),
                self.callsite_line,
            ),
        };

        let wall_start = self.start_mono.to_wall_or_raw(clock_offset);
        let wall_end = self.end_mono.to_wall_or_raw(clock_offset);
        let elapsed_ns = wall_end.saturating_sub(wall_start).raw();

        // Compute locally observed active wall time from enter/exit intervals.
        let never_entered = self.intervals.is_empty();
        let (observed_active_wall_ns, active_thread_time_ns) = if !never_entered {
            let thread_time: u64 = self
                .intervals
                .iter()
                .map(|&(enter, exit)| exit.saturating_sub(enter).raw())
                .fold(0u64, |acc, value| acc.saturating_add(value));
            let wall_time = interval_pairing::union_interval_duration(&self.intervals).raw();
            (wall_time, thread_time)
        } else {
            (0, 0)
        };

        let detail_coverage_ns = observed_active_wall_ns;

        // Concurrent: detected by producer or thread-time exceeding wall-time.
        let concurrent =
            self.concurrent || active_thread_time_ns > observed_active_wall_ns.saturating_add(1);

        // Details are structurally complete if:
        // 1. We have local intervals (span was entered)
        // 2. No unbalanced enters/exits
        // 3. Lifecycle start is at or after the file boundary
        // 4. Identity is authoritative (metadata or path)
        // 5. Not saturated
        // 6. Loss is observable
        let has_unbalanced = self.unmatched_enters > 0 || self.unmatched_exits > 0;
        let lifecycle_starts_in_file = match self.first_clock_sync_mono {
            Some(boundary) => self.start_mono.raw() >= boundary.raw(),
            None => false,
        };
        let identity_authoritative =
            self.identity_quality == "metadata" || self.identity_quality == "path";
        let details_complete = !never_entered
            && !has_unbalanced
            && lifecycle_starts_in_file
            && identity_authoritative
            && !self.saturated
            && self.loss_observable;

        // Active_ns: use producer-reported if available, else locally computed
        let active_ns = if self.producer_active_ns > 0 {
            Some(self.producer_active_ns)
        } else if !never_entered {
            Some(active_thread_time_ns)
        } else {
            None
        };

        // Five-way invariant: unknown_ns = elapsed - classified. Corrupt or
        // overflowing classifications are discarded as a unit; retaining a
        // partial subset would make plausible-looking numbers violate the
        // accounting invariant. Bit 1 marks that attribution detail degraded.
        let mut on_cpu_ns_est = self.on_cpu_ns_est;
        let mut blocked_ns_est = self.blocked_ns_est;
        let mut async_wait_ns = self.async_wait_ns;
        let mut scheduler_delay_ns = self.scheduler_delay_ns;
        let mut attribution_flags = self.attribution_flags;
        let classified = on_cpu_ns_est
            .unwrap_or(0)
            .checked_add(blocked_ns_est.unwrap_or(0))
            .and_then(|sum| sum.checked_add(async_wait_ns.unwrap_or(0)))
            .and_then(|sum| sum.checked_add(scheduler_delay_ns.unwrap_or(0)));
        let unknown_ns = match classified {
            Some(classified) if classified <= elapsed_ns => elapsed_ns - classified,
            _ => {
                on_cpu_ns_est = None;
                blocked_ns_est = None;
                async_wait_ns = None;
                scheduler_delay_ns = None;
                attribution_flags |= 0b0010;
                elapsed_ns
            }
        };

        ResolvedSpan {
            span_uid,
            span_type_uid,
            kind: self.kind,
            name: self.name,
            target: self.target,
            callsite_file: self.callsite_file,
            callsite_line: self.callsite_line,
            start_ns: wall_start.raw(),
            end_ns: wall_end.raw(),
            elapsed_ns,
            active_ns,
            observed_active_wall_ns,
            detail_coverage_ns,
            details_complete,
            concurrent,
            parent_span_uid: self.parent_span_uid,
            attributes: self.attributes,
            on_cpu_ns_est,
            blocked_ns_est,
            async_wait_ns,
            scheduler_delay_ns,
            unknown_ns,
            cpu_sample_count: 0, // Filled during sample attribution (stage 3)
            sched_sample_count: 0,
            attribution_version: 1,
            attribution_flags,
            saturated: self.saturated,
            loss_observable: self.loss_observable,
            unbalanced_exits: self.unmatched_exits,
            unbalanced_enters: self.unmatched_enters,
            identity_quality: self.identity_quality,
            source_key: self.source_key,
            host: self.host,
            service: self.service,
            date: self.date,
        }
    }
}

/// Compute a span_uid from the boot-id + span_instance_id.
///
/// The design specifies: `BLAKE3(boot_id || span_instance_id)[..16]`.
/// The boot_id is either decoded from SegmentMetadata (authoritative, stable
/// across files from the same process) or extracted from the source key path
/// (low-quality fallback that cannot claim cross-file stability).
pub(crate) fn compute_span_uid(boot_id: &str, span_instance_id: u64) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(boot_id.as_bytes());
    hasher.update(&span_instance_id.to_le_bytes());
    let mut uid = [0u8; 16];
    uid.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    uid
}

/// Compute a span_type_uid from the span's public identity fields.
/// `BLAKE3(kind || target || name || file || line)[..16]`
pub(crate) fn compute_span_type_uid(
    kind: &str,
    target: &str,
    name: &str,
    file: Option<&str>,
    line: Option<u32>,
) -> [u8; 16] {
    compute_span_type_uid_with_schema_name(kind, target, name, None, file, line)
}

/// Compute a span type identity with an optional schema-level discriminator.
///
/// The runtime `name` always participates: one reusable event schema may carry
/// many dynamic span names. The schema name is appended only as an additional
/// domain-separated discriminator, allowing two struct-derived schemas with the
/// same runtime name to remain distinct without collapsing different runtime
/// names that share one schema. Omitting it preserves the historical UID.
fn compute_span_type_uid_with_schema_name(
    kind: &str,
    target: &str,
    name: &str,
    schema_name: Option<&str>,
    file: Option<&str>,
    line: Option<u32>,
) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\x00");
    hasher.update(target.as_bytes());
    hasher.update(b"\x00");
    hasher.update(name.as_bytes());
    hasher.update(b"\x00");
    if let Some(f) = file {
        hasher.update(f.as_bytes());
    }
    hasher.update(b"\x00");
    if let Some(l) = line {
        hasher.update(&l.to_le_bytes());
    }
    if let Some(schema_name) = schema_name {
        hasher.update(b"\x00dial9:schema-name\x00");
        hasher.update(&(schema_name.len() as u64).to_le_bytes());
        hasher.update(schema_name.as_bytes());
    }
    let mut uid = [0u8; 16];
    uid.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    uid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::decode::clock::MonoNs;

    fn interval(start: u64, end: u64) -> MonoInterval {
        (MonoNs(start), MonoNs(end))
    }

    fn assert_accounting_invariant(span: &ResolvedSpan) {
        let classified = span
            .on_cpu_ns_est
            .unwrap_or(0)
            .checked_add(span.blocked_ns_est.unwrap_or(0))
            .and_then(|sum| sum.checked_add(span.async_wait_ns.unwrap_or(0)))
            .and_then(|sum| sum.checked_add(span.scheduler_delay_ns.unwrap_or(0)))
            .and_then(|sum| sum.checked_add(span.unknown_ns))
            .expect("finalized accounting must not overflow");
        assert_eq!(classified, span.elapsed_ns);
    }

    #[test]
    fn span_uid_deterministic() {
        let uid1 = compute_span_uid("boot-abc", 42);
        let uid2 = compute_span_uid("boot-abc", 42);
        assert_eq!(uid1, uid2);
    }

    #[test]
    fn span_uid_different_instance() {
        let uid1 = compute_span_uid("boot-abc", 42);
        let uid2 = compute_span_uid("boot-abc", 43);
        assert_ne!(uid1, uid2);
    }

    #[test]
    fn span_type_uid_deterministic() {
        let uid1 = compute_span_type_uid("tracing", "target", "name", Some("f.rs"), Some(10));
        let uid2 = compute_span_type_uid("tracing", "target", "name", Some("f.rs"), Some(10));
        assert_eq!(uid1, uid2);
    }

    #[test]
    fn span_type_uid_different_name() {
        let uid1 = compute_span_type_uid("tracing", "t", "a", None, None);
        let uid2 = compute_span_type_uid("tracing", "t", "b", None, None);
        assert_ne!(uid1, uid2);
    }

    #[test]
    fn finalize_basic_modern_span() {
        let candidate = SpanCandidate {
            boot_id: "boot-test".to_string(),
            instance_id: 1,
            kind: "tracing",
            name: "test_span".to_string(),
            type_name: None,
            target: "test_target".to_string(),
            callsite_file: Some("src/main.rs".to_string()),
            callsite_line: Some(10),
            start_mono: MonoNs(1000),
            end_mono: MonoNs(2000),
            intervals: vec![interval(1000, 1500), interval(1600, 1800)],
            producer_active_ns: 700,
            unmatched_exits: 0,
            unmatched_enters: 0,
            loss_observable: true,
            saturated: false,
            concurrent: false,
            first_clock_sync_mono: Some(MonoNs(900)),
            identity_quality: "metadata",
            on_cpu_ns_est: None,
            blocked_ns_est: None,
            async_wait_ns: None,
            scheduler_delay_ns: None,
            attribution_flags: 0b1111,
            parent_span_uid: None,
            attributes: Vec::new(),
            source_key: "test/key".to_string(),
            host: "host".to_string(),
            service: "svc".to_string(),
            date: "2026-01-01".to_string(),
        };

        let span = candidate.finalize(None);
        assert_eq!(span.elapsed_ns, 1000);
        // Union of [1000,1500) and [1600,1800) = 500 + 200 = 700
        assert_eq!(span.observed_active_wall_ns, 700);
        assert_eq!(span.active_ns, Some(700)); // producer_active_ns
        assert!(span.details_complete);
        assert_eq!(span.unknown_ns, 1000); // nothing classified
        assert!(!span.concurrent);
        assert_accounting_invariant(&span);
    }

    #[test]
    fn finalize_classified_exceeds_elapsed_degrades_gracefully() {
        // Simulate corrupt data where classified > elapsed
        let candidate = SpanCandidate {
            boot_id: "boot-test".to_string(),
            instance_id: 1,
            kind: "tracing",
            name: "overflow".to_string(),
            type_name: None,
            target: "t".to_string(),
            callsite_file: None,
            callsite_line: None,
            start_mono: MonoNs(1000),
            end_mono: MonoNs(1100), // elapsed = 100
            intervals: vec![interval(1000, 1100)],
            producer_active_ns: 0,
            unmatched_exits: 0,
            unmatched_enters: 0,
            loss_observable: true,
            saturated: false,
            concurrent: false,
            first_clock_sync_mono: Some(MonoNs(900)),
            identity_quality: "metadata",
            on_cpu_ns_est: Some(80),
            blocked_ns_est: Some(50), // 80 + 50 = 130 > elapsed 100
            async_wait_ns: None,
            scheduler_delay_ns: None,
            attribution_flags: 0,
            parent_span_uid: None,
            attributes: Vec::new(),
            source_key: "test/key".to_string(),
            host: "host".to_string(),
            service: "svc".to_string(),
            date: "2026-01-01".to_string(),
        };

        let span = candidate.finalize(None);
        // Invalid classification is discarded so the five-way invariant remains exact.
        assert_eq!(span.on_cpu_ns_est, None);
        assert_eq!(span.blocked_ns_est, None);
        assert_eq!(span.unknown_ns, 100);
        assert_eq!(span.elapsed_ns, 100);
        assert_ne!(span.attribution_flags & 0b0010, 0);
        assert_accounting_invariant(&span);
    }

    #[test]
    fn finalize_sum_overflow_saturates() {
        // Categories that would overflow u64 when added
        let candidate = SpanCandidate {
            boot_id: "boot-test".to_string(),
            instance_id: 1,
            kind: "tracing",
            name: "huge".to_string(),
            type_name: None,
            target: "t".to_string(),
            callsite_file: None,
            callsite_line: None,
            start_mono: MonoNs(0),
            end_mono: MonoNs(1000),
            intervals: vec![interval(0, 1000)],
            producer_active_ns: 0,
            unmatched_exits: 0,
            unmatched_enters: 0,
            loss_observable: true,
            saturated: false,
            concurrent: false,
            first_clock_sync_mono: Some(MonoNs(0)),
            identity_quality: "metadata",
            on_cpu_ns_est: Some(u64::MAX),
            blocked_ns_est: Some(u64::MAX), // overflow when added to on_cpu
            async_wait_ns: None,
            scheduler_delay_ns: None,
            attribution_flags: 0,
            parent_span_uid: None,
            attributes: Vec::new(),
            source_key: "test/key".to_string(),
            host: "host".to_string(),
            service: "svc".to_string(),
            date: "2026-01-01".to_string(),
        };

        let span = candidate.finalize(None);
        assert_eq!(span.on_cpu_ns_est, None);
        assert_eq!(span.blocked_ns_est, None);
        assert_eq!(span.unknown_ns, 1000);
        assert_ne!(span.attribution_flags & 0b0010, 0);
        assert_accounting_invariant(&span);
    }

    #[test]
    fn finalize_legacy_span() {
        let candidate = SpanCandidate {
            boot_id: "boot-test".to_string(),
            instance_id: 99,
            kind: "tracing",
            name: "legacy_op".to_string(),
            type_name: None,
            target: "my_crate".to_string(),
            callsite_file: Some("src/lib.rs".to_string()),
            callsite_line: Some(42),
            start_mono: MonoNs(100),
            end_mono: MonoNs(9100),
            intervals: vec![interval(1000, 9000)],
            producer_active_ns: 0, // old producer doesn't report
            unmatched_exits: 0,
            unmatched_enters: 0,
            loss_observable: false,
            saturated: false,
            concurrent: false,
            first_clock_sync_mono: None,
            identity_quality: "legacy",
            on_cpu_ns_est: Some(200),
            blocked_ns_est: None,
            async_wait_ns: Some(7800),
            scheduler_delay_ns: None,
            attribution_flags: 0b1011,
            parent_span_uid: None,
            attributes: Vec::new(),
            source_key: "test/key".to_string(),
            host: "host".to_string(),
            service: "svc".to_string(),
            date: "2026-01-01".to_string(),
        };

        let span = candidate.finalize(None);
        assert_eq!(span.identity_quality, "legacy");
        assert!(!span.details_complete); // legacy → never complete
        assert_eq!(span.elapsed_ns, 9000);
        assert_eq!(span.observed_active_wall_ns, 8000);
        // active_ns from local intervals (producer didn't report)
        assert_eq!(span.active_ns, Some(8000));
        // unknown = 9000 - (200 + 7800) = 1000
        assert_eq!(span.unknown_ns, 1000);
        assert_accounting_invariant(&span);
    }
}
