//! Native S3 error classification and malformed-response regressions.
#![cfg(feature = "s3")]
#![allow(clippy::unwrap_used, reason = "Test assertions fail immediately.")]

use aws_smithy_runtime_api::{
    client::{
        http::{HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
        orchestrator::{HttpRequest, HttpResponse},
    },
    http::StatusCode,
};
use aws_smithy_types::body::SdkBody;
use dial9_sampling::backend::*;
use dial9_sampling::s3::S3SamplingStore;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
struct ResponseFixture {
    status: u16,
    body: String,
    etag: Option<&'static str>,
    modified: Option<&'static str>,
    bucket_region: Option<&'static str>,
    broken_body: bool,
}
impl HttpConnector for ResponseFixture {
    fn call(&self, _: HttpRequest) -> HttpConnectorFuture {
        let body = if self.broken_body {
            SdkBody::taken()
        } else {
            SdkBody::from(self.body.clone())
        };
        let mut response = HttpResponse::new(StatusCode::try_from(self.status).unwrap(), body);
        if let Some(etag) = self.etag {
            response.headers_mut().insert("etag", etag);
        }
        if let Some(modified) = self.modified {
            response.headers_mut().insert("last-modified", modified);
        }
        if let Some(region) = self.bucket_region {
            response.headers_mut().insert("x-amz-bucket-region", region);
        }
        HttpConnectorFuture::new(async move { Ok(response) })
    }
}
async fn store(fixture: ResponseFixture) -> S3SamplingStore {
    store_with_connector(fixture, 1).await
}
async fn store_with_connector(
    connector: impl HttpConnector + 'static,
    attempts: u32,
) -> S3SamplingStore {
    S3SamplingStore::builder()
        .client(sdk_client(connector, attempts))
        .config(
            dial9_destinations_s3::S3Config::builder()
                .bucket("traces")
                .service_name("sampled")
                .prefix("coord")
                .region("us-east-1")
                .build(),
        )
        .build()
        .await
        .unwrap()
}
fn sdk_client(connector: impl HttpConnector + 'static, attempts: u32) -> aws_sdk_s3::Client {
    let connector = SharedHttpConnector::new(connector);
    let http = http_client_fn(move |_, _| connector.clone());
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(attempts),
            )
            .http_client(http)
            .force_path_style(true)
            .build(),
    )
}
#[derive(Debug)]
struct ObservedRequest {
    method: String,
    uri: String,
    authorization: Option<String>,
    if_match: Option<String>,
    if_none_match: Option<String>,
}
#[derive(Debug, Clone)]
struct RecordingFixture {
    responses: Arc<Mutex<VecDeque<ResponseFixture>>>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
}
impl RecordingFixture {
    fn new(responses: impl IntoIterator<Item = ResponseFixture>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
impl HttpConnector for RecordingFixture {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        self.requests.lock().unwrap().push(ObservedRequest {
            method: request.method().into(),
            uri: request.uri().to_string(),
            authorization: request.headers().get("authorization").map(String::from),
            if_match: request.headers().get("if-match").map(String::from),
            if_none_match: request.headers().get("if-none-match").map(String::from),
        });
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected SDK retry");
        response.call(request)
    }
}
fn response(status: u16) -> ResponseFixture {
    ResponseFixture {
        status,
        body: String::new(),
        etag: Some("\"fixture\""),
        modified: Some("Sun, 01 Jan 2023 00:00:00 GMT"),
        bucket_region: None,
        broken_body: false,
    }
}
fn kind(error: &StoreError) -> &'static str {
    match error.kind() {
        StoreErrorKind::Unauthorized { .. } => "unauthorized",
        StoreErrorKind::Unavailable { .. } => "unavailable",
        StoreErrorKind::InvalidResponse { .. } => "invalid",
        StoreErrorKind::WriteOutcomeUnknown { .. } => "unknown",
        other => panic!("unexpected error {other:?}"),
    }
}
#[rstest::rstest]
#[case("get", 403, "unauthorized")]
#[case("get", 400, "invalid")]
#[case("get", 500, "unavailable")]
#[case("list", 403, "unauthorized")]
#[case("list", 400, "invalid")]
#[case("list", 500, "unavailable")]
#[case("delete", 403, "unauthorized")]
#[case("delete", 500, "unavailable")]
#[case("put", 403, "unauthorized")]
#[case("put", 500, "unknown")]
#[tokio::test(start_paused = true)]
async fn backend_errors_preserve_meaning(
    #[case] operation: &str,
    #[case] status: u16,
    #[case] expected: &str,
) {
    let store = store(response(status)).await;
    let error = match operation {
        "get" => store.get("a".into()).await.unwrap_err(),
        "list" => store
            .list_page(ListRequest::builder().prefix("a").build())
            .await
            .unwrap_err(),
        "delete" => store.delete("a".into()).await.unwrap_err(),
        "put" => store
            .put(PutRequest::builder().key("a").body("data".into()).build())
            .await
            .unwrap_err(),
        _ => panic!("unknown operation"),
    };
    assert_eq!(kind(&error), expected);
}
#[rstest::rstest]
#[case(409, true)]
#[case(412, false)]
#[case(404, false)]
#[tokio::test(start_paused = true)]
async fn conditional_conflicts_are_classified(#[case] status: u16, #[case] uncertain: bool) {
    let result = store(response(status))
        .await
        .put(PutRequest::builder().key("a").body("data".into()).build())
        .await;
    if uncertain {
        assert_eq!(kind(&result.unwrap_err()), "unknown");
    } else {
        assert!(matches!(
            result.unwrap(),
            PutOutcome::PreconditionFailed { .. }
        ));
    }
}
#[rstest::rstest]
#[case("etag", "invalid")]
#[case("empty-etag", "invalid")]
#[case("modified", "invalid")]
#[case("negative-time", "invalid")]
#[case("body", "unavailable")]
#[tokio::test(start_paused = true)]
async fn malformed_get_is_not_a_missing_object(#[case] field: &str, #[case] expected: &str) {
    let mut fixture = response(200);
    match field {
        "etag" => fixture.etag = None,
        "empty-etag" => fixture.etag = Some(""),
        "modified" => fixture.modified = None,
        "negative-time" => fixture.modified = Some("Fri, 01 Jan 1960 00:00:00 GMT"),
        "body" => fixture.broken_body = true,
        _ => panic!("unknown field"),
    }
    assert_eq!(
        kind(&store(fixture).await.get("a".into()).await.unwrap_err()),
        expected
    );
}
#[tokio::test(start_paused = true)]
async fn missing_object_and_missing_put_ack_are_distinct() {
    assert!(
        store(response(404))
            .await
            .get("a".into())
            .await
            .unwrap()
            .is_none()
    );
    let mut fixture = response(200);
    fixture.etag = None;
    let error = store(fixture)
        .await
        .put(PutRequest::builder().key("a").body("data".into()).build())
        .await
        .unwrap_err();
    assert_eq!(kind(&error), "unknown");
}
#[rstest::rstest]
#[case("<Contents><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>")]
#[case(
    "<Contents><Key>outside/a</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>"
)]
#[case("<Contents><Key>coord/z</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>")]
#[case("<IsTruncated>true</IsTruncated>")]
#[case("<IsTruncated>true</IsTruncated><NextContinuationToken></NextContinuationToken>")]
#[case("<IsTruncated>false</IsTruncated><NextContinuationToken>next</NextContinuationToken>")]
#[case("")]
#[tokio::test(start_paused = true)]
async fn malformed_list_cannot_become_a_fleet_count(#[case] contents: &str) {
    let mut fixture = response(200);
    fixture.body = format!("<ListBucketResult>{contents}</ListBucketResult>");
    let error = store(fixture)
        .await
        .list_page(ListRequest::builder().prefix("a").build())
        .await
        .unwrap_err();
    assert_eq!(kind(&error), "invalid");
}

