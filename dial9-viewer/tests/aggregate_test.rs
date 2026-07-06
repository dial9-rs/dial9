//! End-to-end integration tests for the demand-driven aggregation refinement
//! loop, driven through the real HTTP `/api/flamegraph` SSE endpoint against a
//! simulated S3 (s3s). This is the Goal-1 "works against fake S3 so we can test
//! the whole flow" coverage: folding, ordering, coverage reporting, the
//! sampling cap, idempotency, zero-sample files, and scope filtering.
//!
//! The endpoint streams: one request folds to the sampling cap and emits a
//! Server-Sent Event per file (the first is the already-folded snapshot, the
//! last is the fully-refined snapshot). [`stream`] collects every event;
//! [`stream_final`] returns just the last (at-cap) one — the natural replacement
//! for the old "poll until coverage stops climbing" loops.

use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use dial9_viewer::ingest::aggregate::AggContext;
use dial9_viewer::server::{AppState, router};
use dial9_viewer::storage::S3Backend;
use std::sync::Arc;

/// Build an s3s-backed client over a filesystem root (the simulated S3).
fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
    let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
    let mut builder = s3s::service::S3ServiceBuilder::new(fs);
    builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    let s3_service = builder.build();
    let s3_client: s3s_aws::Client = s3_service.into();
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(s3_client)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(s3_config)
}

/// Known per-filter sample counts embedded in [`mini_trace`], returned alongside
/// the bytes so the test expectations can never drift from the trace the tests
/// actually fold.
#[derive(Clone, Copy)]
struct MiniCounts {
    /// Every sample (CpuProfile + SchedEvent) — the `source=all` view.
    total: usize,
    /// CpuProfile samples (source 0) — the default / `source=cpu` view.
    cpu: usize,
    /// SchedEvent samples (source 1) — the `source=sched` view.
    sched: usize,
    /// On-runtime CpuProfile samples — the `thread_class=worker` view.
    cpu_on: usize,
    /// Off-runtime CpuProfile samples — the `thread_class=off-worker` view.
    cpu_off: usize,
}

/// Build a tiny, deterministic trace segment used as the body of every synthetic
/// source segment, and return its gzipped bytes together with the exact sample
/// counts it contains.
///
/// We synthesize the trace in-process (via the trace-format encoder) rather than
/// folding the committed `demo-trace.bin`, for two reasons:
///
/// 1. **No drift.** `demo-trace.bin` is owned and periodically regenerated on
///    `main` (and its JS property fixture is only refreshed on a perf-capable
///    host — see `scripts/regenerate_demo_trace.sh`). A PR that merges `main`
///    therefore pairs `main`'s freshly regenerated trace with this branch's
///    pinned expectations, breaking these exact-count assertions for reasons
///    unrelated to the change. A code-defined trace can't drift.
/// 2. **Speed.** Each fold gunzip+decode+parquet-encodes the whole body; a ~9
///    sample trace folds in microseconds where the 3 MB demo trace took ~tens of
///    seconds across the volume tests.
///
/// The trace deliberately exercises every facet the tests assert: both sources
/// (CpuProfile=0, SchedEvent=1), both thread classes (on-worker via tids bound to
/// a worker by park events, plus one off-worker CpuProfile sample on an unbound
/// tid), and multi-frame callchains so the flamegraph tree is non-trivial.
fn mini_trace() -> (Vec<u8>, MiniCounts) {
    let mut enc = Encoder::new();
    // Only the fields `decode_samples` reads are declared; the decoder matches
    // fields by name and ignores the rest of the producer's wider schema.
    let park = enc
        .register_schema(
            "WorkerParkEvent",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("tid", FieldType::Varint),
            ],
        )
        .unwrap();
    let cpu = enc
        .register_schema(
            "CpuSampleEvent",
            vec![
                FieldDef::new("tid", FieldType::Varint),
                FieldDef::new("source", FieldType::Varint),
                FieldDef::new("callchain", FieldType::StackFrames),
            ],
        )
        .unwrap();

    // Bind tid 100 -> worker 0 and tid 101 -> worker 1. Samples on these tids are
    // "on-runtime"; samples on any other tid are "off-worker".
    for (ts, worker, tid) in [(10u64, 0u64, 100u64), (11, 1, 101)] {
        enc.write_event(
            &park,
            &[
                FieldValue::Varint(ts),
                FieldValue::Varint(worker),
                FieldValue::Varint(tid),
            ],
        )
        .unwrap();
    }

    // (timestamp, tid, source, callchain). Sources: 0 = CpuProfile, 1 = SchedEvent.
    let events: &[(u64, u64, u64, &[u64])] = &[
        // 5 on-worker CpuProfile samples.
        (100, 100, 0, &[0x1000, 0x2000, 0x3000]),
        (101, 100, 0, &[0x1000, 0x2000, 0x4000]),
        (102, 101, 0, &[0x1000, 0x5000]),
        (103, 101, 0, &[0x1000, 0x5000, 0x6000]),
        (104, 100, 0, &[0x7000, 0x8000]),
        // 1 off-worker CpuProfile sample (tid 999 is never bound to a worker).
        (105, 999, 0, &[0x1000, 0x2000, 0x3000]),
        // 3 on-worker SchedEvent samples.
        (106, 100, 1, &[0x1000, 0x2000]),
        (107, 101, 1, &[0x1000, 0x5000]),
        (108, 100, 1, &[0x9000, 0xa000]),
    ];
    for &(ts, tid, source, frames) in events {
        enc.write_event(
            &cpu,
            &[
                FieldValue::Varint(ts),
                FieldValue::Varint(tid),
                FieldValue::Varint(source),
                FieldValue::StackFrames(frames.to_vec().into()),
            ],
        )
        .unwrap();
    }

    let raw = enc.finish();
    // Gzip it: source segments are `.bin.gz`, and the fold path gunzips before
    // decoding (`maybe_gunzip`), so this exercises the real code path.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &raw).unwrap();
    let bytes = gz.finish().unwrap();

    let counts = MiniCounts {
        total: 9,
        cpu: 6,
        sched: 3,
        cpu_on: 5,
        cpu_off: 1,
    };
    (bytes, counts)
}

