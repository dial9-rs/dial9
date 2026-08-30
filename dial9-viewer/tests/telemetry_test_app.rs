//! End-to-end coverage for the self-describing telemetry test application.
//!
//! The expectation reader is kept as a private helper because its event and
//! naming conventions belong only to this test.

#[path = "telemetry_test_app/expectations.rs"]
mod expectations;

#[cfg(target_os = "linux")]
#[path = "telemetry_test_app/local_js.rs"]
mod local_js;

#[cfg(target_os = "linux")]
#[test]
fn local_javascript_matches_the_self_described_fixture() {
    use flate2::read::GzDecoder;
    use std::{ffi::OsStr, io::Read as _, path::Path, process::Command};

    let trace_dir = tempfile::tempdir().expect("create fixture trace directory");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("dial9-viewer must be inside the workspace");
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .args(["run", "--release", "-p", "telemetry-test-app", "--"])
        .arg("--trace-dir")
        .arg(trace_dir.path())
        .output()
        .expect("run telemetry-test-app");
    assert!(
        output.status.success(),
        "telemetry-test-app failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut trace_paths: Vec<_> = std::fs::read_dir(trace_dir.path())
        .expect("read fixture trace directory")
        .map(|entry| entry.expect("read fixture trace entry").path())
        .filter(|path| {
            path.extension() == Some(OsStr::new("bin"))
                || path.extension() == Some(OsStr::new("gz"))
        })
        .collect();
    trace_paths.sort();
    assert!(
        !trace_paths.is_empty(),
        "fixture produced no trace segments"
    );

    let raw_segments: Vec<Vec<u8>> = trace_paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).expect("read fixture trace segment");
            if bytes.starts_with(&[0x1f, 0x8b]) {
                let mut raw = Vec::new();
                GzDecoder::new(bytes.as_slice())
                    .read_to_end(&mut raw)
                    .expect("decompress fixture trace segment");
                raw
            } else {
                bytes
            }
        })
        .collect();
    let expected = expectations::read_expected_model(raw_segments.iter().map(Vec::as_slice))
        .expect("read fixture expectations");
    local_js::check_local_trace(&trace_paths, &expected)
        .expect("local JavaScript observations must match fixture expectations");
}
