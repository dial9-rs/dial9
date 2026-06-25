pub mod cli;
pub mod report_serve;
pub mod server;
pub mod storage;

pub use report_serve::report_serve_router;

use std::path::PathBuf;

async fn detect_bucket_region(bucket: &str) -> Option<String> {
    // HeadBucket needs *some* region to construct the S3 endpoint, even though
    // the response header will tell us the real region. Use us-east-1 as a
    // probe region — S3 global endpoint lives there and will redirect with
    // `x-amz-bucket-region` for buckets in other regions.
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .load()
        .await;
    let client = aws_sdk_s3::Client::new(&config);
    server::region_from_head_bucket(&client, bucket).await
}

/// Configuration for the `serve` subcommand, assembled from CLI args.
pub(crate) struct ServeConfig {
    pub port: u16,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub local_dir: Option<PathBuf>,
    pub dev: bool,
    pub enable_upload: bool,
    pub enable_sharing: bool,
}

pub(crate) async fn serve(
    ServeConfig {
        port,
        bucket,
        prefix,
        local_dir,
        dev,
        enable_upload,
        enable_sharing,
    }: ServeConfig,
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
    if enable_upload {
        tracing::info!(
            "trace-upload feature enabled (POST /api/upload); no auth — trusted network only"
        );
        app_state = app_state.with_uploads(server::UploadLimits::default());
    }
    if enable_sharing {
        if app_state.default_bucket.is_some() {
            tracing::info!(
                "sharing feature enabled (POST /api/share); no auth — trusted network only"
            );
            app_state = app_state.with_sharing(true);

            // Smoke-test: verify the ambient credentials can write to the bucket.
            let bucket_name = app_state.default_bucket.as_deref().unwrap();
            let check_key = "dial9-smoketest/check-writable";
            match app_state
                .backend
                .put_object(bucket_name, check_key, b"ok".to_vec(), None)
                .await
            {
                Ok(()) => {
                    tracing::info!(bucket = %bucket_name, "bucket write check passed");
                }
                Err(e) => {
                    tracing::warn!(
                        bucket = %bucket_name,
                        error = %e,
                        "bucket write check failed — sharing may not work"
                    );
                }
            }
        } else {
            tracing::warn!("--enable-sharing ignored: requires --bucket");
        }
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
