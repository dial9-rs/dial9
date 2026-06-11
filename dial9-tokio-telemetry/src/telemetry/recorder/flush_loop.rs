use crate::metrics::{FlushMetrics, Operation, TlDrainMetrics};
use crate::rate_limit::rate_limited;
use crate::telemetry::buffer;
use crate::telemetry::writer::{SegmentWriter, WriterMode};
use metrique::timers::Timer;
use metrique::unit::Microsecond;
use metrique::unit_of_work::metrics;
use metrique_timesource::time_source;
use std::time::Duration;

use super::ControlCommand;
use super::shared_state::SharedState;

/// Tracks the drain coordination state between the flush loop and the writer.
///
/// When the writer reports a drain is due (`should_drain()`), we can't act
/// immediately because thread-local buffers may still hold events that belong
/// in the current segment. Instead we bump the drain epoch (so threads
/// self-flush on their next `record_event`), wait one cycle (~5 ms) for that
/// to propagate, then perform the intrusive drain + flush + notify the writer
/// via `drained()`.
///
/// Without a state machine, the naïve check `if should_drain { schedule drain }`
/// fires every cycle (since we haven't drained yet), forever deferring the
/// actual drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DrainState {
    /// Normal operation — poll `should_drain()` each cycle.
    Idle,
    /// The writer reported drain due and we bumped the drain epoch.
    /// Next cycle: intrusive drain + flush + `drained()`.
    EpochBumped,
}

/// Stats returned by flush for metrics publishing.
#[metrics(subfield, rename_all = "PascalCase")]
#[derive(Debug)]
pub(crate) struct FlushStats {
    pub event_count: u64,
    pub dropped_batches: u64,
    #[metrics(unit = Microsecond)]
    pub cpu_flush_duration: Duration,
}

/// Perform one flush cycle: drain CPU profilers, drain the collector, write
/// events to disk, and flush the writer. This is the only code path that
/// touches the writer, and it runs exclusively on the flush thread.
pub(super) fn flush_once<M: WriterMode>(
    writer: &mut SegmentWriter<M>,
    events_written: &mut u64,
    shared: &SharedState,
    drain_self: bool,
) -> FlushStats {
    use crate::primitives::sync::atomic::Ordering;

    let events_before = *events_written;
    let cpu_events_time = std::time::Instant::now();
    if shared.is_enabled() {
        shared.flush_sources();
    }
    let cpu_flush_duration = cpu_events_time.elapsed();

    if drain_self {
        // Periodically flush the flush thread's own TL buffer (queue samples + CPU events).
        // We don't drain every cycle because each batch becomes its own trace segment;
        // batching ~1s worth avoids writing tiny segments every 5ms.
        buffer::drain_to_collector(&shared.collector);
    }

    let dropped = shared.collector.take_dropped_batches();
    if dropped > 0 {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                dropped_batches = dropped,
                "telemetry flush fell behind, dropped batches"
            );
        });
    }

    while let Some(batch) = shared.collector.next() {
        if batch.event_count > 0 {
            if let Err(e) = writer.write_encoded_batch(&batch) {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to transcode batch: {e}");
                });
                shared.enabled.store(false, Ordering::Relaxed);
                return FlushStats {
                    event_count: *events_written - events_before,
                    dropped_batches: dropped as u64,
                    cpu_flush_duration,
                };
            }
            *events_written += batch.event_count;
        }
    }
    if let Err(e) = writer.flush() {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!("failed to flush trace data: {e}");
        });
    }
    FlushStats {
        event_count: *events_written - events_before,
        dropped_batches: dropped as u64,
        cpu_flush_duration,
    }
}

