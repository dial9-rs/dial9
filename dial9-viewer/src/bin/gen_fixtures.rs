//! Synthetic trace fixture generator (T42, test infrastructure).
//!
//! Produces the trace fixtures the demo trace cannot provide, so the live
//! checks stop carrying NOT-TRIGGERABLE holes (features/01 "Live validation
//! results"):
//!
//!   - `dial9-fixtures` (bucket): multi-host / multi-service Hive-style layouts
//!     driving the heatmap's boot-count annotation (F5), seam tiling (F7),
//!     coverage-gap hatching (F8) and boot-change dividers (F9), plus a
//!     10-segment windowing host and a multi-runtime (#596) segment;
//!   - `dial9-fixtures-dates` (bucket): date partitions at the bucket ROOT
//!     (no key prefix), the #471 date-layer auto-empty case (D4);
//!   - `dial9-fixtures-large` (bucket): a >200 MB multi-segment set that
//!     trips the 200 MB selection cap (H4/H5) and gives T39's "large trace"
//!     budget run a reproducible >=100 MB-raw input (skippable with
//!     `--skip-large`);
//!   - committed small fixtures under `ui/live-checks/fixtures/segments/`: the
//!     10-segment set with controlled boundary-spanning polls (T17) and a
//!     multi-runtime trace, consumed by the vitest suites. `manifest.json`
//!     records the planted facts the tests assert.
//!
//! Everything is deterministic (fixed clock anchors, seeded jitter): the same
//! source produces byte-identical output, which is why the small fixtures can
//! be committed while the large seed tree is regenerated on demand (it is
//! gitignored; see the size policy in `dial9-viewer/ui/README.md`).
//!
//! The seed tree is shaped `<out>/generated/s3/<bucket>/<key...>`. File
//! mtimes are LOAD-BEARING: the dev-server propagates them into the fake S3
//! as each object's `last_modified`, which the heatmap uses as the segment
//! end — the seam/gap scenarios exist entirely in those mtimes.
//!
//! Usage (from the repo root):
//!
//! ```bash
//! cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures
//! # then serve it:
//! DIAL9_SEED_DIR=dial9-viewer/ui/live-checks/fixtures/generated/s3 \
//!   DIAL9_DEFAULT_PREFIX= PORT=3022 \
//!   cargo run -p dial9-viewer --features dev-server --bin dev-server
//! ```
//!
//! Wire compatibility: events are written through the real
//! `dial9-trace-format` encoder with the same event names and field names the
//! runtime emits (`PollStartEvent`, `WorkerParkEvent`, `SegmentMetadataEvent`
//! with `runtime.<name>` entries, ...). The format is self-describing, so the
//! JS decoder reads these exactly like production traces; the vitest fixture
//! suite pins that (`src/lib/trace/segments.fixtures.test.ts`).

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use dial9_trace_format::InternedString;
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

#[allow(dead_code)]
#[path = "../segment_object_key_codec.rs"]
mod segment_object_key_codec;

const NS: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

/// Monotonic clock base for every generated segment (1s, comfortably below
/// the parser's legacy epoch-scale heuristic for `SegmentMetadataEvent`).
const MONO_BASE_NS: u64 = NS;

/// The fixture day. Every scenario lives on 2026-04-09 so the pinned page
/// clock used by the fixture live checks (`FIXTURE_CLOCK`, 2026-04-09T21:00Z)
/// reaches it with the stock "Last 24hr" window.
const FIXTURE_YEAR: i32 = 2026;
const FIXTURE_MONTH: time::Month = time::Month::April;
const FIXTURE_DAY: u8 = 9;

/// Per-segment target for the large family. 8 segments x 28 MB = 224 MB
/// listed, safely past the viewer's 200 MiB selection cap (`MAX_OPEN_BYTES`).
const LARGE_SEGMENT_TARGET_RAW: usize = 28 * 1024 * 1024;
const LARGE_SEGMENT_COUNT: usize = 8;
const SELECTION_CAP_BYTES: u64 = 200 * 1024 * 1024;

/// Spawn locations rotated through background polls.
const SPAWN_LOCS: [&str; 4] = [
    "src/fixture/ingest.rs:21",
    "src/fixture/flush.rs:87",
    "src/fixture/reader.rs:143",
    "src/fixture/timer.rs:9",
];

