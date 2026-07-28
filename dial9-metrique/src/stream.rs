//! The dial9 [`EntryIoStream`]: consumes metrique entries and encodes
//! dial9-opted ones into the trace.

use std::collections::HashSet;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use dial9_trace_format::encoder::FxHashMap;
use dial9_trace_format::types::FieldValue;
use metrique_writer::core::descriptor::DescriptorId;
use metrique_writer::stream::{Tee, tee};
use metrique_writer::{Entry, EntryIoStream, IoStreamError};

use dial9_core::handle::Dial9Handle;
use dial9_core::rate_limited;

use super::WithoutDial9Fields;
use super::plan::{Plan, build_plan};
use super::writer::{EntryWalk, WalkError};

/// How often aggregate sink counters are reported at `debug` level.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Entries between report-deadline checks. Amortizes the monotonic clock
/// read across entries: at high rates the check cost vanishes, and at low
/// rates it only stretches the debug-report cadence (the final report on
/// drop always fires).
const REPORT_CHECK_STRIDE: u32 = 64;

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
    /// Encode plans, appended on first sight of a descriptor-id sequence.
    /// Bounded by the number of distinct entry types the process
    /// instantiates.
    plans: Vec<Plan>,
    /// Index into `plans` per descriptor-id sequence. Fx-hashed: the keys
    /// are already hash-like process-internal ids, and this probe is on the
    /// per-entry path for streams that alternate entry types.
    plan_index: FxHashMap<Vec<DescriptorId>, usize>,
    /// The previous entry's key and plan index. Streams are usually
    /// monomorphic, so a key compare replaces the map probe.
    last_key: Vec<DescriptorId>,
    last_plan: Option<usize>,
    /// Schema-name disambiguation across plans (see `build_plan`).
    used_names: HashSet<String>,
    // Scratch buffers reused across entries (single-threaded: `next` takes
    // `&mut self`).
    key_scratch: Vec<DescriptorId>,
    values_scratch: Vec<FieldValue>,
    stats: Stats,
    last_report: Instant,
    /// Countdown to the next report-deadline check (see
    /// [`REPORT_CHECK_STRIDE`]).
    entries_until_report_check: u32,
}

impl Dial9Stream {
    /// Create a stream recording into the trace behind `handle`.
    ///
    /// A disabled handle (e.g. [`Dial9Handle::disabled`]) yields an inert
    /// stream, so wiring can stay unconditional in applications where dial9
    /// is sometimes off.
    ///
    /// Composing this with an existing pipeline by hand leaves dial9's own
    /// `dial9.`-prefixed fields visible to that pipeline;
    /// [`Dial9Stream::tee`] wires both sides up so they are not.
    pub fn new(handle: &Dial9Handle) -> Self {
        Self {
            handle: handle.clone(),
            plans: Vec::new(),
            plan_index: FxHashMap::default(),
            last_key: Vec::new(),
            last_plan: None,
            used_names: HashSet::new(),
            key_scratch: Vec::new(),
            values_scratch: Vec::new(),
            stats: Stats::default(),
            last_report: Instant::now(),
            entries_until_report_check: 1,
        }
    }

    /// Compose a dial9 sink alongside `other`, the pipeline you already have.
    ///
    /// Equivalent to metrique's [`tee`](metrique_writer::stream::tee) with
    /// dial9 on one side, except that `other` is wrapped so dial9's own
    /// `dial9.`-prefixed fields do not reach it: the trace gets the runtime
    /// context, and your EMF/JSON output looks the way it did before.
    ///
    /// ```no_run
    /// use dial9_metrique::Dial9Stream;
    /// use metrique::ServiceMetrics;
    /// use metrique::writer::AttachGlobalEntrySinkExt;
    ///
    /// # let handle = dial9_core::handle::Dial9Handle::disabled();
    /// # let emf_stream = Dial9Stream::new(&handle); // stand-in for your pipeline
    /// let _join = ServiceMetrics::attach_to_stream(
    ///     Dial9Stream::tee(&handle, emf_stream),
    /// );
    /// ```
    ///
    /// Use [`new`](Self::new) with metrique's `tee` directly to keep those
    /// fields in the other sink, or to control the composition yourself.
    pub fn tee<S: EntryIoStream>(
        handle: &Dial9Handle,
        other: S,
    ) -> Tee<Self, WithoutDial9Fields<S>> {
        tee(Self::new(handle), WithoutDial9Fields::new(other))
    }

