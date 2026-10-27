use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{Sampling, StartupFailure, s3::S3SamplingStore, simulation::MemoryStore};
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(serde::Deserialize, dial9_trace_format::TraceEvent)]
struct SamplingMarker {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u64,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn fake_service(root: &std::path::Path) -> s3s::service::S3Service {
    let fs = s3s_fs::FileSystem::new(root).unwrap();
    let mut service = s3s::service::S3ServiceBuilder::new(fs);
    service.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
    service.build()
}

fn fake_client(service: s3s::service::S3Service) -> aws_sdk_s3::Client {
    let transport: s3s_aws::Client = service.into();
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(transport)
            .force_path_style(true)
            .build(),
    )
}

#[test]
fn two_clients_upload_readable_trace_and_denied_process_allocates_no_writer() {
    let root = tempfile::tempdir().unwrap();
    let sampling_runtime = runtime();
    // All SDK clients must share one server. Independent FileSystem instances
    // reuse temporary names and can corrupt simultaneous writes in the same root.
    let service = fake_service(root.path());
    let verify = fake_client(service.clone());
    sampling_runtime
        .block_on(verify.create_bucket().bucket("traces").send())
        .unwrap();
    let coordination_clients = AtomicUsize::new(0);
    let upload_clients = Arc::new(AtomicUsize::new(0));
    let writers = AtomicUsize::new(0);
    let policy = Sampling::builder()
        .scope("customer/prod")
        .add_dimension("az", "a")
        .exact_target(1)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::disabled())
        .build();
    let make_store = || async {
        coordination_clients.fetch_add(1, Ordering::SeqCst);
        Ok(S3SamplingStore::builder()
            .client(fake_client(service.clone()))
            .bucket("traces")
            .prefix("coord/")
            .build())
    };
    let session = sampling_runtime.block_on(policy.prepare(make_store()));
    assert!(session.enabled(), "{session:?}");
    let config = dial9::s3::S3Config::builder()
        .bucket("traces")
        .service_name("manual-test")
        .prefix("data")
        .region("us-east-1")
        .build();
    let uploader_service = service.clone();
    let count = upload_clients.clone();
    let recorder = if session.enabled() {
        writers.fetch_add(1, Ordering::SeqCst);
        dial9::recorder(MemoryBuffer::new(1 << 20).unwrap())
            .with_s3_uploader_client_future(config, async move {
                count.fetch_add(1, Ordering::SeqCst);
                fake_client(uploader_service)
            })
            .build()
    } else {
        dial9::recorder_disabled()
    };
    let recorder = recorder.attach_object(session);
    let denied = sampling_runtime.block_on(policy.prepare(make_store()));
    assert!(!denied.enabled());
    let disabled = if denied.enabled() {
        writers.fetch_add(1, Ordering::SeqCst);
        dial9::recorder(MemoryBuffer::new(1 << 20).unwrap()).build()
    } else {
        dial9::recorder_disabled()
    };
    let disabled = disabled.attach_object(denied);
    assert!(!disabled.handle().is_connected());
    disabled.enable();
    assert!(!disabled.handle().is_enabled());
    let mut app = tokio::runtime::Builder::new_current_thread();
    app.enable_all();
    let app = recorder
        .handle()
        .attach_tokio_runtime(app, TokioAttachOptions::default())
        .unwrap();
    app.block_on(async {
        dial9::spawn(async {
            dial9::Dial9Handle::current().record_event(SamplingMarker {
                timestamp_ns: dial9::core::clock_monotonic_ns(),
                value: 42,
            });
        })
        .await
        .unwrap();
    });
    drop(app);
    recorder.graceful_shutdown(Duration::from_secs(5));
    drop(disabled);
    assert_eq!(coordination_clients.load(Ordering::SeqCst), 2);
    assert_eq!(upload_clients.load(Ordering::SeqCst), 1);
    assert_eq!(writers.load(Ordering::SeqCst), 1);
    let data = sampling_runtime.block_on(async {
        let traces = verify
            .list_objects_v2()
            .bucket("traces")
            .prefix("data/version=1/")
            .send()
            .await
            .unwrap();
        let key = traces
            .contents()
            .iter()
            .find_map(|o| o.key().filter(|k| k.ends_with(".bin.gz")))
            .expect("selected recorder uploaded trace");
        verify
            .get_object()
            .bucket("traces")
            .key(key)
            .send()
            .await
            .unwrap()
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes()
    });
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(data.as_ref())
        .read_to_end(&mut bytes)
        .unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&bytes).unwrap();
    let mut found = false;
    decoder
        .for_each_event(|event| {
            if event.name == "SamplingMarker" {
                found |= event.deserialize::<SamplingMarker>().unwrap().value == 42;
            }
        })
        .unwrap();
    assert!(found);
    let next = sampling_runtime.block_on(policy.prepare(make_store()));
    assert!(next.enabled(), "recorder-owned cleanup released capacity");
    sampling_runtime.block_on(next.shutdown(Duration::from_secs(5)));
}

