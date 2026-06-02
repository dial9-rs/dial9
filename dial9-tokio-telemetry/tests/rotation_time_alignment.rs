//! Verify that rotated trace segments contain events from non-overlapping
//! (or minimally overlapping) time ranges.

use common::decode_file;
use dial9_tokio_telemetry::telemetry::{DiskWriter, TelemetryCore};
use metrique::local::{LocalFormat, OutputStyle};
use serde::Deserialize;
use std::time::Duration;

mod common;

/// Maximum allowed overlap between adjacent segments in seconds.
const MAX_OVERLAP_SECS: f64 = 2.0;

#[derive(Debug, Deserialize)]
#[allow(dead_code, clippy::enum_variant_names)]
#[serde(tag = "event")]
enum TimedEvent {
    PollStartEvent {
        timestamp_ns: u64,
    },
    PollEndEvent {
        timestamp_ns: u64,
    },
    WorkerParkEvent {
        timestamp_ns: u64,
    },
    WorkerUnparkEvent {
        timestamp_ns: u64,
    },
    QueueSampleEvent {
        timestamp_ns: u64,
    },
    TaskSpawnEvent {
        timestamp_ns: u64,
    },
    TaskTerminateEvent {
        timestamp_ns: u64,
    },
    CpuSampleEvent {
        timestamp_ns: u64,
    },
    TaskDumpEvent {
        timestamp_ns: u64,
    },
    AllocEvent {
        timestamp_ns: u64,
    },
    FreeEvent {
        timestamp_ns: u64,
    },
    WakeEventEvent {
        timestamp_ns: u64,
    },
    ClockSyncEvent {
        timestamp_ns: u64,
    },
    SegmentMetadataEvent {
        timestamp_ns: u64,
    },
    #[serde(other)]
    Other,
}

impl TimedEvent {
    fn timestamp_ns(&self) -> Option<u64> {
        match self {
            Self::PollStartEvent { timestamp_ns }
            | Self::PollEndEvent { timestamp_ns }
            | Self::WorkerParkEvent { timestamp_ns }
            | Self::WorkerUnparkEvent { timestamp_ns }
            | Self::QueueSampleEvent { timestamp_ns }
            | Self::TaskSpawnEvent { timestamp_ns }
            | Self::TaskTerminateEvent { timestamp_ns }
            | Self::CpuSampleEvent { timestamp_ns }
            | Self::TaskDumpEvent { timestamp_ns }
            | Self::AllocEvent { timestamp_ns }
            | Self::FreeEvent { timestamp_ns }
            | Self::WakeEventEvent { timestamp_ns }
            | Self::ClockSyncEvent { timestamp_ns } => Some(*timestamp_ns),
            Self::SegmentMetadataEvent { .. } | Self::Other => None,
        }
    }

    fn is_segment_metadata(&self) -> bool {
        matches!(self, Self::SegmentMetadataEvent { .. })
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::PollStartEvent { .. } => "PollStart",
            Self::PollEndEvent { .. } => "PollEnd",
            Self::WorkerParkEvent { .. } => "WorkerPark",
            Self::WorkerUnparkEvent { .. } => "WorkerUnpark",
            Self::QueueSampleEvent { .. } => "QueueSample",
            Self::TaskSpawnEvent { .. } => "TaskSpawn",
            Self::TaskTerminateEvent { .. } => "TaskTerminate",
            Self::CpuSampleEvent { .. } => "CpuSample",
            Self::TaskDumpEvent { .. } => "TaskDump",
            Self::AllocEvent { .. } => "Alloc",
            Self::FreeEvent { .. } => "Free",
            Self::WakeEventEvent { .. } => "WakeEvent",
            Self::ClockSyncEvent { .. } => "ClockSync",
            Self::SegmentMetadataEvent { .. } => "SegmentMetadata",
            Self::Other => "Other",
        }
    }
}

