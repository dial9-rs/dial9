use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, recorder};
use std::time::Duration;

/// After the recorder is dropped, all trace files should be sealed (.bin),
/// with no .active files remaining. This is the contract the worker depends on.
#[test]
fn guard_drop_produces_sealed_bin_files() {
    let dir = tempfile::tempdir().unwrap();

    let writer = DiskBuffer::builder()
        .base_path(dir.path())
        .max_file_size(1024)
        .max_total_size(1024 * 1024)
        .build()
        .unwrap();
    let recorder = recorder(writer).build();
    let (recorder, rt) = recorder
        .attach_runtime(|t| {
            t.worker_threads(2);
        })
        .expect("build tokio runtime");

    rt.block_on(async {
        for _ in 0..100 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    drop(rt);
    drop(recorder);

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();

    let bin_files: Vec<_> = entries
        .iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "bin"))
        .collect();
    let active_files: Vec<_> = entries
        .iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "active"))
        .collect();

    assert!(!bin_files.is_empty(), "should have at least one .bin file");
    assert!(
        active_files.is_empty(),
        "no .active files should remain after the recorder is dropped, found: {:?}",
        active_files.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}