fn main() -> Result<()> {
    let mut out: Option<PathBuf> = None;
    let mut skip_large = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => {
                let v = args.next().context("--out requires a directory argument")?;
                out = Some(PathBuf::from(v));
            }
            "--skip-large" => skip_large = true,
            "--help" | "-h" => {
                println!(
                    "gen-fixtures [--out <dir>] [--skip-large]\n\
                     default out: <dial9-viewer>/ui/live-checks/fixtures"
                );
                return Ok(());
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    let out = out
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/live-checks/fixtures"));

    let schemas = Schemas::new();
    let mut sizes: Vec<(String, usize)> = Vec::new();

    // A clean seed tree every run: determinism includes absence of stale keys.
    let seed_root = out.join("generated/s3");
    if seed_root.exists() {
        fs::remove_dir_all(&seed_root)
            .with_context(|| format!("removing stale seed tree {}", seed_root.display()))?;
    }

    let segments_dir = out.join("segments");
    fs::create_dir_all(&segments_dir)
        .with_context(|| format!("creating {}", segments_dir.display()))?;

    // ── Committed set: 10-segment window with planted boundary polls ──
    let window = build_window_family(&schemas)?;
    for (i, seg) in window.segments.iter().enumerate() {
        let file = segments_dir.join(format!("window-{i:02}.bin.gz"));
        fs::write(&file, &seg.gz).with_context(|| format!("writing {}", file.display()))?;
        sizes.push((format!("segments/window-{i:02}.bin.gz"), seg.gz.len()));
    }

    // ── Committed set: multi-runtime (#596) trace ──
    let multi_runtime = build_multi_runtime_segment(&schemas)?;
    let mr_file = segments_dir.join("multi-runtime.bin.gz");
    fs::write(&mr_file, &multi_runtime)
        .with_context(|| format!("writing {}", mr_file.display()))?;
    sizes.push(("segments/multi-runtime.bin.gz".into(), multi_runtime.len()));

    write_manifest(&segments_dir, &window)?;

    // ── Seed tree: dial9-fixtures (browse-layout scenarios) ──
    let mut objects: Vec<SeedObject> = Vec::new();
    objects.extend(browse_layout_objects(&schemas, &window, &multi_runtime)?);

    // ── Seed tree: dial9-fixtures-dates (#471 date partitions at root) ──
    objects.push(dates_bucket_object(&schemas)?);

    // ── Seed tree: dial9-fixtures-large (H4/H5 cap + T39 budget input) ──
    if skip_large {
        println!("--skip-large: dial9-fixtures-large not generated");
    } else {
        objects.extend(large_family_objects(&schemas)?);
    }

    for obj in &objects {
        write_seed_object(&seed_root, obj)?;
        sizes.push((format!("{}/{}", obj.bucket, obj.key), obj.gz.len()));
    }

    println!("generated {} files under {}", sizes.len(), out.display());
    for (name, len) in &sizes {
        println!("  {:>12}  {}", human(*len), name);
    }
    println!(
        "\nserve with:\n  DIAL9_SEED_DIR={} DIAL9_DEFAULT_PREFIX= PORT=3022 \\\n    cargo run -p dial9-viewer --features dev-server --bin dev-server",
        seed_root.display()
    );
    Ok(())
}

fn human(len: usize) -> String {
    if len >= 1024 * 1024 {
        format!("{:.1} MB", len as f64 / (1024.0 * 1024.0))
    } else if len >= 1024 {
        format!("{:.1} KB", len as f64 / 1024.0)
    } else {
        format!("{len} B")
    }
}

// ── Wall clock / key layout helpers ──────────────────────────────────────

/// Unix seconds for `HH:MM:SS` on the fixture day (UTC).
fn fixture_epoch(h: u8, m: u8, s: u8) -> i64 {
    time::Date::from_calendar_date(FIXTURE_YEAR, FIXTURE_MONTH, FIXTURE_DAY)
        .expect("fixture date is valid")
        .with_hms(h, m, s)
        .expect("fixture time is valid")
        .assume_utc()
        .unix_timestamp()
}

/// Default segment object key layout:
/// `{prefix}/version=1/date={YYYY-MM-DD}/service={service}/time={HHMM}/instance={instance}/boot={boot_id}/{epoch}-{index}.bin.gz`
/// with the date/HHMM path derived FROM the epoch so they always agree.
fn layout_key(
    prefix: &str,
    service: &str,
    host: &str,
    boot: &str,
    epoch_s: i64,
    index: u32,
) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(epoch_s).expect("epoch in range");
    let date = format!(
        "{:04}-{:02}-{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day()
    );
    let hhmm = format!("{:02}{:02}", dt.hour(), dt.minute());
    let prefix = if prefix.is_empty() {
        None
    } else {
        Some(prefix.trim_end_matches('/'))
    };
    let filename = format!("{epoch_s}-{index}.bin.gz");
    segment_object_key_codec::format_v1_segment_object_key(
        prefix, &date, service, &hhmm, host, boot, &filename,
    )
}

// ── Deterministic jitter ─────────────────────────────────────────────────

/// SplitMix64: tiny, seedable, deterministic. Good enough for cadence jitter.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// FNV-1a over a string, for stable per-host seeds.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

// ── Wire schemas (names + fields match the runtime's events) ─────────────

struct Schemas {
    poll_start: Schema,
    poll_end: Schema,
    park: Schema,
    unpark: Schema,
    queue: Schema,
    task_spawn: Schema,
    clock_sync: Schema,
    metadata: Schema,
}