/// Gzipped bytes of the synthetic trace segment (the body of every seeded file).
fn mini_trace_gz() -> Vec<u8> {
    mini_trace().0
}

/// Expected per-filter sample counts for [`mini_trace_gz`].
fn mini_counts() -> MiniCounts {
    mini_trace().1
}

async fn put(client: &aws_sdk_s3::Client, bucket: &str, key: &str, data: Vec<u8>) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(data.into())
        .send()
        .await
        .unwrap();
}

/// A realistic source key: `{date}/{HHMM}/{service}/{host}/{boot}/{ts}-{i}.bin.gz`.
/// `epoch` is the file start time in seconds (drives the scope time filter).
fn segment_key(date: &str, hhmm: &str, svc: &str, host: &str, epoch: i64, idx: u32) -> String {
    format!("{date}/{hhmm}/{svc}/{host}/boot-1/{epoch}-{idx}.bin.gz")
}

/// Start the server with demand-driven aggregation over SEPARATE source and
/// output buckets in the same simulated S3 filesystem.
async fn start_agg_server(
    fs_root: &std::path::Path,
    source_bucket: &str,
    output_bucket: &str,
    segment_secs: i64,
) -> String {
    let output = Arc::new(S3Backend::from_client(fake_s3_client(fs_root)));
    start_agg_server_with_output(fs_root, source_bucket, output_bucket, segment_secs, output).await
}

/// Like [`start_agg_server`], but the aggregation OUTPUT backend is injected — so
/// a test can point it at a client that fails writes (see
/// [`access_denied_on_put_client`]) to exercise the fold-error reporting path.
async fn start_agg_server_with_output(
    fs_root: &std::path::Path,
    source_bucket: &str,
    output_bucket: &str,
    segment_secs: i64,
    output: Arc<dyn dial9_viewer::storage::StorageBackend>,
) -> String {
    let source = Arc::new(S3Backend::from_client(fake_s3_client(fs_root)));
    let agg = AggContext {
        source,
        output,
        source_bucket: source_bucket.to_string(),
        source_is_local: false,
        output_bucket: output_bucket.to_string(),
        output_prefix: "flamegraph-data".to_string(),
        source_prefixes: vec![String::new()],
        segment_duration_secs: segment_secs,
    };
    let browse_backend = Arc::new(S3Backend::from_client(fake_s3_client(fs_root)));
    let state = AppState::new(browse_backend, Some(source_bucket.into()), None).with_agg(agg);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// An `aws_sdk_s3::Client` whose `PutObject` always fails with 403 AccessDenied
/// (via an injected HTTP client), while `GET`/list return an empty
/// `ListBucketResult`. This is the fold-failure analogue of a real read-only
/// output bucket: reads/lists work, writes are denied — so every part-file write
/// errors and no file ever folds. Used to drive the fold-error reporting path.
fn access_denied_on_put_client() -> aws_sdk_s3::Client {
    use aws_smithy_http_client::test_util::infallible_client_fn;
    let http_client = infallible_client_fn(|req: http::Request<_>| {
        if req.method() == http::Method::PUT {
            let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                <Error><Code>AccessDenied</Code>\
                <Message>Access Denied</Message></Error>";
            http::Response::builder()
                .status(403)
                .header("content-type", "application/xml")
                .body(body)
                .unwrap()
        } else {
            // Reads / LISTs succeed but return nothing folded.
            let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                <KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>";
            http::Response::builder()
                .status(200)
                .header("content-type", "application/xml")
                .body(body)
                .unwrap()
        }
    });
    let cfg = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "test", "test", None, None, "test",
        ))
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .http_client(http_client)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(cfg)
}

/// A [`StorageBackend`] wrapper whose `put_object` fails for keys containing
/// `fail_substr`, delegating everything else to the inner backend. Simulates a
/// fold whose part-file writes PARTIALLY succeed — the interleaving a cancelled
/// or partially-denied fold produces — to pin down the commit-ordering
/// invariant: the `samples/` part (the durable "folded" record) must never land
/// unless the dict and polls parts landed first.
///
/// [`StorageBackend`]: dial9_viewer::storage::StorageBackend
struct FailingPuts {
    inner: Arc<dyn dial9_viewer::storage::StorageBackend>,
    fail_substr: &'static str,
}

impl dial9_viewer::storage::StorageBackend for FailingPuts {
    fn list_buckets(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<String>, dial9_viewer::storage::StorageError>,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_buckets()
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        dial9_viewer::storage::ListPage,
                        dial9_viewer::storage::StorageError,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_objects(bucket, prefix, cap)
    }

    fn list_objects_all(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<dial9_viewer::storage::ObjectInfo>,
                        dial9_viewer::storage::StorageError,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_objects_all(bucket, prefix)
    }

    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<String>, dial9_viewer::storage::StorageError>,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_prefixes(bucket, prefix)
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<u8>, dial9_viewer::storage::StorageError>>
                + Send
                + '_,
        >,
    > {
        self.inner.get_object(bucket, key)
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), dial9_viewer::storage::StorageError>>
                + Send
                + '_,
        >,
    > {
        if key.contains(self.fail_substr) {
            let key = key.to_string();
            return Box::pin(async move {
                Err(dial9_viewer::storage::StorageError::Other(format!(
                    "simulated write failure for {key}"
                )))
            });
        }
        self.inner.put_object(bucket, key, data)
    }
}