#[rstest::rstest]
#[case("a2", "a1")]
#[case("a1", "a1")]
#[tokio::test(start_paused = true)]
async fn unordered_and_duplicate_list_entries_reach_the_census(
    #[case] first: &str,
    #[case] second: &str,
) {
    let prefix = "coord/_dial9_sampling/service=sampled/";
    let mut fixture = response(200);
    fixture.body = format!(
        "<ListBucketResult><IsTruncated>false</IsTruncated>\
         <Contents><Key>{prefix}{first}</Key></Contents>\
         <Contents><Key>{prefix}{second}</Key></Contents>\
         </ListBucketResult>"
    );
    let page = store(fixture)
        .await
        .list_page(ListRequest::builder().prefix("a").build())
        .await
        .unwrap();
    assert_eq!(
        page.objects()
            .iter()
            .map(ListedObject::key)
            .collect::<Vec<_>>(),
        [first, second]
    );
    assert!(page.continuation().is_none());
}

#[tokio::test(start_paused = true)]
async fn uncertain_put_is_not_hidden_by_a_retry_precondition_failure() {
    // The first attempt may commit while its acknowledgement is lost. A retry
    // would observe the changed version and return 412, hiding that commit.
    let fixture = RecordingFixture::new([response(500), response(412)]);
    let error = store_with_connector(fixture.clone(), 5)
        .await
        .put(
            PutRequest::builder()
                .key("v2/pools/pool/roster.json")
                .body("new-roster".into())
                .condition(WriteCondition::matching(VersionToken::new("\"old\"")))
                .build(),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&error), "unknown");
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    assert_eq!(fixture.responses.lock().unwrap().len(), 1);
}

