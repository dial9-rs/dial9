use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{
    Sampling, SamplingStatus, StartupFailure,
    backend::{
        ListPage, ListRequest, PutOutcome, PutRequest, SamplingStore, StoreFuture, StoredObject,
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
    store: S3SamplingStore,
    member_written: Mutex<Option<oneshot::Sender<String>>>,
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
                let notice = if key.starts_with("v2/members/") {
                    &self.member_written
                } else if key.ends_with("/roster.json") {
                    &self.roster_written
                } else {
                    return Ok(outcome);
                };
                if let Some(notice) = notice.lock().unwrap().take()
                    && notice.send(key).is_err()
                {
                    tracing::debug!("completed sampler write after test observer ended");
                }
            }
            Ok(outcome)
        })
    }

    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.store.list(request)
    }

    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.store.delete(key)
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
    let policy = Sampling::builder()
        .scope("customer/prod")
        .add_dimension("az", "a")
        .target(1)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::assume_population(1))
        .build();
    let disabled_policy = Sampling::builder()
        .scope("customer/prod-unarmed")
        .add_dimension("az", "a")
        .target(1)
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
    policy
        .with_backend(make_store())
        .start_application(|status| {
            let SamplingStatus { enabled, .. } = status;
            assert!(enabled);
            let config = dial9::s3::S3Config::builder()
                .bucket("traces")
                .service_name("manual-test")
                .prefix("data")
                .region("us-east-1")
                .build();
            let uploader_service = service.clone();
            let count = upload_clients.clone();
            let recorder = if enabled {
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
            status.register_recorder(&recorder);
            disabled_policy
                .with_backend(make_store())
                .start_application(|SamplingStatus { enabled, .. }| {
                    assert!(!enabled);
                    let uploader_service = service.clone();
                    let count = upload_clients.clone();
                    let upload = async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        fake_client(uploader_service)
                    };
                    let disabled = if enabled {
                        writers.fetch_add(1, Ordering::SeqCst);
                        dial9::recorder(MemoryBuffer::new(1 << 20).unwrap())
                            .with_s3_uploader_client_future(
                                dial9::s3::S3Config::builder()
                                    .bucket("traces")
                                    .service_name("manual-test-unarmed")
                                    .prefix("unarmed-data")
                                    .region("us-east-1")
                                    .build(),
                                upload,
                            )
                            .build()
                    } else {
                        drop(upload);
                        dial9::recorder_disabled()
                    };
                    assert!(!disabled.handle().is_connected());
                    disabled.enable();
                    assert!(!disabled.handle().is_enabled());
                });
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
    policy
        .with_backend(make_store())
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(enabled, "the next invocation follows its startup policy");
        });
}

#[test]
fn failed_recorder_setup_withdraws_membership_and_preserves_roster() {
    let root = tempfile::tempdir().unwrap();
    let service = fake_service(root.path());
    let rt = runtime();
    rt.block_on(
        fake_client(service.clone())
            .create_bucket()
            .bucket("traces")
            .send(),
    )
    .unwrap();
    let make_store = || async {
        Ok(S3SamplingStore::builder()
            .client(fake_client(service.clone()))
            .bucket("traces")
            .build())
    };
    let policy = Sampling::builder()
        .scope("writer-error")
        .target(1)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::assume_population(1))
        .build();
    let directory = tempfile::tempdir().unwrap();
    let invalid_parent = directory.path().join("file-as-parent");
    std::fs::write(&invalid_parent, b"not a directory").unwrap();
    let (member_written, member) = oneshot::channel();
    let (roster_written, roster) = oneshot::channel();
    let mut written_keys = None;
    let result = policy
        .with_backend(async {
            Ok(ObservedWrites {
                store: make_store().await?,
                member_written: Mutex::new(Some(member_written)),
                roster_written: Mutex::new(Some(roster_written)),
            })
        })
        .start_application(|SamplingStatus { enabled, .. }| {
            assert!(enabled);
            // Await actual completed S3 writes, so cleanup is checked against
            // established membership and a persistent roster, without sleeps.
            written_keys = Some(rt.block_on(async {
                let member = tokio::time::timeout(Duration::from_secs(5), member)
                    .await
                    .unwrap()
                    .unwrap();
                let roster = tokio::time::timeout(Duration::from_secs(5), roster)
                    .await
                    .unwrap()
                    .unwrap();
                (member, roster)
            }));
            dial9::DiskBuffer::single_file(invalid_parent.join("trace.bin"))
        });
    assert!(result.is_err());
    let (member, roster) = written_keys.unwrap();
    rt.block_on(async {
        let store = make_store().await.unwrap();
        assert!(store.get(member).await.unwrap().is_none());
        assert!(store.get(roster).await.unwrap().is_some());
        assert!(
            store
                .list(ListRequest::builder().prefix("v2/members/").build())
                .await
                .unwrap()
                .objects()
                .is_empty()
        );
    });
    policy
        .with_backend(make_store())
        .start_application(|SamplingStatus { enabled, .. }| assert!(enabled));
}

#[rstest::rstest]
#[case(None, "_dial9_sampling/")]
#[case(Some("sampling/"), "sampling/")]
#[case(Some(""), "")]
#[test]
fn s3_adapter_enforces_etag_conditions_and_complete_pagination(
    #[case] prefix: Option<&str>,
    #[case] expected_prefix: &str,
) {
    use dial9_sampling::backend::WriteCondition;
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
        let builder = S3SamplingStore::builder()
            .client(client.clone())
            .bucket("traces");
        let store = match prefix {
            Some(prefix) => builder.prefix(prefix).build(),
            None => builder.build(),
        };
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
        assert_eq!(page.objects().len(), 1);
        let mut listed = vec![page.objects()[0].key().to_owned()];
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
        assert_eq!(page.objects().len(), 1);
        assert!(page.continuation().is_none());
        listed.push(page.objects()[0].key().to_owned());
        listed.sort();
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
        assert_eq!(
            keys,
            [format!("{expected_prefix}a"), format!("{expected_prefix}b")],
        );
    });
}