#[test]
fn rotated_segments_have_bounded_time_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let rotation_period = Duration::from_secs(2);
    let num_workers = 4;

    let writer = DiskWriter::builder()
        .base_path(&trace_path)
        .max_file_size(u64::MAX)
        .max_total_size(u64::MAX)
        .rotation_period(rotation_period)
        .build()
        .unwrap();

    let (render_queue, metrics_sink) =
        metrique_writer::test_util::render_entry_sink(LocalFormat::new(OutputStyle::Pretty));

    let guard = TelemetryCore::builder()
        .writer(writer)
        .worker_metrics_sink(metrics_sink)
        .build()
        .unwrap();
    guard.enable();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let (runtime, _handle) = guard.trace_runtime("main").build(builder).unwrap();

    runtime.block_on(async {
        let start = tokio::time::Instant::now();
        let target_duration = Duration::from_secs(15);

        let mut handles = Vec::new();
        for _ in 0..num_workers {
            handles.push(tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                while start.elapsed() < target_duration {
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let elapsed = start.elapsed();
        if elapsed < target_duration {
            tokio::time::sleep(target_duration - elapsed).await;
        }
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(5))
        .expect("graceful shutdown");

    let flush_metrics = render_queue.entries();
    eprintln!("flush-thread metrics ({} entries):", flush_metrics.len());
    for entry in &flush_metrics {
        eprintln!("{entry}");
    }

    // Collect all sealed segment files, sorted by index.
    let mut segment_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "bin")
                && !p
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .ends_with(".active")
        })
        .collect();
    segment_files.sort();

    assert!(
        segment_files.len() >= 3,
        "expected at least 3 rotated segments, got {}. Files: {:?}",
        segment_files.len(),
        segment_files
    );

    let segment_events: Vec<Vec<TimedEvent>> =
        segment_files.iter().map(|path| decode_file(path)).collect();

    let segment_ranges: Vec<(u64, u64)> = segment_events
        .iter()
        .enumerate()
        .map(|(i, events)| {
            let timestamps: Vec<u64> = events
                .iter()
                .filter(|e| !e.is_segment_metadata())
                .filter_map(|e| e.timestamp_ns())
                .collect();
            assert!(
                !timestamps.is_empty(),
                "segment {} has no timestamped events",
                segment_files[i].display()
            );
            let min = *timestamps.iter().min().unwrap();
            let max = *timestamps.iter().max().unwrap();
            (min, max)
        })
        .collect();

    let mut max_observed_overlap = Duration::ZERO;
    let non_final_boundaries = if segment_ranges.len() >= 3 {
        segment_ranges.len() - 2
    } else {
        assert!(segment_ranges.len() >= 2);
        1
    };
    for i in 0..non_final_boundaries {
        let (_min_a, max_a) = segment_ranges[i];
        let (min_b, _max_b) = segment_ranges[i + 1];

        let overlap = if max_a > min_b {
            Duration::from_nanos(max_a - min_b)
        } else {
            Duration::ZERO
        };

        if overlap > max_observed_overlap {
            max_observed_overlap = overlap;
        }

        let overlap_secs = overlap.as_secs_f64();
        eprintln!("segments {i} → {}: overlap = {:.3}s", i + 1, overlap_secs,);

        if overlap_secs > MAX_OVERLAP_SECS {
            let late_in_a: Vec<_> = segment_events[i]
                .iter()
                .filter(|e| !e.is_segment_metadata())
                .filter(|e| e.timestamp_ns().is_some_and(|t| t > min_b))
                .map(|e| e.type_name())
                .collect();
            let early_in_b: Vec<_> = segment_events[i + 1]
                .iter()
                .filter(|e| !e.is_segment_metadata())
                .filter(|e| e.timestamp_ns().is_some_and(|t| t < max_a))
                .map(|e| e.type_name())
                .collect();
            panic!(
                "segment {i} → {} overlap is {:.3}s, exceeds {MAX_OVERLAP_SECS}s tolerance.\n\
                 Events in segment {i} past segment {} start ({} events): {:?}\n\
                 Events in segment {} before segment {i} end ({} events): {:?}\n\
                 Flush-thread metrics ({} entries):\n{}",
                i + 1,
                overlap_secs,
                i + 1,
                late_in_a.len(),
                late_in_a,
                i + 1,
                early_in_b.len(),
                early_in_b,
                flush_metrics.len(),
                flush_metrics.join("\n"),
            );
        }
    }

    eprintln!(
        "max observed overlap: {:.3}s across {} non-final segment boundaries",
        max_observed_overlap.as_secs_f64(),
        non_final_boundaries
    );
}
