use super::observations::Observations;
use anyhow::{Context as _, Result, ensure};
use std::{path::Path, process::Command};

pub(crate) fn observe_local_trace(trace_paths: &[impl AsRef<Path>]) -> Result<Observations> {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/telemetry_test_app/check_local.js");
    let output = Command::new("node")
        .arg(script)
        .args(trace_paths.iter().map(AsRef::as_ref))
        .output()
        .context("run local JavaScript telemetry checker with Node.js")?;
    ensure!(
        output.status.success(),
        "local JavaScript telemetry checker failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decode local JavaScript telemetry observations")
}
