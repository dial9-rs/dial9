//! Per-process namespace isolation: trace segments from the managed
//! `Dial9Config` disk path must land in a `{boot_id}/` subdirectory, and dead
//! peers' directories must be reclaimed (or kept) per the GC setting.

use std::path::{Path, PathBuf};

use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};

/// Names a directory entry that looks like a boot_id (`{4-alpha}-{pid}`).
fn is_boot_id_dir(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some((alpha, pid)) = name.split_once('-') else {
        return false;
    };
    alpha.len() == 4
        && alpha.bytes().all(|b| b.is_ascii_lowercase())
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
}

fn boot_id_dirs(trace_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(trace_dir)
        .expect("trace dir should exist")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_boot_id_dir(p))
        .collect()
}

fn has_trace_segment(boot_dir: &Path) -> bool {
    std::fs::read_dir(boot_dir)
        .expect("boot dir should exist")
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().starts_with("trace."))
}

/// Build a disk-backed runtime under `trace_dir`, run a trivial workload, and
/// shut it down so segments are sealed.
fn run_workload(trace_dir: &Path, gc_dead_namespaces: bool) {
    let cfg = Dial9Config::builder()
        .on_disk_buffer(trace_dir.join("trace.bin"))
        .max_total_size(4 * 1024 * 1024)
        .gc_dead_namespaces(gc_dead_namespaces)
        .build()
        .expect("config should build");
    let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
    assert!(rt.guard().is_enabled());
    rt.block_on(async {
        tokio::task::yield_now().await;
    });
    // Dropping the runtime drops its guard, which flushes and seals segments.
    drop(rt);
}

#[test]
fn traces_land_in_boot_id_subdir() {
    let dir = tempfile::tempdir().unwrap();
    run_workload(dir.path(), true);

    let dirs = boot_id_dirs(dir.path());
    assert_eq!(
        dirs.len(),
        1,
        "expected exactly one boot_id subdir: {dirs:?}"
    );
    assert!(
        has_trace_segment(&dirs[0]),
        "boot_id subdir should contain trace segments"
    );

    // No stray trace files in the parent — everything is namespaced.
    let stray = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().is_file() && e.file_name().to_string_lossy().starts_with("trace."));
    assert!(
        !stray,
        "no trace files should sit directly in the parent dir"
    );
}

/// With GC disabled, a leftover dead peer directory is preserved — the local
/// "keep traces from multiple runs" flow.
#[cfg(unix)]
#[test]
fn gc_disabled_keeps_previous_run() {
    let dir = tempfile::tempdir().unwrap();

    // Simulate a previous, now-dead run: a boot_id dir with a sealed segment
    // and an unlocked .lock file.
    let dead = dir.path().join("aaaa-1");
    std::fs::create_dir(&dead).unwrap();
    std::fs::write(dead.join(".lock"), b"").unwrap();
    std::fs::write(dead.join("trace.0.bin"), b"old trace").unwrap();

    run_workload(dir.path(), false);

    assert!(
        dead.exists(),
        "dead peer dir must survive when gc_dead_namespaces is false"
    );
    // Our run added its own dir, so there are at least two.
    assert!(
        boot_id_dirs(dir.path()).len() >= 2,
        "both the old and new run dirs should be present"
    );
}

/// With GC enabled, a leftover dead peer directory is reclaimed at startup.
#[cfg(unix)]
#[test]
fn gc_enabled_reclaims_dead_peer() {
    let dir = tempfile::tempdir().unwrap();

    let dead = dir.path().join("aaaa-1");
    std::fs::create_dir(&dead).unwrap();
    std::fs::write(dead.join(".lock"), b"").unwrap();
    std::fs::write(dead.join("trace.0.bin"), b"old trace").unwrap();
    std::fs::write(dead.join("trace.0.bin.gz"), b"old gz").unwrap();

    run_workload(dir.path(), true);

    assert!(
        !dead.exists(),
        "dead peer dir must be reclaimed when gc_dead_namespaces is true"
    );
}

/// The S3 uploader's boot_id must match the on-disk namespace directory, so a
/// local segment and its upload share one identity. We can't reach S3 from a
/// unit test, but the same boot_id is embedded in every sealed segment's
/// `SegmentMetadata`, so decoding it and comparing to the directory name
/// proves the injection end to end.
#[cfg(all(unix, feature = "worker-s3"))]
#[test]
fn s3_boot_id_matches_namespace_dir() {
    use std::collections::HashMap;

    use dial9_tokio_telemetry::background_task::s3::S3Config;
    use dial9_trace_format::decoder::Decoder;

    let dir = tempfile::tempdir().unwrap();
    let cfg = Dial9Config::builder()
        .on_disk_buffer(dir.path().join("trace.bin"))
        .max_total_size(4 * 1024 * 1024)
        .with_runtime(|r| {
            r.with_s3_uploader::<dial9_tokio_telemetry::telemetry::Disk>(
                S3Config::builder()
                    .bucket("test-bucket")
                    .service_name("test-svc")
                    .build(),
            )
        })
        .build()
        .expect("config should build");
    let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
    rt.block_on(async {
        tokio::task::yield_now().await;
    });
    drop(rt);

    let boot_dir = boot_id_dirs(dir.path())
        .into_iter()
        .next()
        .expect("a boot_id namespace dir");
    let boot_id = boot_dir.file_name().unwrap().to_str().unwrap();

    let sealed = std::fs::read_dir(&boot_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "bin"))
        .expect("a sealed .bin segment");

    let data = std::fs::read(&sealed).unwrap();
    let mut dec = Decoder::new(&data).unwrap();

    #[derive(serde::Deserialize)]
    #[serde(tag = "event")]
    enum Event {
        SegmentMetadataEvent {
            entries: HashMap<String, String>,
        },
        #[serde(other)]
        Other,
    }

    let mut boot_ids = Vec::new();
    dec.for_each_event(|raw| {
        if let Event::SegmentMetadataEvent { entries } = raw.deserialize().expect("deserialize")
            && let Some(b) = entries.get("boot_id")
        {
            boot_ids.push(b.clone());
        }
    })
    .unwrap();

    assert!(!boot_ids.is_empty(), "expected a boot_id metadata entry");
    assert!(
        boot_ids.iter().all(|b| b == boot_id),
        "segment boot_id metadata {boot_ids:?} must match namespace dir {boot_id:?}"
    );
}