#[rstest::rstest]
#[case(WriteCondition::absent(), None, Some("*"))]
#[case(
    WriteCondition::matching(VersionToken::new("\"old\"")),
    Some("\"old\""),
    None
)]
#[case(WriteCondition::unconditional(), None, None)]
#[tokio::test(start_paused = true)]
async fn requested_write_condition_is_preserved_on_the_wire(
    #[case] condition: WriteCondition,
    #[case] if_match: Option<&str>,
    #[case] if_none_match: Option<&str>,
) {
    let fixture = RecordingFixture::new([response(200)]);
    let key = match &condition {
        WriteCondition::Unconditional { .. } => "v2/members/pool/000001/process-run",
        _ => "v2/pools/pool/roster.json",
    };
    let result = store_with_connector(fixture.clone(), 5)
        .await
        .put(
            PutRequest::builder()
                .key(key)
                .body("roster".into())
                .condition(condition)
                .build(),
        )
        .await
        .unwrap();
    assert!(matches!(result, PutOutcome::Stored { .. }));
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].if_match.as_deref(), if_match);
    assert_eq!(requests[0].if_none_match.as_deref(), if_none_match);
}

#[tokio::test(start_paused = true)]
async fn uploader_config_isolates_services_and_prefixes_and_preserves_custom_client() {
    let fixture = RecordingFixture::new(std::iter::repeat_n(response(200), 6));
    let bootstrap = sdk_client(fixture.clone(), 1);
    let client = aws_sdk_s3::Client::from_conf(
        bootstrap
            .config()
            .to_builder()
            .endpoint_url("https://custom.test")
            .build(),
    );
    let namespaces = [
        ("checkout", None),
        ("checkout", Some("")),
        ("checkout", Some("traces")),
        ("checkout", Some("traces/")),
        ("billing", Some("traces")),
    ];
    for (service, prefix) in namespaces {
        let config = dial9_destinations_s3::S3Config::builder()
            .bucket("shared-traces")
            .service_name(service)
            .maybe_prefix(prefix.map(str::to_owned))
            .region("eu-west-1")
            // Individual trace layout does not change coordination keys.
            .key_fn(|_: &dial9_destinations_s3::KeyContext| "custom-trace-key".to_owned())
            .build();
        let store = S3SamplingStore::builder()
            .config(config)
            .client(client.clone())
            .build()
            .await
            .unwrap();
        store
            .put(
                PutRequest::builder()
                    .key("v3/pools/pool/roster.json")
                    .body("roster".into())
                    .build(),
            )
            .await
            .unwrap();
    }
    let membership_key = "v3/members/environment=production/01M623S6ZY8ND5FEG3ACCRG99H/0000000005977399/575c86b7dbf19363.a.01M623S6ZY8ND5FEG3ACCRG99J";
    let store = S3SamplingStore::builder()
        .config(
            dial9_destinations_s3::S3Config::builder()
                .bucket("shared-traces")
                .service_name("checkout")
                .region("eu-west-1")
                .build(),
        )
        .client(client)
        .build()
        .await
        .unwrap();
    store
        .put(
            PutRequest::builder()
                .key(membership_key)
                .body(bytes::Bytes::new())
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        6,
        "explicit regions need no discovery request"
    );
    for (request, (service, prefix)) in requests.iter().zip(namespaces) {
        let prefix = prefix.map_or_else(String::new, |prefix| format!("{prefix}/"));
        assert_eq!(
            request.uri.split('?').next().unwrap(),
            format!(
                "https://custom.test/shared-traces/{prefix}_dial9_sampling/service%3D{service}/v3/pools/pool/roster.json"
            )
        );
        assert!(
            request
                .authorization
                .as_ref()
                .unwrap()
                .contains("/eu-west-1/s3/aws4_request"),
            "configured region overrides the bootstrap client's region"
        );
    }
    assert_ne!(requests[0].uri, requests[1].uri);
    assert_ne!(requests[2].uri, requests[3].uri);
    assert_ne!(requests[2].uri, requests[4].uri);
    assert_eq!(
        requests[5].uri.split('?').next().unwrap(),
        format!(
            "https://custom.test/shared-traces/_dial9_sampling/service%3Dcheckout/{}",
            membership_key.replace('=', "%3D")
        )
    );
    assert!(requests[5].if_match.is_none());
    assert!(requests[5].if_none_match.is_none());
}