impl Schemas {
    fn new() -> Self {
        let f = |n: &str, t: FieldType| FieldDef::new(n, t);
        Self {
            poll_start: Schema::new(
                "PollStartEvent",
                vec![
                    f("worker_id", FieldType::Varint),
                    f("local_queue", FieldType::Varint),
                    f("task_id", FieldType::Varint),
                    f("spawn_loc", FieldType::PooledString),
                ],
            ),
            poll_end: Schema::new("PollEndEvent", vec![f("worker_id", FieldType::Varint)]),
            park: Schema::new(
                "WorkerParkEvent",
                vec![
                    f("worker_id", FieldType::Varint),
                    f("local_queue", FieldType::Varint),
                    f("cpu_time_ns", FieldType::Varint),
                    f("tid", FieldType::Varint),
                ],
            ),
            unpark: Schema::new(
                "WorkerUnparkEvent",
                vec![
                    f("worker_id", FieldType::Varint),
                    f("local_queue", FieldType::Varint),
                    f("cpu_time_ns", FieldType::Varint),
                    f("sched_wait_ns", FieldType::Varint),
                    f("tid", FieldType::Varint),
                ],
            ),
            queue: Schema::new(
                "QueueSampleEvent",
                vec![f("global_queue", FieldType::Varint)],
            ),
            task_spawn: Schema::new(
                "TaskSpawnEvent",
                vec![
                    f("task_id", FieldType::Varint),
                    f("spawn_loc", FieldType::PooledString),
                    f("instrumented", FieldType::Bool),
                ],
            ),
            clock_sync: Schema::new("ClockSyncEvent", vec![f("realtime_ns", FieldType::Varint)]),
            metadata: Schema::new(
                "SegmentMetadataEvent",
                vec![f("entries", FieldType::StringMap)],
            ),
        }
    }
}

// ── Segment content model ────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Ev {
    PollStart {
        worker: u64,
        task: u64,
        loc: &'static str,
    },
    PollEnd {
        worker: u64,
    },
    Park {
        worker: u64,
        cpu: u64,
        tid: u64,
    },
    Unpark {
        worker: u64,
        cpu: u64,
        tid: u64,
    },
    Queue {
        depth: u64,
    },
    Spawn {
        task: u64,
        loc: &'static str,
    },
}

/// A poll planted to span a segment boundary (T17): the `PollStart` is the
/// worker's LAST event in its segment; the matching `PollEnd` is the worker's
/// FIRST event in a later segment (with full silence in between).
struct PlantedPoll {
    worker: u64,
    task: u64,
    loc: &'static str,
    /// Segment index carrying the open PollStart, and its monotonic ts.
    start_segment: usize,
    start_ns: u64,
    /// Segment index carrying the dangling PollEnd, and its monotonic ts.
    end_segment: usize,
    end_ns: u64,
}

struct SegmentContent<'a> {
    mono_start_ns: u64,
    mono_end_ns: u64,
    /// Wall clock (ns since epoch) at `mono_start_ns` — the ClockSync anchor.
    real_at_start_ns: u64,
    workers: &'a [u64],
    /// Workers with ZERO events in this segment (the silent-interior case).
    silent: &'a [u64],
    /// (worker, task, loc, ts): PollStart with no close, as the worker's
    /// last event.
    open_at_end: &'a [(u64, u64, &'static str, u64)],
    /// (worker, ts): dangling PollEnd as the worker's first event.
    close_at_start: &'a [(u64, u64)],
    metadata: &'a [(String, String)],
    /// Background poll cycles per worker.
    cycles_per_worker: usize,
    /// Jitter seed (equal seeds + equal relative windows => identical bytes).
    seed: u64,
}

