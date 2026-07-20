#[cfg(feature = "self-telemetry")]
fn dial9_config() -> dial9_tokio_telemetry::Dial9Config {
    dial9_tokio_telemetry::Dial9Config::from_env()
}

#[cfg(feature = "self-telemetry")]
#[dial9_tokio_telemetry::main(config = dial9_config)]
async fn main() -> anyhow::Result<()> {
    dial9_viewer::cli::run().await
}

#[cfg(not(feature = "self-telemetry"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dial9_viewer::cli::run().await
}