/// Start a server WITHOUT a server-side `AggContext`, exercising the
/// bring-your-own-credentials `/api/flamegraph?bucket=…` path instead. The
/// output bucket is configured separately (as `--agg-output-bucket` does), so
/// aggregated part-files are written there rather than back into the (possibly
/// read-only) source bucket. Returns the base URL.
async fn start_byoc_server(
    fs_root: &std::path::Path,
    source_bucket: &str,
    output_bucket: Option<&str>,
    segment_secs: i64,
) -> String {
    // The request backend (no BYOC headers in the test → server's ambient
    // identity, which here is the fake S3 client). This both lists the source
    // and, when no output override is set, writes the output.
    let request_backend = Arc::new(S3Backend::from_client(fake_s3_client(fs_root)));
    let mut state = AppState::new(request_backend, Some(source_bucket.into()), None)
        .with_byo_creds(true)
        .with_agg_segment_secs(segment_secs);
    if let Some(out) = output_bucket {
        let out_backend: Arc<dyn dial9_viewer::storage::StorageBackend> =
            Arc::new(S3Backend::from_client(fake_s3_client(fs_root)));
        state = state.with_agg_output_bucket(Some(out.to_string()), Some(out_backend));
    }
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A parsed SSE event. The flamegraph `tree` is deeply nested (real call
/// stacks) and would blow serde_json's default recursion limit, so we do NOT
/// deserialize it into a recursive struct — we capture only the scalar fields
/// and a shallow "is the tree non-trivial" flag derived from the raw JSON, plus
/// the raw JSON body for tests that need to inspect metadata/facets verbatim.
struct Resp {
    total_samples: usize,
    coverage: Option<Coverage>,
    tree_has_children: bool,
    body: String,
}

#[derive(serde::Deserialize)]
struct Coverage {
    files_matched: usize,
    files_folded: usize,
    samples_folded: usize,
    hosts_matched: usize,
    hosts_folded: usize,
    /// Files whose fold failed this stream (0 unless e.g. the output bucket is
    /// unwritable). Defaults to 0 for the scalar extractor when absent.
    fold_errors: usize,
}

/// Parse the `data:` payloads out of a `text/event-stream` body. Each event is
/// separated by a blank line; we join a frame's `data:` lines and skip comment
/// (keep-alive) frames. The endpoints emit one JSON object per event.
fn parse_sse_events(body: &str) -> Vec<String> {
    let body = body.replace("\r\n", "\n");
    let mut events = Vec::new();
    for frame in body.split("\n\n") {
        let mut data = String::new();
        for line in frame.split('\n') {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if !data.is_empty() {
            events.push(data);
        }
    }
    events
}

/// Open the flamegraph SSE stream for `query` and collect every event. The
/// server folds to the sampling cap then closes, so `reqwest`'s buffered
/// `.text()` returns the whole finite stream. The first event is the
/// already-folded snapshot; the last is the fully-refined (at-cap) snapshot.
async fn stream(client: &reqwest::Client, base: &str, query: &str) -> Vec<Resp> {
    let url = format!("{base}/api/flamegraph?{query}");
    let r = client.get(&url).send().await.unwrap();
    assert!(
        r.status().is_success(),
        "request {url} failed: {} {}",
        r.status(),
        r.text().await.unwrap_or_default()
    );
    let body = r.text().await.unwrap();
    let events = parse_sse_events(&body);
    assert!(
        !events.is_empty(),
        "stream for {url} produced no events; body = {body:?}"
    );
    events
        .into_iter()
        .map(|body| {
            let total_samples =
                extract_usize(&body, "\"total_samples\":").expect("total_samples present");
            let coverage = extract_coverage(&body);
            let tree_has_children = body.contains("\"children\":[{");
            Resp {
                total_samples,
                coverage,
                tree_has_children,
                body,
            }
        })
        .collect()
}

/// The final (fully-refined) event of a flamegraph stream — the natural
/// replacement for the old "poll to the cap" loop.
async fn stream_final(client: &reqwest::Client, base: &str, query: &str) -> Resp {
    stream(client, base, query).await.pop().unwrap()
}

/// Find `key` in `json` and parse the unsigned integer immediately following it.
fn extract_usize(json: &str, key: &str) -> Option<usize> {
    let i = json.find(key)? + key.len();
    let rest = &json[i..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn extract_coverage(json: &str) -> Option<Coverage> {
    let i = json.find("\"coverage\":")?;
    let rest = &json[i..];
    Some(Coverage {
        files_matched: extract_usize(rest, "\"files_matched\":")?,
        files_folded: extract_usize(rest, "\"files_folded\":")?,
        samples_folded: extract_usize(rest, "\"samples_folded\":")?,
        hosts_matched: extract_usize(rest, "\"hosts_matched\":")?,
        hosts_folded: extract_usize(rest, "\"hosts_folded\":")?,
        // `fold_errors` is 0 unless folds failed; absent in that common case
        // because we only need it in the read-only-output regression test.
        fold_errors: extract_usize(rest, "\"fold_errors\":").unwrap_or(0),
    })
}

/// Seed N source segments spread across several hosts and minutes, all under one
/// service/date. Returns the number of segments written.
async fn seed_fleet(client: &aws_sdk_s3::Client, bucket: &str, body: &[u8]) -> usize {
    let hosts = ["host-a", "host-b", "host-c", "host-d"];
    let base_epoch = 1_744_224_000i64; // 2026-04-09T... (matches HHMM below loosely)
    let mut n = 0;
    for (hi, host) in hosts.iter().enumerate() {
        for minute in 0..5 {
            let epoch = base_epoch + (minute as i64) * 60;
            let hhmm = format!("19{:02}", 10 + minute);
            let key = segment_key("2026-04-09", &hhmm, "shale", host, epoch + hi as i64, 0);
            put(client, bucket, &key, body.to_vec()).await;
            n += 1;
        }
    }
    n
}

/// The `thread_class` and `source` query filters must reach the aggregator and
/// change the sample count, end-to-end through `/api/flamegraph`. This is the
/// regression test for the bug where the filter selectors were silently ignored
/// (the on/off split "worked in the viewer but not the pure flamegraph view").
///
/// Expected counts come from [`mini_counts`], which are embedded in the
/// synthetic trace ([`mini_trace`]) the test folds — so they can never drift
/// from the trace. The trace has 6 CpuProfile (5 worker / 1 off), 3 SchedEvent,
/// 9 total.
#[tokio::test]
async fn flamegraph_thread_and_source_filters_apply() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    // A single segment so every sample comes from one folded file: the totals
    // are then exactly the per-filter counts, not a multiple.
    let key = segment_key("2026-04-09", "1910", "shale", "host-a", 1_744_224_000, 0);
    put(&uploader, "src-bucket", &key, body).await;

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();
    let want = mini_counts();

    // Stream folds the one file and returns the final (at-cap) snapshot. Each
    // subsequent stream re-reads the (idempotently) already-folded file under a
    // new filter.
    let folded = stream_final(&http, &base, "service=shale&source=all").await;
    assert_eq!(
        folded.coverage.as_ref().map(|c| c.files_folded),
        Some(1),
        "the single segment folds"
    );

    // source=all → every sample (CpuProfile + SchedEvent).
    assert_eq!(
        folded.total_samples, want.total,
        "source=all counts every sample"
    );

    // Default (no source param) → on-CPU profile only, matching the viewer.
    let def = stream_final(&http, &base, "service=shale").await;
    assert_eq!(
        def.total_samples, want.cpu,
        "default view = CpuProfile only"
    );

    // source=cpu is the same as the default.
    let cpu = stream_final(&http, &base, "service=shale&source=cpu").await;
    assert_eq!(cpu.total_samples, want.cpu, "source=cpu = CpuProfile only");

    // source=sched → the scheduler context-switch series.
    let sched = stream_final(&http, &base, "service=shale&source=sched").await;
    assert_eq!(
        sched.total_samples, want.sched,
        "source=sched = SchedEvent only"
    );

    // thread_class=worker over the on-CPU source → on-runtime CpuProfile samples.
    let worker = stream_final(&http, &base, "service=shale&source=cpu&thread_class=worker").await;
    assert_eq!(worker.total_samples, want.cpu_on, "CpuProfile on-runtime");

    // thread_class=off-worker over the on-CPU source → the off-runtime sample.
    let off = stream_final(
        &http,
        &base,
        "service=shale&source=cpu&thread_class=off-worker",
    )
    .await;
    assert_eq!(off.total_samples, want.cpu_off, "CpuProfile off-runtime");

    // The split is exhaustive: worker + off-worker == all CpuProfile samples.
    assert_eq!(
        worker.total_samples + off.total_samples,
        cpu.total_samples,
        "worker + off-worker partitions the CpuProfile samples"
    );
}

/// The raw JSON body of the final `/api/flamegraph` SSE event (the tests using
/// this need the facet/metadata arrays verbatim, which the scalar `Resp` fields
/// drop).
async fn fetch_body(client: &reqwest::Client, base: &str, query: &str) -> String {
    stream_final(client, base, query).await.body
}

/// The response metadata advertises the *available* facets for the scope —
/// host names, sources present, and thread classes present — recorded
/// independent of the active counting filter. This is what makes the flamegraph
/// toolbar data-driven: even when querying `source=cpu`, the response must still
/// report that `sched` data exists so the UI can offer it.
#[tokio::test]
async fn flamegraph_metadata_reports_available_facets() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    let key = segment_key("2026-04-09", "1910", "shale", "host-a", 1_744_224_000, 0);
    put(&uploader, "src-bucket", &key, body).await;

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // Fold the segment (any stream folds it), then read it back filtered to the
    // on-CPU view — the final event carries the facet metadata verbatim.
    let _ = stream_final(&http, &base, "service=shale&source=all").await;
    let json = fetch_body(&http, &base, "service=shale&source=cpu").await;

    // Parse the facets out of the raw JSON string (avoids a recursion limit on
    // the deeply-nested `tree` field).
    let facet_values = |name: &str| -> Vec<String> {
        // Find the facet with this name in the facets array
        let search = format!("\"name\":\"{name}\"");
        let Some(pos) = json.find(&search) else {
            return vec![];
        };
        // Find "values": after this position
        let rest = &json[pos..];
        let vals_start = rest.find("\"values\":").unwrap() + "\"values\":".len();
        let rest = &rest[vals_start..];
        let open = rest.find('[').unwrap();
        let close = rest[open..].find(']').unwrap() + open;
        rest[open + 1..close]
            .split(',')
            .filter_map(|s| {
                let s = s.trim().trim_matches('"');
                (!s.is_empty()).then(|| s.to_string())
            })
            .collect()
    };

    // Host facet: the host parsed from the key path.
    assert_eq!(
        facet_values("host"),
        vec!["host-a".to_string()],
        "host facet reports the scope's host"
    );

    // Sources present is independent of the `source=cpu` counting filter: the
    // demo trace has BOTH CpuProfile and SchedEvent samples in the window.
    let sources = facet_values("source");
    assert!(
        sources.contains(&"cpu".to_string()) && sources.contains(&"sched".to_string()),
        "source facet must list both sources regardless of the source filter, got {sources:?}"
    );

    // Likewise both worker classes are present in the demo trace.
    let threads = facet_values("thread_class");
    assert!(
        threads.contains(&"worker".to_string()) && threads.contains(&"off-worker".to_string()),
        "thread_class facet must list both classes, got {threads:?}"
    );

    // The resolved scope echoes the normalized selectors back to the UI.
    let scope = &json[json.find("\"scope\":").expect("scope present")..];
    assert!(
        scope.contains("\"hosts\":[]"),
        "scope.hosts empty when no host filter was requested"
    );
    // The filters object echoes back the active facet values.
    let filters = &scope[scope.find("\"filters\":").expect("filters in scope")..];
    assert!(
        filters.contains("\"source\":\"cpu\""),
        "scope echoes the active source selector"
    );
    assert!(
        filters.contains("\"thread_class\":\"\""),
        "scope echoes the (empty) thread-class selector"
    );
}

/// The headline Goal-1 test: the full refinement flow over fake S3, now as one
/// SSE stream.
///
/// - The first event is the already-folded snapshot: nothing folded, empty tree.
/// - Coverage climbs monotonically across events as files fold.
/// - The stream stops at the sampling cap (here K=4, floored at the baseline).
/// - A second stream (everything already folded) is idempotent: it re-serves the
///   same cap and totals, with the final event matching the first stream's final.
#[tokio::test]
async fn refinement_loop_folds_progressively_and_caps() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    let total = seed_fleet(&uploader, "src-bucket", &body).await;
    assert_eq!(total, 20, "4 hosts × 5 minutes");

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // One stream drives the whole fold. Cap = max(ceil(0.05 × 20), 4).min(100) =
    // max(1,4) = 4, floored at the baseline. So the stream folds 4 files.
    let events = stream(&http, &base, "service=shale").await;

    // First event: the already-folded snapshot. Nothing folded yet → empty tree,
    // but the matched set (coverage denominator) is already known.
    let first = &events[0];
    let c0 = first.coverage.as_ref().expect("coverage present");
    assert_eq!(c0.files_matched, 20, "all 20 segments match the scope");
    assert_eq!(c0.files_folded, 0, "first event folds nothing");
    assert_eq!(first.total_samples, 0, "nothing folded → no samples yet");

    // Coverage climbs monotonically and every event agrees samples == coverage.
    let mut prev_folded = 0;
    for r in &events {
        let c = r.coverage.as_ref().unwrap();
        assert_eq!(c.files_matched, 20);
        assert!(
            c.files_folded >= prev_folded,
            "coverage is monotonic: {} >= {}",
            c.files_folded,
            prev_folded
        );
        assert_eq!(c.samples_folded, r.total_samples, "samples track coverage");
        prev_folded = c.files_folded;
    }

    // Final event: folded exactly the baseline K=4, with a real tree.
    let last = events.last().unwrap();
    let cf = last.coverage.as_ref().unwrap();
    assert_eq!(cf.files_folded, 4, "stream stops at cap=4");
    assert!(last.total_samples > 0, "at-cap tree has samples");
    assert!(last.tree_has_children, "at-cap tree has real structure");

    // A second stream over the fully-folded set is idempotent: same cap + totals.
    let reload = stream_final(&http, &base, "service=shale").await;
    let cr = reload.coverage.unwrap();
    assert_eq!(cr.files_folded, 4, "reload sees the already-folded set");
    assert_eq!(
        reload.total_samples, last.total_samples,
        "reload is identical to the first stream's final snapshot"
    );
}

/// With a large matched set, the cap is the 5% fraction (not the baseline), so
/// the stream refines across several events before stopping — and never reaches
/// 100%.
#[tokio::test]
async fn refinement_climbs_then_plateaus_below_full() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();

    // 100 segments → cap = ceil(0.05 * 100) = 5 files.
    let mut n = 0;
    for host in 0..10 {
        for minute in 0..10 {
            let epoch = 1_744_224_000i64 + minute * 60;
            let hhmm = format!("20{minute:02}");
            let key = segment_key(
                "2026-04-09",
                &hhmm,
                "shale",
                &format!("host-{host:02}"),
                epoch + host,
                0,
            );
            put(&uploader, "src-bucket", &key, body.clone()).await;
            n += 1;
        }
    }
    assert_eq!(n, 100);

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // One stream folds to the cap. Coverage must climb monotonically and stop at
    // the 5% fraction (5 files), never the whole scope.
    let events = stream(&http, &base, "service=shale").await;
    let mut last_folded = 0;
    for r in &events {
        let c = r.coverage.as_ref().unwrap();
        assert_eq!(c.files_matched, 100);
        assert!(
            c.files_folded >= last_folded,
            "coverage is monotonic: {} >= {}",
            c.files_folded,
            last_folded
        );
        last_folded = c.files_folded;
    }
    assert_eq!(last_folded, 5, "stops exactly at the 5% cap");
    assert!(last_folded < 100, "never folds the whole scope");
}

/// Regression: folded files that fall OUTSIDE the cap window must not starve
/// the in-cap fold budget. Previously `already_folded` was counted over the
/// whole matched set, so folded files scattered beyond the cap (e.g. left by a
/// prior, differently-scoped query sharing the output bucket) made
/// `room = cap - already_folded` go to zero — permanently stalling refinement
/// far below the cap (the observed "stuck at 40 / 14291" bug).
///
/// We reproduce the cross-scope leftovers by folding host-a heavily first, then
/// querying the whole fleet: host-a's many folded files are spread across the
/// fleet-wide order, but the fleet query must still fold its own capped prefix.
#[tokio::test]
async fn folded_outside_cap_does_not_starve_budget() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();

    // 100 segments across 10 hosts × 10 minutes. Fleet cap = ceil(0.05×100) = 5.
    let mut n = 0;
    for host in 0..10 {
        for minute in 0..10 {
            let epoch = 1_744_224_000i64 + minute * 60;
            let key = segment_key(
                "2026-04-09",
                &format!("19{minute:02}"),
                "shale",
                &format!("host-{host:02}"),
                epoch + host,
                0,
            );
            put(&uploader, "src-bucket", &key, body.clone()).await;
            n += 1;
        }
    }
    assert_eq!(n, 100);

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // 1. Fold host-00 deeply via fetch-more (max_files=10 > its 10 files), so up
    //    to 10 host-00 files become folded. These are scattered across the
    //    fleet-wide order (host-00 is just 1 of 10 hosts), mostly OUTSIDE the
    //    fleet cap of 5.
    let host_folded = stream_final(&http, &base, "service=shale&host=host-00&max_files=10")
        .await
        .coverage
        .unwrap()
        .files_folded;
    assert!(
        host_folded >= 5,
        "host-00 folded several files, got {host_folded}"
    );

    // 2. Now query the whole fleet at the DEFAULT cap (5). Pre-fix, the folded
    //    host-00 files (counted across the whole 100-file order) would make
    //    room = 5 - host_folded <= 0, folding nothing. Post-fix, budgeting is
    //    scoped to the capped prefix, so the fleet stream folds toward its cap.
    let fleet = stream_final(&http, &base, "service=shale").await;
    let cf = fleet.coverage.unwrap();
    assert_eq!(cf.files_matched, 100);
    assert_eq!(
        cf.files_folded, 5,
        "fleet refines to its 5% cap despite folded host-00 files outside the cap"
    );
}

