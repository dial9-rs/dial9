use assert2::check;
use dial9_viewer::server::{AppState, UploadLimits, router};
use dial9_viewer::storage::{LocalBackend, ObjectInfo, S3Backend, StorageBackend, StorageError};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// In-memory fake backend for tests that don't need S3.
struct FakeBackend;

impl StorageBackend for FakeBackend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn list_objects(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn get_object(
        &self,
        _bucket: &str,
        _key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::NotFound("fake".into())) })
    }
}

fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(fake_s3_http_client(fs_root))
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(s3_config)
}

/// Build the s3s-backed HTTP client (without wrapping it in an `aws_sdk_s3`
/// client). Used both by [`fake_s3_client`] and by the ephemeral
/// bring-your-own-credentials path, which needs to inject this connector into
/// `EphemeralS3Config`.
fn fake_s3_http_client(fs_root: &std::path::Path) -> aws_sdk_s3::config::SharedHttpClient {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let mut builder = s3s::service::S3ServiceBuilder::new(fs);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();
    aws_sdk_s3::config::SharedHttpClient::new(s3_client)
}

/// An `EphemeralS3Config` pointed at the s3s fake, so the header → ephemeral
/// client → fake-S3 path can be exercised in tests.
fn fake_ephemeral_config(fs_root: &std::path::Path) -> dial9_viewer::storage::EphemeralS3Config {
    dial9_viewer::storage::EphemeralS3Config {
        http_client: fake_s3_http_client(fs_root),
        // s3s ignores the host, but an endpoint is required so the SDK doesn't
        // try to resolve real S3 DNS.
        endpoint_url: Some("http://localhost:0".to_string()),
        force_path_style: true,
    }
}

/// A backend that always errors — used to prove a request was served by the
/// header-supplied ephemeral backend and NOT the server's default backend.
struct ErroringBackend;

impl StorageBackend for ErroringBackend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::Other("default backend used".into())) })
    }

    fn list_objects(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::Other("default backend used".into())) })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::Other("default backend used".into())) })
    }

    fn get_object(
        &self,
        _bucket: &str,
        _key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        Box::pin(async { Err(StorageError::Other("default backend used".into())) })
    }
}

async fn start_server(state: AppState) -> String {
    let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let state = state.with_dev_ui_dir(ui_dir);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// --- basic tests ---

#[tokio::test]
async fn serves_static_files() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/index.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("dial9"));

    let resp = client
        .get(format!("{base}/viewer.html"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("Trace Viewer"));
}

#[tokio::test]
async fn search_requires_bucket() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn search_uses_default_bucket() {
    let state = AppState::new(Arc::new(FakeBackend), Some("my-bucket".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.is_empty());
}

#[tokio::test]
async fn trace_requires_keys() {
    let state = AppState::new(Arc::new(FakeBackend), Some("test-bucket".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/trace?keys=&bucket=test-bucket"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn config_returns_defaults() {
    let state = AppState::new(
        Arc::new(FakeBackend),
        Some("my-bucket".into()),
        Some("my-prefix".into()),
    );
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["default_bucket"] == "my-bucket");
    check!(resp["default_prefix"] == "my-prefix");
}

#[tokio::test]
async fn config_returns_nulls_when_no_defaults() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["default_bucket"].is_null());
    check!(resp["default_prefix"].is_null());
}

// --- s3s integration tests ---

/// Upload a test object via the S3 API.
async fn put_object(client: &aws_sdk_s3::Client, bucket: &str, key: &str, data: &[u8]) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(data.to_vec().into())
        .send()
        .await
        .unwrap();
}

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Set up a fake S3 environment: returns (upload_client, backend, base_url).
async fn setup_s3_test(
    bucket: &str,
    default_bucket: Option<String>,
    default_prefix: Option<String>,
) -> (aws_sdk_s3::Client, String, tempfile::TempDir) {
    let s3_root = tempfile::tempdir().unwrap();
    std::fs::create_dir(s3_root.path().join(bucket)).unwrap();

    // One client for uploading test data
    let upload_client = fake_s3_client(s3_root.path());
    // Separate client for the backend (s3s-fs doesn't share state across instances,
    // but they share the same filesystem root)
    let backend = S3Backend::from_client(fake_s3_client(s3_root.path()));

    let state = AppState::new(Arc::new(backend), default_bucket, default_prefix);
    let base = start_server(state).await;
    (upload_client, base, s3_root)
}