#[rstest::rstest]
#[case("checkout", "checkout")]
#[case("payments/api=50%", "payments%252Fapi%253D50%2525")]
#[case("payments%2Fapi", "payments%25252Fapi")]
#[case("東京", "%E6%9D%B1%E4%BA%AC")]
#[tokio::test(start_paused = true)]
async fn service_and_grouping_keys_use_hive_escaping_before_sdk_url_encoding(
    #[case] service: &str,
    #[case] service_uri: &str,
) {
    let fixture = RecordingFixture::new([response(200)]);
    let bootstrap = sdk_client(fixture.clone(), 1);
    let client = aws_sdk_s3::Client::from_conf(
        bootstrap
            .config()
            .to_builder()
            .endpoint_url("https://example.test")
            .build(),
    );
    let store = S3SamplingStore::builder()
        .config(
            dial9_destinations_s3::S3Config::builder()
                .bucket("traces")
                .service_name(service)
                .region("us-east-1")
                .build(),
        )
        .client(client)
        .build()
        .await
        .unwrap();
    store
        .put(
            PutRequest::builder()
                .key("v3/pools/a%2Fb=c%3Dd%25雪/roster.json")
                .body("roster".into())
                .build(),
        )
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].uri.split('?').next().unwrap(),
        format!(
            "https://example.test/traces/_dial9_sampling/service%3D{service_uri}/v3/pools/a%252Fb%3Dc%253Dd%2525%E9%9B%AA/roster.json"
        )
    );
}

#[rstest::rstest]
#[case(200)]
#[case(301)]
#[tokio::test(start_paused = true)]
async fn initialization_discovers_region_from_success_or_redirect(#[case] status: u16) {
    let mut discovery = response(status);
    discovery.bucket_region = Some("eu-west-1");
    let fixture = RecordingFixture::new([discovery, response(200)]);
    let config = dial9_destinations_s3::S3Config::builder()
        .bucket("traces")
        .service_name("checkout")
        .build();
    let store = S3SamplingStore::builder()
        .config(config)
        .client(sdk_client(fixture.clone(), 1))
        .build()
        .await
        .unwrap();
    store
        .put(PutRequest::builder().key("a").body("data".into()).build())
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "HEAD");
    assert!(
        requests[1]
            .authorization
            .as_ref()
            .unwrap()
            .contains("/eu-west-1/s3/aws4_request")
    );
}

#[rstest::rstest]
#[case(200)]
#[case(403)]
#[tokio::test(start_paused = true)]
async fn missing_region_is_an_initialization_error(#[case] status: u16) {
    let fixture = RecordingFixture::new([response(status)]);
    let error = S3SamplingStore::builder()
        .config(
            dial9_destinations_s3::S3Config::builder()
                .bucket("traces")
                .service_name("checkout")
                .build(),
        )
        .client(sdk_client(fixture.clone(), 1))
        .build()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[rstest::rstest]
#[case("", "checkout", "us-east-1")]
#[case("traces", " ", "us-east-1")]
#[case("traces", "checkout", "")]
#[tokio::test(start_paused = true)]
async fn invalid_configuration_does_not_issue_requests(
    #[case] bucket: &str,
    #[case] service: &str,
    #[case] region: &str,
) {
    let fixture = RecordingFixture::new([]);
    let error = S3SamplingStore::builder()
        .config(
            dial9_destinations_s3::S3Config::builder()
                .bucket(bucket)
                .service_name(service)
                .region(region)
                .build(),
        )
        .client(sdk_client(fixture.clone(), 1))
        .build()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(fixture.requests.lock().unwrap().is_empty());
}