/// "Fetch more" (the `max_files` ceiling override) lets a scope refine past the
/// default cap on demand.
#[tokio::test]
async fn fetch_more_raises_the_cap() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    // 40 files → default cap = max(ceil(0.05 × 40), 4) = max(2, 4) = 4 (the
    // baseline floor wins). Kept small: each fold is a full demo-trace decode,
    // so we prove the override with few folds.
    for host in 0..8 {
        for minute in 0..5 {
            let epoch = 1_744_224_000i64 + minute * 60;
            let key = segment_key(
                "2026-04-09",
                &format!("21{minute:02}"),
                "shale",
                &format!("host-{host:02}"),
                epoch + host,
                0,
            );
            put(&uploader, "src-bucket", &key, body.clone()).await;
        }
    }
    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // Default cap (4): one stream folds to it.
    let default = stream_final(&http, &base, "service=shale").await;
    assert_eq!(
        default.coverage.unwrap().files_folded,
        4,
        "default cap = max(5% of 40, baseline 4) = 4"
    );

    // Now request more: max_files=12 raises the ceiling for this scope. The
    // stream serves the 4 already-folded files instantly, then folds 8 more.
    let more = stream_final(&http, &base, "service=shale&max_files=12").await;
    assert_eq!(
        more.coverage.unwrap().files_folded,
        12,
        "fetch-more lifts the cap to 12"
    );
}