#[tokio::test]
async fn prefixes_discovers_top_level_prefixes() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", Some("test-bucket".into()), None).await;
    let client = reqwest::Client::new();

    put_object(&s3, "test-bucket", "traces/2026-04-09/file.bin", b"data").await;
    put_object(&s3, "test-bucket", "logs/other.bin", b"data").await;

    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.contains(&"traces/".to_string()));
    check!(resp.contains(&"logs/".to_string()));
}

#[tokio::test]
async fn search_returns_objects_from_s3() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", None, None).await;
    let client = reqwest::Client::new();

    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1910/svc/host/123-0.bin.gz",
        &gzip_bytes(b"trace data 1"),
    )
    .await;
    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1910/svc/host/123-1.bin.gz",
        &gzip_bytes(b"trace data 2"),
    )
    .await;
    put_object(
        &s3,
        "test-bucket",
        "traces/2026-04-09/1920/svc/host/456-0.bin.gz",
        &gzip_bytes(b"other data"),
    )
    .await;

    // Search with prefix matching the 1910 time bucket
    let resp = client
        .get(format!(
            "{base}/api/search?q=traces/2026-04-09/1910/&bucket=test-bucket"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 2);
    check!(body[0].key.contains("1910"));
    check!(body[1].key.contains("1910"));

    // Search with broader prefix returns all 3
    let resp = client
        .get(format!(
            "{base}/api/search?q=traces/2026-04-09/&bucket=test-bucket"
        ))
        .send()
        .await
        .unwrap();
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 3);
}