/// Encode one segment (file header + schemas + events) and return the raw
/// (uncompressed) bytes. Events are emitted in timestamp order.
fn build_segment(sch: &Schemas, c: &SegmentContent<'_>) -> Result<Vec<u8>> {
    let span = c.mono_end_ns - c.mono_start_ns;
    ensure!(
        span > 10 * NS,
        "segment span too short for the activity model"
    );
    let mut seed = c.seed;

    // (ts, seq, ev): seq keeps construction order stable across equal ts.
    let mut evs: Vec<(u64, u32, Ev)> = Vec::new();
    let mut seq: u32 = 0;
    let mut push = |evs: &mut Vec<(u64, u32, Ev)>, ts: u64, ev: Ev| {
        evs.push((ts, seq, ev));
        seq += 1;
    };

    for (wi, &w) in c.workers.iter().enumerate() {
        if c.silent.contains(&w) {
            continue;
        }
        let tid = 40_000 + w;
        let stagger = wi as u64 * 7 * MS;

        // A dangling close must be the worker's FIRST event (a park/unpark
        // before it would make the segment start ambiguous — see
        // computeSegmentEdgePolls).
        let close = c.close_at_start.iter().find(|(cw, _)| *cw == w);
        let mut t = match close {
            Some(&(_, ts)) => {
                push(&mut evs, ts, Ev::PollEnd { worker: w });
                ts + NS
            }
            None => c.mono_start_ns + 200 * MS + stagger,
        };

        // A planted open PollStart must be the worker's LAST event.
        let open = c.open_at_end.iter().find(|(ow, ..)| *ow == w);
        let bg_end = match open {
            Some(&(_, _, _, ts)) => ts.saturating_sub(NS),
            None => c.mono_end_ns - 2 * NS,
        };
        ensure!(t < bg_end, "worker {w}: no room for background activity");

        push(
            &mut evs,
            t,
            Ev::Unpark {
                worker: w,
                cpu: 0,
                tid,
            },
        );
        t += MS;

        let bg_span = bg_end - t;
        let period = bg_span / (c.cycles_per_worker as u64 + 1);
        let mut cpu: u64 = 0;
        for k in 0..c.cycles_per_worker {
            let jitter = splitmix64(&mut seed) % (period / 4 + 1);
            let start = t + jitter;
            let poll_len = period / 3 + splitmix64(&mut seed) % (period / 4 + 1);
            let end = start + poll_len;
            if end >= bg_end {
                break;
            }
            let task = w * 100 + (k as u64 % 3);
            let loc = SPAWN_LOCS[((w as usize) + k) % SPAWN_LOCS.len()];
            push(
                &mut evs,
                start,
                Ev::PollStart {
                    worker: w,
                    task,
                    loc,
                },
            );
            push(&mut evs, end, Ev::PollEnd { worker: w });
            cpu += poll_len;
            t += period;
        }
        push(
            &mut evs,
            bg_end,
            Ev::Park {
                worker: w,
                cpu,
                tid,
            },
        );

        if let Some(&(_, task, loc, ts)) = open {
            // Re-enter and leave the poll open across the segment end.
            push(
                &mut evs,
                ts.saturating_sub(50 * MS),
                Ev::Unpark {
                    worker: w,
                    cpu,
                    tid,
                },
            );
            push(
                &mut evs,
                ts,
                Ev::PollStart {
                    worker: w,
                    task,
                    loc,
                },
            );
        }
    }

    // Global queue samples every ~500ms (no worker attribution).
    let mut qt = c.mono_start_ns + 100 * MS;
    let mut qi: u64 = 0;
    while qt < c.mono_end_ns - 100 * MS {
        push(&mut evs, qt, Ev::Queue { depth: qi % 7 });
        qt += 500 * MS;
        qi += 1;
    }

    // Task spawns for the planted tasks (identity for the boundary polls).
    for &(_, task, loc, _) in c.open_at_end {
        push(&mut evs, c.mono_start_ns + 50 * MS, Ev::Spawn { task, loc });
    }

    evs.sort_by_key(|(ts, seq, _)| (*ts, *seq));

    let mut w = SegmentWriter::new(sch);
    w.clock_sync(c.mono_start_ns, c.real_at_start_ns)?;
    w.metadata(c.mono_start_ns, c.metadata)?;
    for (ts, _, ev) in &evs {
        w.write(*ts, *ev)?;
    }
    Ok(w.finish())
}

/// Thin typed facade over the dynamic encoder API.
struct SegmentWriter<'a> {
    enc: Encoder<Vec<u8>>,
    sch: &'a Schemas,
    interned: HashMap<&'static str, InternedString>,
}

impl<'a> SegmentWriter<'a> {
    fn new(sch: &'a Schemas) -> Self {
        Self {
            enc: Encoder::new(),
            sch,
            interned: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &'static str) -> Result<InternedString> {
        if let Some(id) = self.interned.get(s) {
            return Ok(*id);
        }
        let id = self.enc.intern_string(s).context("interning spawn_loc")?;
        self.interned.insert(s, id);
        Ok(id)
    }

    fn write(&mut self, ts: u64, ev: Ev) -> Result<()> {
        use FieldValue::{Bool, PooledString, Varint};
        match ev {
            Ev::PollStart { worker, task, loc } => {
                let loc = self.intern(loc)?;
                self.enc.write_event(
                    &self.sch.poll_start,
                    ts,
                    &[Varint(worker), Varint(0), Varint(task), PooledString(loc)],
                )?;
            }
            Ev::PollEnd { worker } => {
                self.enc
                    .write_event(&self.sch.poll_end, ts, &[Varint(worker)])?;
            }
            Ev::Park { worker, cpu, tid } => {
                self.enc.write_event(
                    &self.sch.park,
                    ts,
                    &[Varint(worker), Varint(0), Varint(cpu), Varint(tid)],
                )?;
            }
            Ev::Unpark { worker, cpu, tid } => {
                self.enc.write_event(
                    &self.sch.unpark,
                    ts,
                    &[
                        Varint(worker),
                        Varint(0),
                        Varint(cpu),
                        Varint(0),
                        Varint(tid),
                    ],
                )?;
            }
            Ev::Queue { depth } => {
                self.enc
                    .write_event(&self.sch.queue, ts, &[Varint(depth)])?;
            }
            Ev::Spawn { task, loc } => {
                let loc = self.intern(loc)?;
                self.enc.write_event(
                    &self.sch.task_spawn,
                    ts,
                    &[Varint(task), PooledString(loc), Bool(true)],
                )?;
            }
        }
        Ok(())
    }

    fn clock_sync(&mut self, mono_ns: u64, real_ns: u64) -> Result<()> {
        self.enc.write_event(
            &self.sch.clock_sync,
            mono_ns,
            &[FieldValue::Varint(real_ns)],
        )?;
        Ok(())
    }

    fn metadata(&mut self, ts: u64, entries: &[(String, String)]) -> Result<()> {
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = entries
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        self.enc
            .write_event(&self.sch.metadata, ts, &[FieldValue::StringMap(pairs)])?;
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.enc.finish()
    }
}

fn gzip(raw: &[u8], level: flate2::Compression) -> Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), level);
    enc.write_all(raw).context("gzip write")?;
    enc.finish().context("gzip finish")
}