/// Re-folding is idempotent: a source file folds to a deterministically named
/// part-file (`{blake3(source_key)}.parquet`), so re-polling writes the same
/// keys and aggregation yields identical counts. We assert the observable: the
/// output bucket holds exactly one samples part-file per folded source file,
/// with no duplicates across repeated polls (the folded-set LIST is the source
/// of truth for what's done — see ADR-0003).
#[tokio::test]
async fn refold_is_idempotent_no_duplicate_part_files() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await;

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    let r1 = stream_final(&http, &base, "service=shale").await;
    let r2 = stream_final(&http, &base, "service=shale").await;
    let r3 = stream_final(&http, &base, "service=shale").await;
    // Identical totals across re-streams: re-folding writes the same keys, and
    // aggregation over the same folded set yields the same counts.
    assert_eq!(r1.total_samples, r2.total_samples);
    assert_eq!(r2.total_samples, r3.total_samples);

    // The output bucket should contain exactly `files_folded` samples part-files
    // (one per folded source file) — no duplicates from re-polling. The output
    // is namespaced by source bucket: `…/v1/bucket={src}/samples/…`.
    let listed = uploader
        .list_objects_v2()
        .bucket("out-bucket")
        .prefix("flamegraph-data/v3/bucket=src-bucket/samples/")
        .send()
        .await
        .unwrap();
    let part_count = listed
        .contents()
        .iter()
        .filter(|o| o.key().is_some_and(|k| k.ends_with(".parquet")))
        .count();
    let folded = r3.coverage.unwrap().files_folded;
    assert_eq!(
        part_count, folded,
        "exactly one part-file per folded source file, no duplicates"
    );
}

