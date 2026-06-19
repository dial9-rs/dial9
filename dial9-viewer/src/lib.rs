pub mod cli;
pub mod report_serve;
pub mod server;
pub mod storage;

pub use report_serve::report_serve_router;

use std::path::PathBuf;

async fn detect_bucket_region(bucket: &str) -> Option<String> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    server::region_from_head_bucket(&client, bucket).await
}

pub(crate) async fn serve(
    port: u16,
    bucket: Option<String>,
    prefix: Option<String>,
    local_dir: Option<PathBuf>,
    dev: bool,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dial9_viewer=info".parse().unwrap()),
        )
        .init();

    let dev_ui_dir = if dev {
        let candidates = [PathBuf::from("ui"), PathBuf::from("dial9-viewer/ui")];
        let dir = candidates.into_iter().find(|p| p.exists());
        match dir {
            Some(d) => {
                tracing::info!(path = %d.display(), "dev mode: serving UI from disk");
                Some(d)
            }
            None => {
                anyhow::bail!(
                    "--dev: could not find ui/ directory. Run from the dial9-viewer/ or repo root directory."
                );
            }
        }
    } else {
        None
    };

    // Build the base state per backend. `allow_byo` is true for every S3
    // backend — users may always optionally supply their own credentials; it is
    // false only in local-dir mode, where the data is local and credentials are
    // meaningless.
    let (mut app_state, allow_byo) = if let Some(dir) = &local_dir {
        let dir = std::fs::canonicalize(dir)?;
        tracing::info!(path = %dir.display(), "serving traces from local directory");
        let backend = storage::LocalBackend::new(&dir);
        let state = server::AppState::new(
            std::sync::Arc::new(backend),
            Some("local".into()),
            prefix.clone(),
        );
        (state, false)
    } else if let Some(bucket_name) = &bucket {
        if let Some(region) = detect_bucket_region(bucket_name).await {
            tracing::info!(%region, bucket = %bucket_name, "detected bucket region");
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(region))
                .load()
                .await;
            let client = aws_sdk_s3::Client::new(&config);
            let backend = storage::S3Backend::from_client(client);
            let state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            (state, true)
        } else {
            tracing::warn!(bucket = %bucket_name, "could not detect bucket region, using default");
            let backend = storage::S3Backend::from_env().await;
            let state =
                server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
            (state, true)
        }
    } else {
        let backend = storage::S3Backend::from_env().await;
        let state =
            server::AppState::new(std::sync::Arc::new(backend), bucket.clone(), prefix.clone());
        (state, true)
    };

    app_state = app_state.with_byo_creds(allow_byo);
    if let Some(d) = dev_ui_dir {
        app_state = app_state.with_dev_ui_dir(d);
    }

    let app = server::router(app_state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, dev, "dial9-viewer listening");
    println!("\n  → http://localhost:{}\n", port);
    if let Some(dir) = &local_dir {
        tracing::info!(path = %dir.display(), "local directory mode");
    } else if let Some(bucket) = &bucket {
        tracing::info!(%bucket, "default bucket");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    tracing::info!("shutting down");
}
