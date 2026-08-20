/// Dev helper: starts an s3s fake S3 server, seeds it with test trace data,
/// then starts the dial9-viewer pointed at it.
///
/// Usage: cargo run -p dial9-viewer --bin dev-server
///
/// Optional fixture seeding (T42, see ui/README.md "Live UI checks"):
///   DIAL9_SEED_DIR=<dir>       seed extra buckets from a tree shaped
///                              `<dir>/<bucket>/<key path...>`. Each file's
///                              mtime is preserved into the fake S3 (s3s-fs
///                              derives `last_modified` from it — fixture
///                              scenarios depend on that). Generate the tree
///                              with the `gen-fixtures` bin.
///   DIAL9_DEFAULT_PREFIX=<p>   override the server's default key prefix
///                              ("traces"); an EMPTY value means no default
///                              prefix (required by the date-root fixture,
///                              features/01 D4/#471).
use std::io::Write;

#[allow(dead_code)]
#[path = "../segment_object_key_codec.rs"]
mod segment_object_key_codec;
use segment_object_key_codec::format_v1_segment_object_key;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("dial9_viewer=info,dev_server=info")
        .init();

    // Set up s3s-fs backed fake S3
    let s3_root = tempfile::tempdir()?;
    let bucket = "demo-traces";
    std::fs::create_dir(s3_root.path().join(bucket))?;

    // Build an s3s-backed HTTP client over the fake FS. `s3s_aws::Client` isn't
    // Clone, so make a second one (sharing the same FS root) for the
    // bring-your-own-credentials ephemeral path below.
    let make_s3s_http_client = || -> anyhow::Result<s3s_aws::Client> {
        let fs = s3s_fs::FileSystem::new(s3_root.path()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        Ok(builder.build().into())
    };

    let s3_client = make_s3s_http_client()?;
    let http_client = aws_sdk_s3::config::SharedHttpClient::new(make_s3s_http_client()?);

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();

    let client = aws_sdk_s3::Client::from_conf(s3_config);

    // Seed with demo trace data — use the actual demo-trace.bin if available
    let demo_trace_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/public/demo-trace.bin");

    if demo_trace_path.exists() {
        let compressed = std::fs::read(&demo_trace_path)?;
        // demo-trace.bin is gzipped — decompress before splitting
        let demo_data = gunzip_bytes(&compressed);

        // Upload the full trace as a single gzipped segment
        let full_compressed = gzip_bytes(&demo_data);
        let epoch_secs = 1_785_975_935;
        let key = format_v1_segment_object_key(
            Some("traces"),
            "2026-08-06",
            "demo-service",
            "0025",
            "local/host-0",
            "abcd",
            &format!("{epoch_secs}-0.bin.gz"),
        );
        client
            .put_object()
            .bucket(bucket)
            .key(&key)
            .body(full_compressed.into())
            .send()
            .await?;
        set_object_mtime(&s3_root.path().join(bucket).join(&key), epoch_secs + 5)?;
        tracing::info!(key, size = demo_data.len(), "seeded full demo trace");
    } else {
        tracing::warn!("demo-trace.bin not found, seeding with synthetic data");
        for i in 0..5 {
            let data = format!("synthetic trace segment {i}");
            let compressed = gzip_bytes(data.as_bytes());
            let epoch_secs = 1_785_976_200 + i * 60;
            let key = format_v1_segment_object_key(
                Some("traces"),
                "2026-08-06",
                "test-svc",
                &format!("003{i}"),
                "local/host-1",
                "xyzw",
                &format!("{epoch_secs}-0.bin.gz"),
            );
            client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(compressed.into())
                .send()
                .await?;
            set_object_mtime(&s3_root.path().join(bucket).join(&key), epoch_secs + 60)?;
            tracing::info!(%key, "seeded");
        }
    }

    // T42 fixture seeding: copy a pre-generated bucket tree into the s3s root.
    // Objects in s3s-fs are plain files at `<root>/<bucket>/<key>` (the demo
    // seeding above already relies on `create_dir` for the bucket), so a
    // mtime-preserving copy is equivalent to a PUT — and mtime is exactly the
    // knob the fixtures need, since s3s-fs reports it as `last_modified`.
    if let Ok(seed_dir) = std::env::var("DIAL9_SEED_DIR") {
        let seeded = seed_tree(std::path::Path::new(&seed_dir), s3_root.path())?;
        tracing::info!(dir = %seed_dir, objects = seeded, "seeded fixture tree");
    }

    // Start the viewer with the s3s-backed S3Backend. Marking the source as S3
    // enables bring-your-own credentials (pointed at the same s3s fake) so the
    // credentials panel can be exercised end-to-end: use access key id `test`,
    // secret `test`, region `us-east-1`.
    // Default prefix: "traces" (matches the demo seed), overridable for
    // fixture runs — an empty DIAL9_DEFAULT_PREFIX means NO default prefix.
    let default_prefix = match std::env::var("DIAL9_DEFAULT_PREFIX") {
        Ok(p) if p.is_empty() => None,
        Ok(p) => Some(p),
        Err(_) => Some("traces".to_string()),
    };

    let backend = dial9_viewer::storage::S3Backend::from_client(client);
    let state = dial9_viewer::server::AppState::new(
        std::sync::Arc::new(backend),
        Some(bucket.to_string()),
        default_prefix.clone(),
    )
    .with_byo_creds(true)
    .with_ephemeral_s3(dial9_viewer::storage::EphemeralS3Config {
        http_client,
        endpoint_url: Some("http://localhost:0".to_string()),
        force_path_style: true,
    });

    // Serve the BUILT UI (ui/dist) from disk, not the ui/ sources: the
    // servable set is the vite build output (root assets such as
    // demo-trace.bin and flamegraph.css live in ui/public/ and only appear at
    // the served root via a build). Two dev loops (ADR-0004 section 3):
    //   - single-server: `npm run dev:embedded` (vite build --watch) keeps
    //     ui/dist fresh while this server serves it (edit -> refresh);
    //   - proxy: `npm run dev` serves the UI itself with HMR and proxies
    //     /api/* here, in which case ui/dist may legitimately be empty or
    //     stale - hence a warning below, not an error.
    let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    if !ui_dir.join("index.html").exists() {
        tracing::warn!(
            path = %ui_dir.display(),
            "ui/dist has no built UI - run `npm run build` or `npm run dev:embedded` in \
             dial9-viewer/ui (or use `npm run dev` and browse the Vite server instead)"
        );
    }
    let state = state.with_dev_ui_dir(ui_dir);
    let app = dial9_viewer::server::router(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("dial9-viewer dev server listening on http://localhost:{port}");
    tracing::info!(
        "bucket={bucket}, prefix={}",
        default_prefix.as_deref().unwrap_or("(none)")
    );
    tracing::info!("try: http://localhost:{port}/");
    tracing::info!("search for: version=1/date=2026-08-06/");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}

/// Copy a fixture tree (`<dir>/<bucket>/<key path...>`) into the s3s-fs root,
/// preserving each file's mtime (the object's `last_modified`). Hard-links
/// where the filesystem allows it, falling back to copy + explicit mtime.
/// Returns the number of objects seeded.
fn seed_tree(seed_dir: &std::path::Path, s3_root: &std::path::Path) -> anyhow::Result<usize> {
    use anyhow::Context as _;

    if !seed_dir.is_dir() {
        anyhow::bail!(
            "DIAL9_SEED_DIR {} is not a directory — generate it with:\n  \
             cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures",
            seed_dir.display()
        );
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(seed_dir, &mut files).with_context(|| format!("walking {}", seed_dir.display()))?;
    files.sort(); // deterministic seeding order (for logs and debugging)

    let mut seeded = 0usize;
    for src in files {
        let rel = src
            .strip_prefix(seed_dir)
            .context("walked file must live under the seed dir")?;
        // First component is the bucket; the rest is the object key.
        anyhow::ensure!(
            rel.components().count() >= 2,
            "seed file {} is not under a bucket directory",
            src.display()
        );
        let dst = s3_root.join(rel);
        let parent = dst.parent().context("seed destination has a parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        if std::fs::hard_link(&src, &dst).is_err() {
            // Cross-device (or unsupported): copy, then carry the mtime over.
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
            let mtime = src.metadata()?.modified()?;
            let f = std::fs::OpenOptions::new()
                .append(true)
                .open(&dst)
                .with_context(|| format!("reopening {} to set mtime", dst.display()))?;
            f.set_times(std::fs::FileTimes::new().set_modified(mtime))
                .with_context(|| format!("setting mtime on {}", dst.display()))?;
        }
        seeded += 1;
    }
    Ok(seeded)
}

fn set_object_mtime(path: &std::path::Path, epoch_secs: u64) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.set_times(std::fs::FileTimes::new().set_modified(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs),
    ))
}

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn gunzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    out
}