/// Scope filtering: a host filter restricts the matched set to that host's
/// files, and a time window restricts to overlapping files.
#[tokio::test]
async fn scope_filters_matched_set() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 4 hosts × 5 minutes = 20

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // Host filter: only host-a's 5 files match.
    let r = stream_final(&http, &base, "service=shale&host=host-a").await;
    assert_eq!(
        r.coverage.unwrap().files_matched,
        5,
        "host filter → 5 files"
    );

    // Wrong service: nothing matches → 404.
    let url = format!("{base}/api/flamegraph?service=does-not-exist");
    let status = http.get(&url).send().await.unwrap().status();
    assert_eq!(status.as_u16(), 404, "no matching files → 404");
}

/// A multi-host scope (repeatable `host=` params, as the heatmap box sends)
/// matches the UNION of those hosts' files and excludes the rest.
#[tokio::test]
async fn multi_host_scope_matches_union() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // host-a..d × 5 min = 20

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // Two hosts → 10 files (5 each); host-c/host-d excluded.
    let r = stream_final(&http, &base, "service=shale&host=host-a&host=host-b").await;
    let cov = r.coverage.unwrap();
    assert_eq!(
        cov.files_matched, 10,
        "host set {{a,b}} → union of 10 files"
    );
    assert_eq!(
        cov.hosts_matched, 2,
        "host set {{a,b}} → 2 distinct hosts in the matched set"
    );
    // The folded sample spans a subset of the matched fleet (exact count is
    // order-dependent, so assert the invariant rather than a fixed number).
    assert!(
        cov.files_folded > 0 && (1..=cov.hosts_matched).contains(&cov.hosts_folded),
        "hosts_folded ({}) within 1..={} once files are folded ({})",
        cov.hosts_folded,
        cov.hosts_matched,
        cov.files_folded,
    );

    // Three hosts → 15.
    let r3 = stream_final(
        &http,
        &base,
        "service=shale&host=host-a&host=host-b&host=host-c",
    )
    .await;
    let cov3 = r3.coverage.unwrap();
    assert_eq!(cov3.files_matched, 15);
    assert_eq!(cov3.hosts_matched, 3, "3 hosts in the matched set");
}

