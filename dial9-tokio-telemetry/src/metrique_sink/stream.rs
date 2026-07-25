//! The dial9 [`EntryIoStream`]: consumes metrique entries and encodes
//! dial9-opted ones into the trace.

use std::collections::HashMap;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use dial9_trace_format::types::FieldValue;
use metrique_writer::core::descriptor::DescriptorId;
use metrique_writer::{Entry, EntryIoStream, IoStreamError};

use crate::rate_limit::rate_limited;
use crate::telemetry::Dial9Handle;

use super::plan::{Plan, build_plan};
use super::writer::EntryWalk;

/// How often aggregate sink counters are reported at `debug` level.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Aggregate counters for periodic reporting.
#[derive(Debug, Default)]
struct Stats {
    events_emitted: u64,
    entries_skipped_inert: u64,
    entries_no_descriptor: u64,
    entries_dropped: u64,
}

/// A metrique [`EntryIoStream`] that records entries into the dial9 trace.
///
/// See the [module docs](super) for wiring, the opt-in model, and current
/// limitations.
#[derive(Debug)]
pub struct Dial9Stream {
    handle: Dial9Handle,
    /// Encode plans keyed on each entry type's descriptor-id sequence.
    /// Bounded by the number of distinct entry types the process
    /// instantiates.
    plans: HashMap<Vec<DescriptorId>, Plan>,
    /// Schema-name disambiguation across plans (see `build_plan`).
    used_names: HashMap<String, u32>,
    /// Scratch buffers reused across entries (single-threaded: `next` takes
    /// `&mut self`).
    key_scratch: Vec<DescriptorId>,
    slots_scratch: Vec<Option<FieldValue>>,
    values_scratch: Vec<FieldValue>,
    stats: Stats,
    last_report: Instant,
}

impl Dial9Stream {
    /// Create a stream recording into the trace behind `handle`.
    ///
    /// A disabled handle (e.g. [`Dial9Handle::disabled`]) yields an inert
    /// stream, so wiring can stay unconditional in applications where dial9
    /// is sometimes off.
    pub fn new(handle: &Dial9Handle) -> Self {
        Self {
            handle: handle.clone(),
            plans: HashMap::new(),
            used_names: HashMap::new(),
            key_scratch: Vec::new(),
            slots_scratch: Vec::new(),
            values_scratch: Vec::new(),
            stats: Stats::default(),
            last_report: Instant::now(),
        }
    }

    fn maybe_report(&mut self) {
        if self.last_report.elapsed() < REPORT_INTERVAL {
            return;
        }
        self.last_report = Instant::now();
        tracing::debug!(
            plans = self.plans.len(),
            events_emitted = self.stats.events_emitted,
            entries_skipped_inert = self.stats.entries_skipped_inert,
            entries_no_descriptor = self.stats.entries_no_descriptor,
            entries_dropped = self.stats.entries_dropped,
            "dial9 metrique sink counters"
        );
    }
}

impl EntryIoStream for Dial9Stream {
    fn next(&mut self, entry: &impl Entry) -> Result<(), IoStreamError> {
        if !self.handle.is_enabled() {
            return Ok(());
        }
        self.maybe_report();

        let Some(descriptors) = entry.descriptors().into_available() else {
            // No stable way to identify the concrete type here, so this is
            // rate-limited rather than deduped per type.
            self.stats.entries_no_descriptor += 1;
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(
                    "metrique entry without descriptors reached the dial9 sink and was \
                     skipped; hand-written Entry impls need #[metrics] derive to appear \
                     in dial9 traces"
                );
            });
            return Ok(());
        };

        self.key_scratch.clear();
        self.key_scratch.extend(descriptors.iter().map(|d| d.id()));
        // `Vec<DescriptorId>: Borrow<[DescriptorId]>` lets the lookup reuse
        // the scratch key; only a first-seen type allocates an owned key.
        if !self.plans.contains_key(self.key_scratch.as_slice()) {
            let plan = build_plan(&descriptors, &mut self.used_names);
            self.plans.insert(self.key_scratch.clone(), plan);
        }
        let plan = &self.plans[self.key_scratch.as_slice()];

        if plan.inert {
            self.stats.entries_skipped_inert += 1;
            return Ok(());
        }
        if plan.unusable {
            // Duplicate field names; reported once at plan build.
            self.stats.entries_dropped += 1;
            return Ok(());
        }

        let mut emitted = false;
        let mut dropped = false;
        let mut resolved = None;
        let slots = &mut self.slots_scratch;
        let values = &mut self.values_scratch;
        self.handle.with_encoder(|enc| {
            let mut walk = EntryWalk::new(plan, enc, slots);
            // A panicking `Value::write` impl must not poison the flush
            // thread. Capture happens before any event bytes are written,
            // so a mid-walk panic leaves at most orphaned string-pool
            // entries (harmless).
            let walked = catch_unwind(AssertUnwindSafe(|| entry.write(&mut walk)));
            if walked.is_err() {
                dropped = true;
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %plan.entry_name,
                        "metrique entry panicked during dial9 capture; event dropped"
                    );
                });
                return;
            }
            if !walk.aligned() {
                dropped = true;
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %plan.entry_name,
                        "metrique entry emitted values that do not match its descriptor \
                         (dynamic-key Flex fields are the usual cause); event dropped"
                    );
                });
                return;
            }
            if let Err(field) = walk.fill_values(values) {
                dropped = true;
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        entry = %plan.entry_name,
                        %field,
                        "metrique entry produced no value for a required field; event \
                         dropped"
                    );
                });
                return;
            }
            resolved = walk.take_recorded();
            enc.write_event(&plan.schema, values);
            emitted = true;
        });

        // Cache the write-order dispatch recorded by a successful resolving
        // walk; subsequent entries of this type skip the name lookups.
        if emitted
            && let Some(actions) = resolved
            && let Some(plan) = self.plans.get_mut(self.key_scratch.as_slice())
        {
            debug_assert_eq!(actions.len(), plan.expected_values);
            plan.positional = Some(actions);
        }

        if emitted {
            self.stats.events_emitted += 1;
        } else if dropped {
            self.stats.entries_dropped += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        // dial9 flushes its thread-local buffers on its own cadence.
        Ok(())
    }
}