/// The flush thread main loop. Extracted so `TelemetryCore::builder` stays readable.
pub(super) fn run_flush_loop<M: WriterMode>(
    control_rx: crate::primitives::sync::mpsc::Receiver<ControlCommand>,
    shared: &SharedState,
    flush_metrics_sink: &metrique_writer::BoxEntrySink,
    mut writer: SegmentWriter<M>,
) {
    // Drain the flush thread's own TL buffer every ~1s (200 × 5ms)
    // rather than every cycle, so queue samples and CPU events
    // are batched into reasonably-sized segments.
    let mut cycle_count: u64 = 0;
    const SELF_DRAIN_INTERVAL: u64 = 200;
    let mut events_written: u64 = 0;

    let sample_interval = Duration::from_millis(10);
    let mut last_sample = time_source().instant();
    // Snapshot the user-provided segment metadata so we can
    // merge it with runtime→worker entries on each flush cycle.
    let static_metadata = writer.segment_metadata().to_vec();

    let mut drain_state = DrainState::Idle;

    loop {
        let mut ack_tx = None;
        let mut exit = false;
        // Wait for control commands up to 5ms.
        match control_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(ControlCommand::FinalizeAndStop(ack)) => {
                ack_tx = Some(ack);
                exit = true;
            }
            Err(crate::primitives::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // All senders dropped — do a best-effort finalize.
                exit = true;
            }
            Err(crate::primitives::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        // When disabled, skip all recording work (queue sampling, metadata
        // merging, drain coordination, flush). The loop still wakes every
        // 5ms to check for control commands and the exit signal.
        if !exit && !shared.is_enabled() {
            continue;
        }

        if last_sample.elapsed() >= sample_interval {
            last_sample = time_source().instant();
            let contexts = shared.contexts.lock().unwrap().clone();
            let total_global_queue: usize = contexts.iter().map(|c| c.global_queue_depth()).sum();
            if !contexts.is_empty() {
                shared.record_queue_sample(total_global_queue);
            }
        }

        // Merge user-provided metadata with runtime→worker mappings and
        // source metadata so the next rotated segment is fully self-describing.
        let contexts = shared.contexts.lock().unwrap().clone();
        let runtime_entries: Vec<(String, String)> =
            contexts.iter().filter_map(|c| c.metadata_entry()).collect();
        let source_entries: Vec<(String, String)> = shared
            .sources
            .lock()
            .unwrap()
            .iter()
            .flat_map(|s| s.segment_metadata())
            .collect();
        if !runtime_entries.is_empty() || !source_entries.is_empty() {
            let mut merged = static_metadata.clone();
            merged.extend(runtime_entries);
            merged.extend(source_entries);
            writer.update_segment_metadata(merged);
        }

        cycle_count += 1;
        let drain_self = exit || cycle_count.is_multiple_of(SELF_DRAIN_INTERVAL);
        // --- Drain coordination state machine ---
        //
        // When the writer reports a drain is due, we can't act immediately
        // because thread-local buffers may still hold events that belong
        // in the current segment.  The two-state machine ensures we:
        //   Idle        → detect should_drain, bump epoch, transition
        //   EpochBumped → intrusive drain + flush + drained(), back to Idle
        //
        // This avoids the bug where re-checking should_drain() every
        // cycle (it stays true until we actually call drained()) would
        // forever reschedule the drain and never reach the drained step.
        let do_drain = match drain_state {
            DrainState::Idle => {
                if !exit && writer.should_drain() {
                    shared.bump_drain_epoch();
                    drain_state = DrainState::EpochBumped;
                }
                false
            }
            DrainState::EpochBumped => {
                drain_state = DrainState::Idle;
                true
            }
        };

        // On exit, bump + drain in the same tick since there is no next
        // tick for the grace period.
        if exit {
            shared.bump_drain_epoch();
        }

        // --- Execute intrusive drain when needed ---
        if exit || do_drain {
            let mut tl_drain_timer = Timer::start_now();
            let stats = shared.drain_all_tl_buffers();
            tl_drain_timer.stop();
            let _guard = TlDrainMetrics {
                operation: Operation::TlDrain,
                duration: tl_drain_timer,
                stats,
                last_drain: exit,
            }
            .append_on_drop(flush_metrics_sink.clone());
        }
        let mut flush_timer = Timer::start_now();
        let stats = flush_once(&mut writer, &mut events_written, shared, drain_self);
        flush_timer.stop();

        // Notify the writer that TL buffers have been drained and flushed.
        // The writer may rotate the segment or just advance its drain timer.
        // Skip on exit — finalize() below will seal the final segment.
        if do_drain
            && !exit
            && let Err(e) = writer.drained()
        {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("failed to complete post-drain action: {e}");
            });
        }

        // Create the metrics guard up front; mutate on the exit path,
        // then let it drop (which emits the entry).
        let mut flush_guard =
            (stats.event_count > 0 || stats.dropped_batches > 0 || exit).then(|| {
                FlushMetrics {
                    operation: Operation::Flush,
                    stats,
                    flush_duration: flush_timer,
                    last_flush: exit,
                    write_metadata_failed: false,
                    finalize_failed: false,
                }
                .append_on_drop(flush_metrics_sink.clone())
            });
        if exit {
            // Write final metadata before sealing so single-segment
            // traces contain runtime→worker mappings.
            if let Err(e) = writer.write_current_segment_metadata() {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to write final segment metadata: {e}");
                });
                if let Some(g) = flush_guard.as_mut() {
                    g.write_metadata_failed = true;
                }
            }
            if let Err(e) = writer.finalize() {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to finalize trace segment: {e}");
                });
                if let Some(g) = flush_guard.as_mut() {
                    g.finalize_failed = true;
                }
            }
        }
        drop(flush_guard);
        if let Some(tx) = ack_tx.take() {
            let _ = tx.send(());
        }
        if exit {
            return;
        }
    }
}