    fn report(&mut self) {
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

    fn maybe_report(&mut self) {
        self.entries_until_report_check -= 1;
        if self.entries_until_report_check == 0 {
            self.entries_until_report_check = REPORT_CHECK_STRIDE;
            if self.last_report.elapsed() >= REPORT_INTERVAL {
                self.report();
            }
        }
    }
}

impl Drop for Dial9Stream {
    fn drop(&mut self) {
        // Final counter report; the periodic one never fires for
        // short-lived processes.
        self.report();
    }
}

impl EntryIoStream for Dial9Stream {
    fn next(&mut self, entry: &impl Entry) -> Result<(), IoStreamError> {
        // Also short-circuits a connected-but-paused recorder: descriptor
        // identification and plan lookup are wasted work when the encode
        // below would be skipped anyway.
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
                     skipped; hand-written Entry impls that do not implement descriptors() \
                     and entries containing Flex dynamic-key fields carry none"
                );
            });
            return Ok(());
        };

        self.key_scratch.clear();
        self.key_scratch.extend(descriptors.iter().map(|d| d.id()));
        let idx = match self.last_plan {
            Some(idx) if self.key_scratch == self.last_key => idx,
            _ => {
                // `Vec<DescriptorId>: Borrow<[DescriptorId]>` lets the probe
                // reuse the scratch key; only a first-seen type allocates an
                // owned key.
                let idx = match self.plan_index.get(self.key_scratch.as_slice()) {
                    Some(&idx) => idx,
                    None => {
                        let plan = build_plan(&descriptors, &mut self.used_names);
                        self.plans.push(plan);
                        self.plan_index
                            .insert(self.key_scratch.clone(), self.plans.len() - 1);
                        self.plans.len() - 1
                    }
                };
                self.last_key.clear();
                self.last_key.extend_from_slice(&self.key_scratch);
                self.last_plan = Some(idx);
                idx
            }
        };
        let plan = &self.plans[idx];

        if plan.inert {
            self.stats.entries_skipped_inert += 1;
            return Ok(());
        }

        let mut emitted = false;
        let mut dropped = false;
        let values = &mut self.values_scratch;
        self.handle.with_encoder(|enc| {
            let mut walk = EntryWalk::new(plan, &mut *enc, values);
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
            if let Err(err) = walk.finish() {
                dropped = true;
                match err {
                    WalkError::PlanMismatch => {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(
                                entry = %plan.entry_name,
                                "metrique entry emitted a different number of values than \
                                 its descriptor declares (descriptor/write mismatch in \
                                 metrique); event dropped"
                            );
                        });
                    }
                    WalkError::MissingRequired { field } => {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(
                                entry = %plan.entry_name,
                                %field,
                                "metrique entry produced no value for a required field; \
                                 event dropped"
                            );
                        });
                    }
                }
                return;
            }
            match enc.write_event(&plan.schema, values) {
                Ok(()) => emitted = true,
                Err(e) => {
                    dropped = true;
                    rate_limited!(Duration::from_secs(60), {
                        tracing::error!(
                            entry = %plan.entry_name,
                            "encoder rejected the assembled event; dropped: {e}"
                        );
                    });
                }
            }
        });

        if emitted {
            self.stats.events_emitted += 1;
        } else if dropped {
            self.stats.entries_dropped += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        // No metrique-side flush needed:
        // `with_encoder` self-flushes on batch thresholds,
        // dial9's flush thread drains thread-local buffers intrusively on
        // its own cadence, and the buffer flushes on thread exit.
        Ok(())
    }
}