#[tokio::test]
async fn search_with_default_prefix() {
    let (s3, base, _dir) = setup_s3_test(
        "test-bucket",
        Some("test-bucket".into()),
        Some("my-prefix".into()),
    )
    .await;
    let client = reqwest::Client::new();

    put_object(
        &s3,
        "test-bucket",
        "my-prefix/2026-04-09/1910/svc/host/123-0.bin.gz",
        b"data",
    )
    .await;

    let resp = client
        .get(format!("{base}/api/search?q=2026-04-09/"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 1);
    check!(body[0].key.contains("my-prefix"));
}

#[tokio::test]
async fn trace_fetches_and_concatenates() {
    let (s3, base, _dir) = setup_s3_test("test-bucket", Some("test-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let trace1 = b"TRACE_SEGMENT_1_DATA";
    let trace2 = b"TRACE_SEGMENT_2_DATA";

    put_object(&s3, "test-bucket", "seg1.bin.gz", &gzip_bytes(trace1)).await;
    put_object(&s3, "test-bucket", "seg2.bin.gz", &gzip_bytes(trace2)).await;

    let resp = client
        .get(format!(
            "{base}/api/trace?keys=seg1.bin.gz&keys=seg2.bin.gz"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);

    let body = resp.bytes().await.unwrap();
    let expected: Vec<u8> = [trace1.as_slice(), trace2.as_slice()].concat();
    check!(body.as_ref() == expected.as_slice());
}

#[tokio::test]
async fn trace_single_key() {
    let (s3, base, _dir) = setup_s3_test("trace-bucket", Some("trace-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let data = b"single segment data";
    put_object(&s3, "trace-bucket", "key.bin.gz", &gzip_bytes(data)).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=key.bin.gz"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

#[tokio::test]
async fn trace_handles_uncompressed_data() {
    let (s3, base, _dir) = setup_s3_test("trace-bucket", Some("trace-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let data = b"raw uncompressed trace";
    put_object(&s3, "trace-bucket", "raw.bin", data).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=raw.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

// --- /api/object (raw single-object passthrough) tests ---

/// The defining property of /api/object: it serves a `.bin.gz` object's bytes
/// VERBATIM, still gzipped — it must NOT decompress (that's the browser's job
/// via fetchTraces). Contrast with /api/trace, which gunzips server-side.
#[tokio::test]
async fn object_serves_raw_gzipped_bytes() {
    let (s3, base, _dir) = setup_s3_test("obj-bucket", Some("obj-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let plaintext = b"DECOMPRESSED_TRACE_BODY";
    let gzipped = gzip_bytes(plaintext);
    put_object(&s3, "obj-bucket", "seg.bin.gz", &gzipped).await;

    let resp = client
        .get(format!("{base}/api/object?key=seg.bin.gz"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    // Bytes are the raw gzip stream, NOT the decompressed plaintext.
    check!(body.as_ref() == gzipped.as_slice());
    check!(body.as_ref() != plaintext.as_slice());
    // Sanity: a gzip stream starts with the 0x1f 0x8b magic.
    check!(body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b);
}

/// An uncompressed object is returned verbatim too.
#[tokio::test]
async fn object_serves_uncompressed_bytes() {
    let (s3, base, _dir) = setup_s3_test("obj-bucket", Some("obj-bucket".into()), None).await;
    let client = reqwest::Client::new();

    let data = b"raw uncompressed object";
    put_object(&s3, "obj-bucket", "raw.bin", data).await;

    let resp = client
        .get(format!("{base}/api/object?key=raw.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

/// A large object must stream back byte-for-byte intact. Because the handler
/// now uses `Body::from_stream` instead of buffering, this exercises the
/// multi-chunk path: the s3s fake delivers the body in several `ByteStream`
/// chunks and they must reassemble exactly. Kept to a few MB so it stays cheap.
#[tokio::test]
async fn object_streams_large_object_intact() {
    let (s3, base, _dir) = setup_s3_test("obj-bucket", Some("obj-bucket".into()), None).await;
    let client = reqwest::Client::new();

    // 4MB of non-trivial bytes so a single read can't accidentally satisfy it.
    let big: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    put_object(&s3, "obj-bucket", "big.bin", &big).await;

    let resp = client
        .get(format!("{base}/api/object?key=big.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.len() == big.len());
    check!(body.as_ref() == big.as_slice());
}

#[tokio::test]
async fn object_requires_key() {
    let state = AppState::new(Arc::new(FakeBackend), Some("test-bucket".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/object?key=&bucket=test-bucket"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

/// BYO credentials: /api/object must be served by the header-supplied ephemeral
/// backend (the s3s fake), not the erroring default backend.
#[tokio::test]
async fn byo_credentials_serve_object_from_headers() {
    let (s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    let data = b"BYO_OBJECT_BYTES";
    let gzipped = gzip_bytes(data);
    put_object(&s3, "byo-bucket", "seg.bin.gz", &gzipped).await;

    let resp = client
        .get(format!(
            "{base}/api/object?key=seg.bin.gz&bucket=byo-bucket"
        ))
        .header(H_AKID, "test")
        .header(H_SECRET, "test")
        .header(H_REGION, "us-east-1")
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    // Served raw (still gzipped) by the ephemeral backend.
    check!(body.as_ref() == gzipped.as_slice());
}

/// Full end-to-end smoke test: simulates the browser flow.
/// 1. Upload gzipped trace segments to fake S3
/// 2. Search for them via /api/search
/// 3. Pick keys from the search results
/// 4. Fetch concatenated trace via /api/trace
/// 5. Verify the concatenated output matches the original data
#[tokio::test]
async fn e2e_search_then_view() {
    let (s3, base, _dir) = setup_s3_test("traces-bucket", Some("traces-bucket".into()), None).await;
    let client = reqwest::Client::new();

    // Simulate two trace segments from the same time bucket
    let seg1 = b"SEGMENT_ONE_BINARY_DATA_HERE";
    let seg2 = b"SEGMENT_TWO_BINARY_DATA_HERE";

    put_object(
        &s3,
        "traces-bucket",
        "2026-04-09/1910/checkout-api/us-east-1/host1/1000-0.bin.gz",
        &gzip_bytes(seg1),
    )
    .await;
    put_object(
        &s3,
        "traces-bucket",
        "2026-04-09/1910/checkout-api/us-east-1/host1/1000-1.bin.gz",
        &gzip_bytes(seg2),
    )
    .await;

    // Step 1: Search — like the browser would
    let search_resp: Vec<ObjectInfo> = client
        .get(format!(
            "{base}/api/search?q=2026-04-09/1910/checkout-api/&bucket=traces-bucket"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    check!(search_resp.len() == 2);

    // Step 2: Build the trace URL from search results — like the browser's viewSelected()
    let keys: Vec<&str> = search_resp.iter().map(|o| o.key.as_str()).collect();
    let keys_param: String = keys
        .iter()
        .map(|k| format!("keys={}", urlencoding::encode(k)))
        .collect::<Vec<_>>()
        .join("&");

    let trace_resp = client
        .get(format!(
            "{base}/api/trace?{keys_param}&bucket=traces-bucket",
        ))
        .send()
        .await
        .unwrap();

    check!(trace_resp.status().as_u16() == 200);
    let body = trace_resp.bytes().await.unwrap();

    // The concatenated output should be seg1 + seg2 (order depends on S3 listing)
    check!(body.len() == seg1.len() + seg2.len());
    // Both segments should be present
    let body_slice = body.as_ref();
    let has_seg1 = body_slice.windows(seg1.len()).any(|w| w == seg1.as_slice());
    let has_seg2 = body_slice.windows(seg2.len()).any(|w| w == seg2.as_slice());
    check!(has_seg1);
    check!(has_seg2);
}

/// Regression test: a compressed segment that decompresses to >50MB must be
/// served successfully. Previously, the server truncated at 50MB during
/// decompression (overshooting by up to 8KB) and then rejected the result
/// with HTTP 413 because it exceeded the same 50MB limit.
#[tokio::test]
async fn trace_serves_large_decompressed_segment() {
    let (s3, base, _dir) = setup_s3_test("big-bucket", Some("big-bucket".into()), None).await;
    let client = reqwest::Client::new();

    // 60MB of data — compresses well, decompresses to >50MB
    let big_data = vec![0xABu8; 60 * 1024 * 1024];
    put_object(&s3, "big-bucket", "big.bin.gz", &gzip_bytes(&big_data)).await;

    let resp = client
        .get(format!("{base}/api/trace?keys=big.bin.gz"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.len() == big_data.len());
}

// --- bring-your-own-credentials tests ---

const H_AKID: &str = "x-dial9-aws-access-key-id";
const H_SECRET: &str = "x-dial9-aws-secret-access-key";
const H_REGION: &str = "x-dial9-aws-region";

/// Set up a BYO server: the default backend always errors, so any successful
/// data response must have been served by the header-supplied ephemeral
/// backend pointed at the s3s fake.
async fn setup_byo_test(bucket: &str) -> (aws_sdk_s3::Client, String, tempfile::TempDir) {
    let s3_root = tempfile::tempdir().unwrap();
    std::fs::create_dir(s3_root.path().join(bucket)).unwrap();

    let upload_client = fake_s3_client(s3_root.path());

    let state = AppState::new(Arc::new(ErroringBackend), Some(bucket.to_string()), None)
        .with_byo_creds(true)
        .with_ephemeral_s3(fake_ephemeral_config(s3_root.path()));
    let base = start_server(state).await;
    (upload_client, base, s3_root)
}

#[tokio::test]
async fn byo_credentials_serve_search_from_headers() {
    let (s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    put_object(
        &s3,
        "byo-bucket",
        "traces/2026-04-09/seg.bin.gz",
        &gzip_bytes(b"x"),
    )
    .await;

    // With credentials → served by the ephemeral backend (the s3s fake).
    let resp = client
        .get(format!("{base}/api/search?q=traces/&bucket=byo-bucket"))
        .header(H_AKID, "test")
        .header(H_SECRET, "test")
        .header(H_REGION, "us-east-1")
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: Vec<ObjectInfo> = resp.json().await.unwrap();
    check!(body.len() == 1);
    check!(body[0].key == "traces/2026-04-09/seg.bin.gz");
}

#[tokio::test]
async fn byo_credentials_serve_trace_from_headers() {
    let (s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    let data = b"TRACE_BYTES";
    put_object(&s3, "byo-bucket", "seg.bin.gz", &gzip_bytes(data)).await;

    let resp = client
        .get(format!(
            "{base}/api/trace?keys=seg.bin.gz&bucket=byo-bucket"
        ))
        .header(H_AKID, "test")
        .header(H_SECRET, "test")
        .header(H_REGION, "us-east-1")
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    check!(body.as_ref() == data);
}

#[tokio::test]
async fn byo_credentials_list_buckets_from_headers() {
    // The s3s fake exposes buckets that exist as directories under its root.
    let (s3, base, dir) = setup_byo_test("byo-bucket").await;
    // Create a second bucket so the listing returns more than one.
    std::fs::create_dir(dir.path().join("dial9-traces")).unwrap();
    // Touch the fake so the dir is recognized as a bucket (PutObject creates it
    // lazily otherwise); upload into both.
    put_object(&s3, "byo-bucket", "x", b"x").await;

    let resp = client_list_buckets(&base).await;
    check!(resp.status().as_u16() == 200);
    let names: Vec<String> = resp.json().await.unwrap();
    check!(names.contains(&"byo-bucket".to_string()));
    check!(names.contains(&"dial9-traces".to_string()));
}

#[tokio::test]
async fn credentials_check_succeeds_for_existing_bucket() {
    // HeadBucket against the s3s fake succeeds for a bucket that exists, so the
    // check endpoint reports ok:true. (The wrong-credentials → ok:false path is
    // validated live against real S3; s3s does not enforce sigv4 the same way.)
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/credentials/check?bucket=byo-bucket"))
        .header(H_AKID, "test")
        .header(H_SECRET, "test")
        .header(H_REGION, "us-east-1")
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    check!(body["ok"] == true);
}

#[tokio::test]
async fn credentials_check_requires_credentials() {
    // No headers → the check endpoint reports the missing-credentials 400.
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/credentials/check?bucket=byo-bucket"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn list_buckets_without_credentials_uses_default_backend() {
    // No headers → the (erroring) default backend, not the ephemeral path.
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/api/buckets"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 500);
}

/// GET /api/buckets with the s3s test credentials in headers.
async fn client_list_buckets(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}/api/buckets"))
        .header(H_AKID, "test")
        .header(H_SECRET, "test")
        .header(H_REGION, "us-east-1")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn without_credentials_falls_back_to_default_backend() {
    // No headers → the (erroring) default backend is used. Proves credentials
    // are genuinely optional AND that the default path is still taken.
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/search?q=traces/&bucket=byo-bucket"))
        .send()
        .await
        .unwrap();
    // ErroringBackend → 500, not a 200 from the ephemeral path.
    check!(resp.status().as_u16() == 500);
}

#[tokio::test]
async fn incomplete_credentials_rejected_with_400() {
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    // Access key id without a secret → 400, never a silent fallback.
    let resp = client
        .get(format!("{base}/api/search?q=traces/&bucket=byo-bucket"))
        .header(H_AKID, "test")
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 400);
}

#[tokio::test]
async fn config_reports_credential_support() {
    let (_s3, base, _dir) = setup_byo_test("byo-bucket").await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["supports_byo_credentials"] == true);
}

#[tokio::test]
async fn config_reports_no_credential_support_by_default() {
    // A plain (non-BYO) server should not advertise credential support.
    let state = AppState::new(Arc::new(FakeBackend), Some("b".into()), None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp["supports_byo_credentials"] == false);
}

// --- local backend tests ---

fn setup_local_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // Create a directory structure mimicking trace output:
    //   2026-04-09/1910/svc/host/123-0.bin.gz
    //   2026-04-09/1910/svc/host/123-1.bin.gz
    //   2026-04-09/1920/svc/host/456-0.bin.gz
    let base = dir.path().join("2026-04-09/1910/svc/host");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("123-0.bin.gz"), gzip_bytes(b"trace seg 0")).unwrap();
    std::fs::write(base.join("123-1.bin.gz"), gzip_bytes(b"trace seg 1")).unwrap();

    let base2 = dir.path().join("2026-04-09/1920/svc/host");
    std::fs::create_dir_all(&base2).unwrap();
    std::fs::write(base2.join("456-0.bin.gz"), gzip_bytes(b"other trace")).unwrap();
    dir
}

fn local_state(dir: &std::path::Path) -> AppState {
    AppState::new(Arc::new(LocalBackend::new(dir)), Some("local".into()), None)
}

#[tokio::test]
async fn local_search_lists_all_files() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 3);
}

#[tokio::test]
async fn local_search_filters_by_prefix() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search?q=2026-04-09/1910/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 2);
    for obj in &resp {
        check!(obj.key.contains("1910"));
    }
}

#[tokio::test]
async fn local_trace_fetches_and_decompresses() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{base}/api/trace?keys=2026-04-09/1910/svc/host/123-0.bin.gz&keys=2026-04-09/1910/svc/host/123-1.bin.gz"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    // Both segments decompressed and concatenated
    let body_slice = body.as_ref();
    let has_seg0 = body_slice
        .windows(b"trace seg 0".len())
        .any(|w| w == b"trace seg 0");
    let has_seg1 = body_slice
        .windows(b"trace seg 1".len())
        .any(|w| w == b"trace seg 1");
    check!(has_seg0);
    check!(has_seg1);
}

#[tokio::test]
async fn local_trace_not_found() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/trace?keys=nonexistent.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 404);
}

/// /api/object on the local backend serves the file's raw (gzipped) bytes
/// without decompressing.
#[tokio::test]
async fn local_object_serves_raw_bytes() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{base}/api/object?key=2026-04-09/1910/svc/host/123-0.bin.gz"
        ))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.bytes().await.unwrap();
    // The on-disk file is gzip(b"trace seg 0"); served verbatim.
    check!(body.as_ref() == gzip_bytes(b"trace seg 0").as_slice());
}

#[tokio::test]
async fn local_object_not_found() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/object?key=nonexistent.bin"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 404);
}

#[tokio::test]
async fn local_object_path_traversal_rejected() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/object?key=../../../etc/passwd"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() != 200);
}

#[tokio::test]
async fn local_prefixes_lists_subdirs() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Top-level prefixes
    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp == vec!["2026-04-09/"]);

    // Nested prefixes
    let resp: Vec<String> = client
        .get(format!("{base}/api/prefixes?prefix=2026-04-09/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.contains(&"2026-04-09/1910/".to_string()));
    check!(resp.contains(&"2026-04-09/1920/".to_string()));
}

#[tokio::test]
async fn local_e2e_search_then_view() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Search for segments in the 1910 time bucket
    let search_resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search?q=2026-04-09/1910/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(search_resp.len() == 2);

    // Build trace URL from search results
    let keys_param: String = search_resp
        .iter()
        .map(|o| format!("keys={}", urlencoding::encode(&o.key)))
        .collect::<Vec<_>>()
        .join("&");

    let trace_resp = client
        .get(format!("{base}/api/trace?{keys_param}"))
        .send()
        .await
        .unwrap();
    check!(trace_resp.status().as_u16() == 200);

    let body = trace_resp.bytes().await.unwrap();
    // Both segments present (decompressed)
    let body_slice = body.as_ref();
    let has_seg0 = body_slice
        .windows(b"trace seg 0".len())
        .any(|w| w == b"trace seg 0");
    let has_seg1 = body_slice
        .windows(b"trace seg 1".len())
        .any(|w| w == b"trace seg 1");
    check!(has_seg0);
    check!(has_seg1);
}

#[tokio::test]
async fn local_search_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.is_empty());
}

#[tokio::test]
async fn local_search_returns_file_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"hello world";
    std::fs::write(dir.path().join("test.bin"), data).unwrap();

    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    let resp: Vec<ObjectInfo> = client
        .get(format!("{base}/api/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    check!(resp.len() == 1);
    check!(resp[0].key == "test.bin");
    check!(resp[0].size == data.len() as i64);
}

#[tokio::test]
async fn local_path_traversal_rejected() {
    let dir = setup_local_dir();
    let base = start_server(local_state(dir.path())).await;
    let client = reqwest::Client::new();

    // Attempt to escape root via ../
    let resp = client
        .get(format!("{base}/api/trace?keys=../../../etc/passwd"))
        .send()
        .await
        .unwrap();
    // Should fail — either not found or error, but not 200
    check!(resp.status().as_u16() != 200);
}

// --- trace upload tests ---

/// A minimal but valid trace prefix: the `TRC\0` magic the upload validator
/// looks for. The bytes after it don't matter for the upload path (the viewer
/// decodes client-side), so we just need recognizable content to round-trip.
const TRACE_MAGIC_BYTES: &[u8] = b"TRC\0sample-trace-bytes";

/// An `AppState` with the (opt-in) upload feature enabled at default caps.
fn upload_state() -> AppState {
    AppState::new(Arc::new(FakeBackend), None, None).with_uploads(UploadLimits::default())
}

/// Uploads are opt-in: without `.with_uploads(...)` (i.e. no `--enable-upload`),
/// the upload routes are not registered and both endpoints 404.
#[tokio::test]
async fn upload_disabled_by_default() {
    let state = AppState::new(Arc::new(FakeBackend), None, None);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let post = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(post.status().as_u16() == 404);

    let get = client
        .get(format!("{base}/api/uploaded/anything"))
        .send()
        .await
        .unwrap();
    check!(get.status().as_u16() == 404);
}

/// POST a trace, then GET it back via the returned `trace_url`. The bytes must
/// survive verbatim.
#[tokio::test]
async fn upload_round_trips() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let trace_url = body["trace_url"].as_str().unwrap();
    check!(body["id"].as_str().is_some());
    check!(trace_url.starts_with("/api/uploaded/"));
    // viewer_url points the viewer at the trace via the existing ?trace= param.
    check!(
        body["viewer_url"]
            .as_str()
            .unwrap()
            .starts_with("/viewer.html?trace=%2Fapi%2Fuploaded%2F")
    );

    let trace_resp = client
        .get(format!("{base}{trace_url}"))
        .send()
        .await
        .unwrap();
    check!(trace_resp.status().as_u16() == 200);
    let fetched = trace_resp.bytes().await.unwrap();
    check!(fetched.as_ref() == TRACE_MAGIC_BYTES);
}

/// The second GET of an uploaded trace 404s — uploads are single-use.
#[tokio::test]
async fn upload_is_single_use() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let trace_url = body["trace_url"].as_str().unwrap().to_string();

    let first = client
        .get(format!("{base}{trace_url}"))
        .send()
        .await
        .unwrap();
    check!(first.status().as_u16() == 200);

    let second = client
        .get(format!("{base}{trace_url}"))
        .send()
        .await
        .unwrap();
    check!(second.status().as_u16() == 404);
}

/// GET of an id that was never uploaded 404s.
#[tokio::test]
async fn uploaded_unknown_id_not_found() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/api/uploaded/does-not-exist"))
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 404);
}

/// Empty bodies and bytes lacking the gzip/`TRC\0` magic are rejected with 400.
#[tokio::test]
async fn upload_rejects_invalid_bodies() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let empty = client
        .post(format!("{base}/api/upload"))
        .body(Vec::<u8>::new())
        .send()
        .await
        .unwrap();
    check!(empty.status().as_u16() == 400);

    let junk = client
        .post(format!("{base}/api/upload"))
        .body(b"not a trace at all".to_vec())
        .send()
        .await
        .unwrap();
    check!(junk.status().as_u16() == 400);
}

/// A gzipped body is accepted (matches how traces are stored on the wire).
#[tokio::test]
async fn upload_accepts_gzipped_body() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let gz = gzip_bytes(b"TRC\0whatever");
    let resp = client
        .post(format!("{base}/api/upload"))
        .body(gz.clone())
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);

    // Stored verbatim: the bytes come back gzipped, the viewer gunzips client-side.
    let body: serde_json::Value = resp.json().await.unwrap();
    let trace_url = body["trace_url"].as_str().unwrap();
    let fetched = client
        .get(format!("{base}{trace_url}"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    check!(fetched.as_ref() == gz.as_slice());
}

/// A body larger than the per-upload limit is rejected by the body-limit layer
/// (413 Payload Too Large).
#[tokio::test]
async fn upload_rejects_oversized_body() {
    let limits = UploadLimits::builder().max_upload_bytes(64).build();
    let state = AppState::new(Arc::new(FakeBackend), None, None).with_uploads(limits);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let mut big = b"TRC\0".to_vec();
    big.resize(256, 0);
    let resp = client
        .post(format!("{base}/api/upload"))
        .body(big)
        .send()
        .await
        .unwrap();
    check!(resp.status().as_u16() == 413);
}

/// Once the count cap is reached, further uploads are rejected with 507.
#[tokio::test]
async fn upload_rejects_when_full() {
    let limits = UploadLimits::builder().max_uploads(1).build();
    let state = AppState::new(Arc::new(FakeBackend), None, None).with_uploads(limits);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(first.status().as_u16() == 200);

    let second = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(second.status().as_u16() == 507);
}

/// Once the total-bytes cap is reached, further uploads are rejected with 507
/// (exercises the byte cap over HTTP, distinct from the count cap above).
#[tokio::test]
async fn upload_rejects_when_total_bytes_full() {
    // Big enough per-upload limit to accept one body, but a tiny total budget
    // so the second body tips it over.
    let limits = UploadLimits::builder()
        .max_total_bytes(TRACE_MAGIC_BYTES.len())
        .build();
    let state = AppState::new(Arc::new(FakeBackend), None, None).with_uploads(limits);
    let base = start_server(state).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(first.status().as_u16() == 200);

    // Without fetching the first (which would free its bytes), the second 507s.
    let second = client
        .post(format!("{base}/api/upload"))
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(second.status().as_u16() == 507);
}

/// A cross-origin preflight (OPTIONS) is answered with permissive CORS headers,
/// and the actual POST response carries `access-control-allow-origin`.
#[tokio::test]
async fn upload_supports_cors() {
    let base = start_server(upload_state()).await;
    let client = reqwest::Client::new();

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/api/upload"))
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    check!(preflight.status().is_success());
    check!(
        preflight
            .headers()
            .contains_key("access-control-allow-origin")
    );

    let posted = client
        .post(format!("{base}/api/upload"))
        .header("Origin", "https://example.com")
        .body(TRACE_MAGIC_BYTES.to_vec())
        .send()
        .await
        .unwrap();
    check!(posted.status().as_u16() == 200);
    check!(posted.headers().contains_key("access-control-allow-origin"));
}

#[cfg(test)]
mod skills_unpack_tests {
    use std::path::Path;
    use std::process::Command;

    fn validate_skill_frontmatter(skill_dir: &Path, content: &str) {
        assert!(
            content.starts_with("---\n"),
            "SKILL.md in {skill_dir:?} missing frontmatter delimiter"
        );

        // Validate name field matches directory name
        let dir_name = skill_dir.file_name().unwrap().to_string_lossy().to_string();
        let name_line = content
            .lines()
            .find(|l| l.starts_with("name:"))
            .unwrap_or_else(|| panic!("SKILL.md in {skill_dir:?} missing name field"));
        let name = name_line.strip_prefix("name:").unwrap().trim();
        assert_eq!(
            name, dir_name,
            "name field {:?} doesn't match directory {:?}",
            name, dir_name
        );

        // Validate name format: lowercase + hyphens, no leading/trailing/consecutive hyphens
        assert!(
            !name.is_empty() && name.len() <= 64,
            "name {:?} invalid length",
            name
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name {:?} contains invalid characters",
            name
        );
        assert!(
            !name.starts_with('-') && !name.ends_with('-'),
            "name {:?} has leading/trailing hyphen",
            name
        );
        assert!(
            !name.contains("--"),
            "name {:?} has consecutive hyphens",
            name
        );

        // Validate description field exists and is a non-empty scalar string.
        let desc_line = content
            .lines()
            .find(|l| l.starts_with("description:"))
            .unwrap_or_else(|| panic!("SKILL.md in {skill_dir:?} missing description field"));
        let desc = desc_line.strip_prefix("description:").unwrap().trim();
        assert!(!desc.is_empty(), "empty description in {skill_dir:?}");
        assert!(
            !(desc.starts_with('[') || desc.starts_with('{')),
            "description must be a scalar string in {skill_dir:?}: {desc:?}"
        );
        assert!(
            desc.len() <= 1024,
            "description too long ({}) in {:?}",
            desc.len(),
            skill_dir
        );
    }

    /// Validates source skill definitions in `dial9-viewer/skills/` have valid frontmatter.
    #[test]
    fn source_skills_have_valid_frontmatter() {
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let mut skill_count = 0;

        for entry in std::fs::read_dir(&skills_dir).unwrap() {
            let entry = entry.unwrap();
            if !entry.path().is_dir() {
                continue;
            }
            skill_count += 1;
            let skill_md = entry.path().join("SKILL.md");
            assert!(skill_md.exists(), "Missing SKILL.md in {:?}", entry.path());

            let content = std::fs::read_to_string(&skill_md).unwrap();
            validate_skill_frontmatter(&entry.path(), &content);
        }

        assert!(
            skill_count >= 6,
            "expected at least 6 source skills, got {skill_count}"
        );
    }

    /// Validates that `agents skills <dir>` produces a valid Agent Skills directory.
    /// Each skill must have a SKILL.md with valid frontmatter (name + description).
    #[test]
    fn unpack_produces_valid_agent_skills_layout() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "skills", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "agents skills failed: {:?}",
            output
        );

        // Each subdirectory must have a SKILL.md with valid frontmatter
        let mut skill_count = 0;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let entry = entry.unwrap();
            if !entry.path().is_dir() {
                continue;
            }
            skill_count += 1;
            let skill_md = entry.path().join("SKILL.md");
            assert!(skill_md.exists(), "Missing SKILL.md in {:?}", entry.path());

            let content = std::fs::read_to_string(&skill_md).unwrap();
            validate_skill_frontmatter(&entry.path(), &content);
        }
        assert!(
            skill_count >= 6,
            "expected at least 6 skills, got {skill_count}"
        );
    }

    /// Validates that `agents toolkit <dir>` produces the expected JS files.
    #[test]
    fn toolkit_produces_expected_files() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "toolkit", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "agents toolkit failed: {:?}",
            output
        );

        let expected = [
            "analyze.js",
            "decode.js",
            "trace_parser.js",
            "trace_analysis.js",
        ];
        for name in &expected {
            let path = dir.path().join(name);
            assert!(path.exists(), "missing toolkit file: {name}");
            assert!(
                std::fs::metadata(&path).unwrap().len() > 0,
                "empty toolkit file: {name}"
            );
        }
    }

    /// Validates that scripts in unpacked skills can find their dependencies.
    /// The red_flag_scan.js should be able to resolve trace_parser.js from the toolkit skill.
    #[test]
    fn unpacked_scripts_resolve_dependencies() {
        let bin = env!("CARGO_BIN_EXE_dial9-viewer");
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(bin)
            .args(["agents", "skills", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());

        // The toolkit skill must have all 4 scripts
        let toolkit_scripts = dir.path().join("dial9-toolkit").join("scripts");
        assert!(toolkit_scripts.join("analyze.js").exists());
        assert!(toolkit_scripts.join("decode.js").exists());
        assert!(toolkit_scripts.join("trace_parser.js").exists());
        assert!(toolkit_scripts.join("trace_analysis.js").exists());

        // The red-flags skill must have its script
        let red_flags_script = dir
            .path()
            .join("dial9-red-flags")
            .join("scripts")
            .join("red_flag_scan.js");
        assert!(red_flags_script.exists());
    }
}