static STORE: Mutex<Option<Arc<MemoryStore>>> = Mutex::new(None);

async fn configure() -> std::io::Result<dial9::AttachedRuntime> {
    let store = STORE.lock().unwrap().as_ref().unwrap().clone();
    let session = Sampling::builder()
        .scope("macro")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build()
        .prepare(async { Ok(store) })
        .await;
    assert!(session.enabled());
    let recorder = if session.enabled() {
        dial9::recorder(MemoryBuffer::new(1 << 20)?).build()
    } else {
        dial9::recorder_disabled()
    };
    let mut app = tokio::runtime::Builder::new_current_thread();
    app.enable_all();
    let app = recorder
        .handle()
        .attach_tokio_runtime(app, TokioAttachOptions::default())?;
    Ok((recorder.attach_object(session), app))
}

#[dial9::main(config_async = configure)]
async fn application() -> bool {
    dial9::Dial9Handle::current().is_enabled()
}

#[test]
fn ordinary_async_configuration_macro_retains_manual_session_until_application_shutdown() {
    let store = Arc::new(MemoryStore::new());
    *STORE.lock().unwrap() = Some(store.clone());
    assert!(application());
    let runtime = runtime();
    let next = runtime.block_on(
        Sampling::builder()
            .scope("macro")
            .exact_target(1)
            .if_failed(StartupFailure::disabled())
            .build()
            .prepare(async { Ok(store) }),
    );
    assert!(next.enabled());
    runtime.block_on(next.shutdown(Duration::from_secs(1)));
}

#[test]
fn failed_recorder_setup_can_release_guard_before_returning_error() {
    let runtime = runtime();
    let store = Arc::new(MemoryStore::new());
    let policy = Sampling::builder()
        .scope("writer-error")
        .exact_target(1)
        .if_failed(StartupFailure::disabled())
        .build();
    let session = runtime.block_on(policy.prepare(async { Ok(store.clone()) }));
    assert!(session.enabled());
    let directory = tempfile::tempdir().unwrap();
    let invalid_parent = directory.path().join("file-as-parent");
    std::fs::write(&invalid_parent, b"not a directory").unwrap();
    assert!(dial9::DiskBuffer::single_file(invalid_parent.join("trace.bin")).is_err());
    runtime.block_on(session.shutdown(Duration::from_secs(1)));
    let next = runtime.block_on(policy.prepare(async { Ok(store) }));
    assert!(next.enabled());
    runtime.block_on(next.shutdown(Duration::from_secs(1)));
}

#[test]
fn s3_adapter_enforces_etag_conditions_and_sorted_pagination() {
    use dial9_sampling::backend::{
        ListRequest, PutOutcome, PutRequest, SamplingStore, WriteCondition,
    };
    let root = tempfile::tempdir().unwrap();
    let client = fake_client(fake_service(root.path()));
    let rt = runtime();
    rt.block_on(async {
        client
            .create_bucket()
            .bucket("traces")
            .send()
            .await
            .unwrap();
        let store = dial9_sampling::s3::S3SamplingStore::builder()
            .client(client)
            .bucket("traces")
            .prefix("sampling/")
            .build();
        let outcome = store
            .put(PutRequest::builder().key("a").body("one".into()).build())
            .await
            .unwrap();
        let PutOutcome::Stored { version, .. } = outcome else {
            panic!("first create failed")
        };
        assert!(matches!(
            store
                .put(PutRequest::builder().key("a").body("two".into()).build())
                .await
                .unwrap(),
            PutOutcome::PreconditionFailed { .. }
        ));
        store
            .put(
                PutRequest::builder()
                    .key("a")
                    .body("two".into())
                    .condition(WriteCondition::matching(version.clone()))
                    .build(),
            )
            .await
            .unwrap();
        assert!(matches!(
            store
                .put(
                    PutRequest::builder()
                        .key("a")
                        .body("three".into())
                        .condition(WriteCondition::matching(version))
                        .build()
                )
                .await
                .unwrap(),
            PutOutcome::PreconditionFailed { .. }
        ));
        assert_eq!(
            store
                .get("a".into())
                .await
                .unwrap()
                .unwrap()
                .body()
                .as_ref(),
            b"two"
        );
        store
            .put(PutRequest::builder().key("b").body("b".into()).build())
            .await
            .unwrap();
        let page = store
            .list(ListRequest::builder().prefix("").page_size(1).build())
            .await
            .unwrap();
        assert_eq!(page.objects()[0].key(), "a");
        let page = store
            .list(
                ListRequest::builder()
                    .prefix("")
                    .page_size(1)
                    .continuation(page.continuation().unwrap().clone())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(page.objects()[0].key(), "b");
    });
}