fn standard_metadata(service: &str, host: &str) -> Vec<(String, String)> {
    vec![
        (
            "dial9.dial9-tokio-telemetry.version".into(),
            "0.0.0-fixture".into(),
        ),
        ("process.available_parallelism".into(), "8".into()),
        ("service".into(), service.into()),
        ("host".into(), host.into()),
    ]
}

// ── Committed family: 10-segment window with boundary polls (T17) ────────

struct WindowSegment {
    gz: Vec<u8>,
    epoch_s: i64,
    mono_start_ns: u64,
    mono_end_ns: u64,
    key: String,
}

struct WindowFamily {
    segments: Vec<WindowSegment>,
    planted: Vec<PlantedPoll>,
    workers: Vec<u64>,
    epoch_base_s: i64,
    segment_seconds: u64,
}

const WINDOW_SERVICE: &str = "svc-fix";
const WINDOW_HOST: &str = "window";
const WINDOW_BOOT: &str = "boot-fixw";
const WINDOW_SEGMENTS: usize = 10;
const WINDOW_SEGMENT_SECONDS: u64 = 60;

/// 10 contiguous 60s segments sharing ONE monotonic clock (like a real
/// rotating writer), with two planted boundary-spanning polls:
///
///   - P1: worker 1 opens a poll 5s before the seg0/seg1 boundary and closes
///     it 2s into seg1 (adjacent-boundary stitch);
///   - P2: worker 2 opens 4s before the end of seg3, is completely SILENT
///     through seg4, and closes 3s into seg5 (the T17-audit N-segment chain).
fn build_window_family(sch: &Schemas) -> Result<WindowFamily> {
    let workers: Vec<u64> = vec![1, 2, 3, 4];
    let epoch_base = fixture_epoch(19, 30, 0);
    let seg_ns = WINDOW_SEGMENT_SECONDS * NS;
    let mono = |i: usize| MONO_BASE_NS + i as u64 * seg_ns;

    let planted = vec![
        PlantedPoll {
            worker: 1,
            task: 101,
            loc: "src/fixture/pipeline.rs:11",
            start_segment: 0,
            start_ns: mono(1) - 5 * NS,
            end_segment: 1,
            end_ns: mono(1) + 2 * NS,
        },
        PlantedPoll {
            worker: 2,
            task: 202,
            loc: "src/fixture/compactor.rs:58",
            start_segment: 3,
            start_ns: mono(4) - 4 * NS,
            end_segment: 5,
            end_ns: mono(5) + 3 * NS,
        },
    ];

    let mut segments = Vec::with_capacity(WINDOW_SEGMENTS);
    for i in 0..WINDOW_SEGMENTS {
        let epoch_s = epoch_base + (i as u64 * WINDOW_SEGMENT_SECONDS) as i64;
        let open: Vec<(u64, u64, &'static str, u64)> = planted
            .iter()
            .filter(|p| p.start_segment == i)
            .map(|p| (p.worker, p.task, p.loc, p.start_ns))
            .collect();
        let close: Vec<(u64, u64)> = planted
            .iter()
            .filter(|p| p.end_segment == i)
            .map(|p| (p.worker, p.end_ns))
            .collect();
        // Workers silent because a planted poll spans right across them.
        let silent: Vec<u64> = planted
            .iter()
            .filter(|p| i > p.start_segment && i < p.end_segment)
            .map(|p| p.worker)
            .collect();

        let raw = build_segment(
            sch,
            &SegmentContent {
                mono_start_ns: mono(i),
                mono_end_ns: mono(i + 1),
                real_at_start_ns: epoch_s as u64 * NS,
                workers: &workers,
                silent: &silent,
                open_at_end: &open,
                close_at_start: &close,
                metadata: &standard_metadata(WINDOW_SERVICE, WINDOW_HOST),
                cycles_per_worker: 150,
                seed: fnv1a(WINDOW_HOST) ^ i as u64,
            },
        )?;
        segments.push(WindowSegment {
            gz: gzip(&raw, flate2::Compression::default())?,
            epoch_s,
            mono_start_ns: mono(i),
            mono_end_ns: mono(i + 1),
            key: layout_key(
                "traces",
                WINDOW_SERVICE,
                WINDOW_HOST,
                WINDOW_BOOT,
                epoch_s,
                0,
            ),
        });
    }

    Ok(WindowFamily {
        segments,
        planted,
        workers,
        epoch_base_s: epoch_base,
        segment_seconds: WINDOW_SEGMENT_SECONDS,
    })
}

/// Committed manifest: the facts the vitest suites assert. Nanosecond wall
/// values exceed 2^53, so anything realtime-scale is a STRING; monotonic
/// values stay numbers (< 2^53 by construction).
fn write_manifest(dir: &Path, w: &WindowFamily) -> Result<()> {
    let segments: Vec<serde_json::Value> = w
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            serde_json::json!({
                "file": format!("window-{i:02}.bin.gz"),
                "key": s.key,
                "epochS": s.epoch_s,
                "monoStartNs": s.mono_start_ns,
                "monoEndNs": s.mono_end_ns,
            })
        })
        .collect();
    let planted: Vec<serde_json::Value> = w
        .planted
        .iter()
        .map(|p| {
            serde_json::json!({
                "workerId": p.worker,
                "taskId": p.task,
                "spawnLoc": p.loc,
                "startSegment": p.start_segment,
                "startNs": p.start_ns,
                "endSegment": p.end_segment,
                "endNs": p.end_ns,
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "generator": "cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures",
        "service": WINDOW_SERVICE,
        "host": WINDOW_HOST,
        "boot": WINDOW_BOOT,
        "workers": w.workers,
        "epochBaseS": w.epoch_base_s,
        "segmentSeconds": w.segment_seconds,
        "monoBaseNs": MONO_BASE_NS,
        // realtime(ns) = clockOffsetNs + monotonic(ns); > 2^53, hence string.
        "clockOffsetNs": (w.epoch_base_s as u64 * NS - MONO_BASE_NS).to_string(),
        "segments": segments,
        "plantedPolls": planted,
        "multiRuntime": {
            "file": "multi-runtime.bin.gz",
            "runtimes": { (MULTI_RUNTIME_NAME): MULTI_RUNTIME_WORKERS.to_vec() },
            "mainWorkers": MAIN_WORKERS.to_vec(),
        },
    });
    let path = dir.join("manifest.json");
    let mut body = serde_json::to_string_pretty(&manifest).context("serializing manifest")?;
    body.push('\n');
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ── Committed family: multi-runtime (#596) segment ───────────────────────

const MULTI_RUNTIME_NAME: &str = "journal";
const MULTI_RUNTIME_WORKERS: [u64; 4] = [64, 65, 66, 67];
const MAIN_WORKERS: [u64; 4] = [0, 1, 2, 3];
const MULTI_RUNTIME_SERVICE: &str = "svc-alt";
const MULTI_RUNTIME_HOST: &str = "host-z";

/// One segment with an unnamed main block (workers 0..3) plus a named
/// `journal` runtime (workers 64..67) declared via the `runtime.<name>`
/// segment-metadata convention (`("runtime.journal", "64,65,66,67")`).
fn build_multi_runtime_segment(sch: &Schemas) -> Result<Vec<u8>> {
    let workers: Vec<u64> = MAIN_WORKERS
        .iter()
        .chain(MULTI_RUNTIME_WORKERS.iter())
        .copied()
        .collect();
    let mut metadata = standard_metadata(MULTI_RUNTIME_SERVICE, MULTI_RUNTIME_HOST);
    metadata.push((
        format!("runtime.{MULTI_RUNTIME_NAME}"),
        MULTI_RUNTIME_WORKERS.map(|w| w.to_string()).join(","),
    ));
    let epoch_s = fixture_epoch(19, 0, 0);
    let raw = build_segment(
        sch,
        &SegmentContent {
            mono_start_ns: MONO_BASE_NS,
            mono_end_ns: MONO_BASE_NS + 280 * NS,
            real_at_start_ns: epoch_s as u64 * NS,
            workers: &workers,
            silent: &[],
            open_at_end: &[],
            close_at_start: &[],
            metadata: &metadata,
            cycles_per_worker: 60,
            seed: fnv1a(MULTI_RUNTIME_HOST),
        },
    )?;
    gzip(&raw, flate2::Compression::default())
}

// ── Seed-tree families ───────────────────────────────────────────────────

struct SeedObject {
    bucket: &'static str,
    key: String,
    gz: Vec<u8>,
    /// Becomes the object's `last_modified` (heatmap segment END).
    mtime_s: i64,
}

fn write_seed_object(root: &Path, obj: &SeedObject) -> Result<()> {
    let path = root.join(obj.bucket).join(&obj.key);
    let parent = path.parent().context("seed object path has a parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    fs::write(&path, &obj.gz).with_context(|| format!("writing {}", path.display()))?;
    let mtime = SystemTime::UNIX_EPOCH
        + Duration::from_secs(u64::try_from(obj.mtime_s).context("mtime pre-1970")?);
    let file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("reopening {} to set mtime", path.display()))?;
    file.set_times(fs::FileTimes::new().set_modified(mtime))
        .with_context(|| format!("setting mtime on {}", path.display()))?;
    Ok(())
}

/// A small standard-activity segment for the browse-layout hosts. Content is
/// seeded per (service, host) ONLY, and the activity window is a fixed 280s
/// relative to the segment's own monotonic base — so all segments of a host
/// differ solely in their ClockSync epoch (same LEB128 width on the fixture
/// day) and encode to byte-identical raw LENGTHS. Stored gzip (level 0)
/// preserves that equality in the LISTED object size, which is what the
/// heatmap density spreads — the seam scenario's strictly uniform density
/// (F7's observable) depends on it.
fn build_small_layout_segment(
    sch: &Schemas,
    service: &str,
    host: &str,
    epoch_s: i64,
) -> Result<Vec<u8>> {
    let raw = build_segment(
        sch,
        &SegmentContent {
            mono_start_ns: MONO_BASE_NS,
            mono_end_ns: MONO_BASE_NS + 280 * NS,
            real_at_start_ns: epoch_s as u64 * NS,
            workers: &[1, 2, 3, 4],
            silent: &[],
            open_at_end: &[],
            close_at_start: &[],
            metadata: &standard_metadata(service, host),
            cycles_per_worker: 60,
            seed: fnv1a(service) ^ fnv1a(host),
        },
    )?;
    gzip(&raw, flate2::Compression::none())
}

/// The `dial9-fixtures` bucket: one heatmap row per scenario host, all under
/// `traces/` on the fixture day (reachable from the pinned page clock).
///
///   svc-alt / host-z   one segment, multi-runtime content (F4 multi-service);
///   svc-fix / boots    three segments with three boot ids (F5 label, F9
///                      dashed dividers at 19:05 and 19:10);
///   svc-fix / gap      segments 19:00-19:05 and 19:15-19:20: a genuine
///                      10-minute coverage hole (F8 hatching);
///   svc-fix / seam     19:00 with mtime 19:10 overlapping 19:05-19:10: an
///                      upload-lag seam the tiling must not double-count (F7);
///   svc-fix / window   the 10-segment windowing set (same bytes as the
///                      committed fixtures).
fn browse_layout_objects(
    sch: &Schemas,
    window: &WindowFamily,
    multi_runtime_gz: &[u8],
) -> Result<Vec<SeedObject>> {
    const BUCKET: &str = "dial9-fixtures";
    let mut out = Vec::new();

    // svc-alt/host-z: the multi-runtime segment, doubling as a second service.
    let alt_epoch = fixture_epoch(19, 0, 0);
    out.push(SeedObject {
        bucket: BUCKET,
        key: layout_key("traces", "svc-alt", "host-z", "boot-alt1", alt_epoch, 0),
        gz: multi_runtime_gz.to_vec(),
        mtime_s: alt_epoch + 300,
    });

    // svc-fix/boots: three contiguous segments, three boot ids.
    for (i, boot) in ["boot-fixb1", "boot-fixb2", "boot-fixb3"]
        .iter()
        .enumerate()
    {
        let epoch = fixture_epoch(19, i as u8 * 5, 0);
        out.push(SeedObject {
            bucket: BUCKET,
            key: layout_key("traces", "svc-fix", "boots", boot, epoch, 0),
            gz: build_small_layout_segment(sch, "svc-fix", "boots", epoch)?,
            mtime_s: epoch + 300,
        });
    }

    // svc-fix/gap: 19:00-19:05, then nothing until 19:15-19:20.
    for start_min in [0u8, 15] {
        let epoch = fixture_epoch(19, start_min, 0);
        out.push(SeedObject {
            bucket: BUCKET,
            key: layout_key("traces", "svc-fix", "gap", "boot-fixg", epoch, 0),
            gz: build_small_layout_segment(sch, "svc-fix", "gap", epoch)?,
            mtime_s: epoch + 300,
        });
    }

    // svc-fix/seam: the first segment's mtime overshoots the second's start
    // by five minutes (upload lag). Identical content lengths by
    // construction; asserted because F7's uniform-density observable
    // depends on it.
    let seam_a_epoch = fixture_epoch(19, 0, 0);
    let seam_b_epoch = fixture_epoch(19, 5, 0);
    let seam_a = build_small_layout_segment(sch, "svc-fix", "seam", seam_a_epoch)?;
    let seam_b = build_small_layout_segment(sch, "svc-fix", "seam", seam_b_epoch)?;
    ensure!(
        seam_a.len() == seam_b.len(),
        "seam segments must be size-identical (got {} vs {})",
        seam_a.len(),
        seam_b.len()
    );
    out.push(SeedObject {
        bucket: BUCKET,
        key: layout_key("traces", "svc-fix", "seam", "boot-fixs", seam_a_epoch, 0),
        gz: seam_a,
        mtime_s: seam_b_epoch + 300, // overshoots seam_b's start: the seam
    });
    out.push(SeedObject {
        bucket: BUCKET,
        key: layout_key("traces", "svc-fix", "seam", "boot-fixs", seam_b_epoch, 0),
        gz: seam_b,
        mtime_s: seam_b_epoch + 300,
    });

    // svc-fix/window: the committed windowing set, served from S3 too.
    for seg in &window.segments {
        out.push(SeedObject {
            bucket: BUCKET,
            key: seg.key.clone(),
            gz: seg.gz.clone(),
            mtime_s: seg.epoch_s + WINDOW_SEGMENT_SECONDS as i64,
        });
    }

    Ok(out)
}

/// The `dial9-fixtures-dates` bucket: date partitions at the ROOT (no key
/// prefix at all) — the #471 layout that makes prefix discovery set the
/// prefix EMPTY (features/01 D4).
fn dates_bucket_object(sch: &Schemas) -> Result<SeedObject> {
    let epoch = fixture_epoch(19, 0, 0);
    Ok(SeedObject {
        bucket: "dial9-fixtures-dates",
        key: layout_key("", "svc-dates", "host-1", "boot-d1", epoch, 0),
        gz: build_small_layout_segment(sch, "svc-dates", "host-1", epoch)?,
        mtime_s: epoch + 300,
    })
}

/// The `dial9-fixtures-large` bucket: eight ~28 MB one-minute segments
/// (gzip level 0, so listed size ~= raw size) on one host. A full-row
/// selection is ~224 MB — past the 200 MiB cap (H4/H5) — and the set is
/// T39's reproducible >=100 MB-raw parse workload.
fn large_family_objects(sch: &Schemas) -> Result<Vec<SeedObject>> {
    const SERVICE: &str = "svc-big";
    const HOST: &str = "host-01";
    let workers: Vec<u64> = vec![1, 2, 3, 4];

    // Calibrate bytes-per-cycle so the cycle count lands near the target
    // without guessing at encoding overhead (u24 deltas, resets, varints).
    let probe = |cycles: usize| -> Result<usize> {
        let raw = build_segment(
            sch,
            &SegmentContent {
                mono_start_ns: MONO_BASE_NS,
                mono_end_ns: MONO_BASE_NS + 60 * NS,
                real_at_start_ns: fixture_epoch(19, 0, 0) as u64 * NS,
                workers: &workers,
                silent: &[],
                open_at_end: &[],
                close_at_start: &[],
                metadata: &standard_metadata(SERVICE, HOST),
                cycles_per_worker: cycles,
                seed: fnv1a(HOST),
            },
        )?;
        Ok(raw.len())
    };
    let (n0, n1) = (2_000usize, 4_000usize);
    let (b0, b1) = (probe(n0)?, probe(n1)?);
    ensure!(b1 > b0, "calibration probes must grow with cycle count");
    let bytes_per_cycle = (b1 - b0) as f64 / (n1 - n0) as f64;
    let base = b0 as f64 - bytes_per_cycle * n0 as f64;
    let cycles = ((LARGE_SEGMENT_TARGET_RAW as f64 - base) / bytes_per_cycle).ceil() as usize;

    let mut out = Vec::with_capacity(LARGE_SEGMENT_COUNT);
    let mut total: u64 = 0;
    for i in 0..LARGE_SEGMENT_COUNT {
        let epoch = fixture_epoch(19, 0, 0) + i as i64 * 60;
        let raw = build_segment(
            sch,
            &SegmentContent {
                mono_start_ns: MONO_BASE_NS + i as u64 * 60 * NS,
                mono_end_ns: MONO_BASE_NS + (i as u64 + 1) * 60 * NS,
                real_at_start_ns: epoch as u64 * NS,
                workers: &workers,
                silent: &[],
                open_at_end: &[],
                close_at_start: &[],
                metadata: &standard_metadata(SERVICE, HOST),
                cycles_per_worker: cycles,
                seed: fnv1a(HOST) ^ i as u64,
            },
        )?;
        ensure!(
            raw.len() >= LARGE_SEGMENT_TARGET_RAW * 9 / 10,
            "large segment {i} undershot its raw target: {} < {}",
            raw.len(),
            LARGE_SEGMENT_TARGET_RAW
        );
        // Level 0 (stored): listed object size ~= raw size, which is what
        // the selection cap sums.
        let gz = gzip(&raw, flate2::Compression::none())?;
        total += gz.len() as u64;
        out.push(SeedObject {
            bucket: "dial9-fixtures-large",
            key: layout_key("traces", SERVICE, HOST, "boot-big1", epoch, 0),
            gz,
            mtime_s: epoch + 60,
        });
    }
    ensure!(
        total > SELECTION_CAP_BYTES,
        "large family must exceed the {SELECTION_CAP_BYTES}-byte selection cap (got {total})"
    );
    Ok(out)
}
