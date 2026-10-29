use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{
    Sampling, SamplingStatus, StartupFailure,
    backend::{
        self, ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture,
        StoredObject,
    },
    s3::S3SamplingStore,
};
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[allow(dead_code, unexpected_cfgs)]
#[path = "../../dial9-sampling/src/test_store.rs"]
mod test_store;
use test_store::MemoryStore;

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

#[derive(Debug)]
struct ObservedWrites {
    store: Arc<MemoryStore>,
    roster_written: Mutex<Option<oneshot::Sender<String>>>,
}

impl SamplingStore for ObservedWrites {
    fn modification_time_uncertainty(&self) -> Duration {
        self.store.modification_time_uncertainty()
    }

    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        self.store.get(key)
    }

    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            let key = request.key().to_owned();
            let outcome = self.store.put(request).await?;
            if matches!(outcome, PutOutcome::Stored { .. }) {
                if !key.ends_with("/roster.json") {
                    return Ok(outcome);
                }
                let notice = &self.roster_written;
                if let Some(notice) = notice.lock().unwrap().take()
                    && notice.send(key).is_err()
                {
                    tracing::debug!("completed sampler write after test observer ended");
                }
            }
            Ok(outcome)
        })
    }

    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.store.list_sorted_page(request)
    }
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
    let config = dial9::s3::S3Config::builder()
        .bucket("traces")
        .service_name("manual-test")
        .prefix("data")
        .region("us-east-1")
        .build();
    let make_store = || async {
        coordination_clients.fetch_add(1, Ordering::SeqCst);
        S3SamplingStore::builder()
            .client(fake_client(service.clone()))
            .config(config.clone())
            .build()
            .await
    };
    for (environment, fallback, expected) in [
        ("customer/prod", StartupFailure::assume_population(1), true),
        ("customer/prod-unarmed", StartupFailure::disabled(), false),
    ] {
        Sampling::builder()
            .add_dimension("environment", environment)
            .add_dimension("az", "a")
            .num_target_profilers(1)
            .startup_timeout(Duration::from_secs(5))
            .if_failed(fallback)
            .build()
            .with_storage(make_store())
            .start_application(|status| {
                let SamplingStatus { enabled, .. } = status;
                assert_eq!(enabled, expected);
                let recorder = if enabled {
                    writers.fetch_add(1, Ordering::SeqCst);
                    let uploader_service = service.clone();
                    let count = upload_clients.clone();
                    dial9::recorder(MemoryBuffer::new(1 << 20).unwrap())
                        .with_s3_uploader_client_future(config.clone(), async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            fake_client(uploader_service)
                        })
                        .build()
                } else {
                    dial9::recorder_disabled()
                };
                status.register_recorder(&recorder);
                assert_eq!(recorder.handle().is_connected(), expected);
                if !enabled {
                    recorder.enable();
                    assert!(!recorder.handle().is_enabled());
                }
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
            });
    }
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
    let mut metadata = std::collections::HashMap::new();
    decoder
        .for_each_event(|event| {
            if event.name == "SamplingMarker" {
                found |= event.deserialize::<SamplingMarker>().unwrap().value == 42;
            } else if event.name == "SegmentMetadataEvent"
                && let Some(dial9_trace_format::types::FieldValueRef::StringMap(entries)) =
                    event.fields.first()
            {
                metadata.extend(
                    entries
                        .iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned())),
                );
            }
        })
        .unwrap();
    assert!(found);
    assert_eq!(metadata["dial9.sampling.weight"], "1");
    assert_eq!(
        metadata["dial9.sampling.pool"],
        "az=a/environment=customer%2Fprod/"
    );
    assert_eq!(metadata["service_name"], config.service_name());
    assert_eq!(metadata["bucket"], config.bucket());
}

#[test]
fn failed_recorder_setup_preserves_roster_without_deleting_observations() {
    // MemoryStore observes callback-error cleanup; native uploads are covered separately.
    let store = Arc::new(MemoryStore::new());
    let rt = runtime();
    let policy = Sampling::builder()
        .num_target_profilers(1)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::assume_population(1))
        .build();
    let directory = tempfile::tempdir().unwrap();
    let invalid_parent = directory.path().join("file-as-parent");
    std::fs::write(&invalid_parent, b"not a directory").unwrap();
    let (roster_written, roster) = oneshot::channel();
    let mut written_roster = None;
    let mut observed_keys = Vec::new();
    let result = policy
        .with_storage(async {
            Ok(ObservedWrites {
                store: store.clone(),
                roster_written: Mutex::new(Some(roster_written)),
            })
        })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(enabled);
            // Await an actual completed roster publication before the callback fails.
            written_roster = Some(rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), roster)
                    .await
                    .unwrap()
                    .unwrap()
            }));
            observed_keys.extend(store.objects().into_keys());
            dial9::DiskBuffer::single_file(invalid_parent.join("trace.bin"))
        });
    assert!(result.is_err());
    let roster = written_roster.unwrap();
    rt.block_on(async {
        assert!(store.get(roster).await.unwrap().is_some());
    });
    for key in observed_keys {
        assert!(store.objects().contains_key(&key), "shutdown removed {key}");
    }
    policy
        .with_storage(async { Ok(store.clone()) })
        .start_application(|SamplingStatus { enabled, .. }| assert!(enabled));
}

#[rstest::rstest]
#[case(None, "_dial9_sampling/service=manual-test/")]
#[case(Some("sampling"), "sampling/_dial9_sampling/service=manual-test/")]
#[test]
fn s3_adapter_enforces_etag_conditions_and_complete_pagination(
    #[case] prefix: Option<&str>,
    #[case] expected_prefix: &str,
) {
    use dial9_sampling::backend::WriteCondition;
    // s3s-fs normalizes separators; empty/trailing prefixes are checked at the SDK boundary.
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
        let config = dial9::s3::S3Config::builder()
            .bucket("traces")
            .service_name("manual-test")
            .maybe_prefix(prefix.map(str::to_owned))
            .region("us-east-1")
            .build();
        let store = S3SamplingStore::builder()
            .client(client.clone())
            .config(config)
            .build()
            .await
            .unwrap();
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
            .list_sorted_page(ListRequest::builder().prefix("").page_size(1).build())
            .await
            .unwrap();
        assert_eq!(page.objects().len(), 1);
        let mut listed = vec![page.objects()[0].key().to_owned()];
        let page = store
            .list_sorted_page(
                ListRequest::builder()
                    .prefix("")
                    .page_size(1)
                    .continuation(page.continuation().unwrap().clone())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(page.objects().len(), 1);
        assert!(page.continuation().is_none());
        listed.push(page.objects()[0].key().to_owned());
        assert_eq!(listed, ["a", "b"]);
        let mut keys = client
            .list_objects_v2()
            .bucket("traces")
            .send()
            .await
            .unwrap()
            .contents()
            .iter()
            .map(|object| object.key().unwrap().to_string())
            .collect::<Vec<_>>();
        keys.sort();
        let namespace = keys[0].strip_suffix('a').unwrap();
        assert!(namespace.starts_with(expected_prefix));
        assert_eq!(keys, [format!("{namespace}a"), format!("{namespace}b")],);
    });
}