/// `/api/config` advertises `aggregation_enabled: true` when the server runs
/// the refinement loop, so the client knows to drive the sampled path.
#[tokio::test]
async fn config_reports_aggregation_enabled() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();
    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    let body = http
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("\"aggregation_enabled\":true"),
        "config should advertise aggregation enabled, got: {body}"
    );
}

/// Count objects under `prefix` in `bucket` of the simulated S3.
async fn count_objects(client: &aws_sdk_s3::Client, bucket: &str, prefix: &str) -> usize {
    client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await
        .unwrap()
        .contents()
        .len()
}

/// Regression: in BYOC mode (`/api/flamegraph?bucket=…`) with a configured
/// output bucket, aggregated part-files must be written to the OUTPUT bucket,
/// not the source bucket. Previously the BYOC path hardcoded
/// `output_bucket = source bucket`, so folding a read-only source bucket failed
/// with S3 AccessDenied on the first PutObject (and, on a writable source,
/// polluted it with `flamegraph-data/`).
#[tokio::test]
async fn byoc_writes_output_to_configured_bucket_not_source() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 20 segments

    let base = start_byoc_server(fs.path(), "src-bucket", Some("out-bucket"), 60).await;
    let http = reqwest::Client::new();

    // Drive the BYOC path (bucket query param) — folds the baseline and returns
    // a real tree. This is the request that used to fail on the source bucket.
    let r = stream_final(&http, &base, "bucket=src-bucket&service=shale&host=host-a").await;
    assert!(
        r.total_samples > 0,
        "BYOC poll should fold and return samples"
    );
    assert!(r.tree_has_children, "tree should be non-trivial");

    // Output part-files land in the OUTPUT bucket …
    let out_n = count_objects(&uploader, "out-bucket", "flamegraph-data/").await;
    assert!(out_n > 0, "expected part-files in out-bucket, found none");

    // … and NOT in the source bucket (no `flamegraph-data/` written there).
    let src_pollution = count_objects(&uploader, "src-bucket", "flamegraph-data/").await;
    assert_eq!(
        src_pollution, 0,
        "source bucket must not receive aggregated output"
    );
}

/// When NO output bucket is configured, BYOC aggregation falls back to writing
/// into the source bucket (the historical behavior, valid when the source is
/// writable). Guards the fallback branch of the output-bucket routing.
#[tokio::test]
async fn byoc_without_output_bucket_writes_to_source() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await;

    let base = start_byoc_server(fs.path(), "src-bucket", None, 60).await;
    let http = reqwest::Client::new();

    let r = stream_final(&http, &base, "bucket=src-bucket&service=shale&host=host-a").await;
    assert!(r.total_samples > 0);

    let src_n = count_objects(&uploader, "src-bucket", "flamegraph-data/").await;
    assert!(
        src_n > 0,
        "with no output override, output falls back to the source bucket"
    );
}

/// The `/api/tokio-stats` SSE endpoint streams the same refinement machinery as
/// flamegraph: one request folds to the sampling cap, emitting a
/// `TokioStatsResponse` per file. We assert the stream mechanics — an initial
/// already-folded snapshot, monotonic coverage climbing to the cap, and a
/// well-formed final event — over the synthetic fleet. (The synthetic trace has
/// no poll spans, so `total_polls` is 0; poll *contents* are covered by the
/// `read_polls_part` unit test in `src/server/tokio_stats.rs`.)
#[tokio::test]
async fn tokio_stats_streams_and_refines_to_cap() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 20 segments → cap 4

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    // Collect every event of the tokio-stats stream.
    let url = format!("{base}/api/tokio-stats?service=shale");
    let resp = http.get(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "tokio-stats stream failed: {}",
        resp.status()
    );
    let events = parse_sse_events(&resp.text().await.unwrap());
    assert!(!events.is_empty(), "tokio-stats produced no events");

    // First event: already-folded snapshot (nothing folded yet).
    let c0 = extract_coverage(&events[0]).expect("coverage present");
    assert_eq!(c0.files_matched, 20, "all 20 segments match");
    assert_eq!(c0.files_folded, 0, "first event folds nothing");

    // Coverage climbs monotonically; every event carries a total_polls field.
    let mut prev = 0;
    for ev in &events {
        assert!(
            extract_usize(ev, "\"total_polls\":").is_some(),
            "each event has total_polls"
        );
        let c = extract_coverage(ev).unwrap();
        assert_eq!(c.files_matched, 20);
        assert!(c.files_folded >= prev, "monotonic coverage");
        prev = c.files_folded;
    }

    // Final event stops at the cap (4 files for 20 matched).
    let cf = extract_coverage(events.last().unwrap()).unwrap();
    assert_eq!(cf.files_folded, 4, "tokio-stats stream stops at cap=4");
}

