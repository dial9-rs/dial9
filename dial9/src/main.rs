#[cfg(feature = "self-telemetry")]
#[dial9::main(config = dial9::recorder_from_env)]
async fn main() -> anyhow::Result<()> {
    dial9_viewer::cli::run().await
}

#[cfg(not(feature = "self-telemetry"))]
fn main() -> anyhow::Result<()> {
    dial9_viewer::cli::run_blocking()
}