/// tokio-stats "Load more": `max_files` raises the sampling-cap ceiling, the
/// same as the flamegraph's fetch-more (see [`fetch_more_raises_the_cap`]).
/// A reopened stream serves the already-folded prefix instantly, then folds
/// deeper into the matched set up to the new cap.
#[tokio::test]
async fn tokio_stats_max_files_raises_the_cap() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 20 segments → default cap 4

    let base = start_agg_server(fs.path(), "src-bucket", "out-bucket", 60).await;
    let http = reqwest::Client::new();

    let final_folded = |body: String| {
        let events = parse_sse_events(&body);
        extract_coverage(events.last().unwrap())
            .unwrap()
            .files_folded
    };

    // Default cap: 4.
    let resp = http
        .get(format!("{base}/api/tokio-stats?service=shale"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        final_folded(resp.text().await.unwrap()),
        4,
        "default cap = max(5% of 20, baseline 4) = 4"
    );

    // Load more: max_files=12 lifts the ceiling for this scope.
    let resp = http
        .get(format!("{base}/api/tokio-stats?service=shale&max_files=12"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        final_folded(resp.text().await.unwrap()),
        12,
        "max_files lifts the tokio-stats cap to 12"
    );
}

/// Regression: when folds FAIL (here: the aggregation OUTPUT client returns 403
/// AccessDenied on every `PutObject`, the analogue of a read-only output bucket),
/// the stream must SURFACE the failures rather than silently returning an empty
/// tree. Coverage should report a non-zero `fold_errors` and a
/// `fold_error_sample` message, while `files_folded` stays 0.
#[tokio::test]
async fn fold_failures_are_reported_in_coverage() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 20 segments → cap 4

    // Inject an output client that denies writes: folds fetch + decode fine (the
    // SOURCE is the normal fake S3) but every part-file PUT gets 403 AccessDenied.
    let output = Arc::new(S3Backend::from_client(access_denied_on_put_client()));
    let base =
        start_agg_server_with_output(fs.path(), "src-bucket", "out-bucket", 60, output).await;
    let http = reqwest::Client::new();

    let events = stream(&http, &base, "service=shale").await;

    // First event is the already-folded snapshot: nothing folded, no errors yet.
    let first = events[0].coverage.as_ref().unwrap();
    assert_eq!(first.files_folded, 0);
    assert_eq!(
        first.fold_errors, 0,
        "no folds attempted yet on the first event"
    );

    // Final event: every attempted fold failed, so files_folded stayed 0 but the
    // failures are counted (the whole point — not a silent empty result).
    let last = events.last().unwrap();
    let cf = last.coverage.as_ref().unwrap();
    assert_eq!(
        cf.files_folded, 0,
        "no file could be written, so none folded"
    );
    assert_eq!(cf.samples_folded, 0, "empty tree");
    assert_eq!(
        cf.fold_errors, 4,
        "all 4 capped folds failed and were counted"
    );
    // The representative error message rides on the event for the UI to show.
    assert!(
        last.body.contains("\"fold_error_sample\":"),
        "coverage carries a fold_error_sample message; body = {}",
        last.body
    );
}

/// Regression for the fold commit ordering: when a fold's part-file writes only
/// PARTIALLY succeed (here: `polls/` PUTs fail while everything else works —
/// the same interleaving a fold task cancelled mid-write can produce, and with
/// SSE streams a client disconnect cancels in-flight folds routinely), the file
/// must NOT be recorded as folded. The `samples/` part is the durable folded
/// record (`list_folded_leaves` lists it), so it must be written last: a file
/// recorded as folded with its polls part missing would serve incomplete
/// tokio-stats silently, forever — folded files are never re-folded.
#[tokio::test]
async fn partial_write_failure_does_not_commit_fold() {
    let fs = tempfile::tempdir().unwrap();
    std::fs::create_dir(fs.path().join("src-bucket")).unwrap();
    std::fs::create_dir(fs.path().join("out-bucket")).unwrap();

    let uploader = fake_s3_client(fs.path());
    let body = mini_trace_gz();
    seed_fleet(&uploader, "src-bucket", &body).await; // 20 segments → cap 4

    let output = Arc::new(FailingPuts {
        inner: Arc::new(S3Backend::from_client(fake_s3_client(fs.path()))),
        fail_substr: "/polls/",
    });
    let base =
        start_agg_server_with_output(fs.path(), "src-bucket", "out-bucket", 60, output).await;
    let http = reqwest::Client::new();

    // Every fold attempt fails at the polls write and is reported as an error.
    let r = stream_final(&http, &base, "service=shale").await;
    let c = r.coverage.unwrap();
    assert_eq!(c.fold_errors, 4, "all 4 capped folds failed");
    assert_eq!(c.files_folded, 0, "a failed fold is not counted as folded");

    // The load-bearing assertion: a fresh stream must ALSO see nothing folded.
    // If the samples part had been written before (or concurrently with) the
    // failing polls part, the folded-set listing would now claim these files
    // are folded — committing them with their polls data missing forever.
    let again = stream(&http, &base, "service=shale").await;
    let c0 = again[0].coverage.as_ref().unwrap();
    assert_eq!(
        c0.files_folded, 0,
        "no samples part may exist after a partial write failure (samples must be written last)"
    );
}
