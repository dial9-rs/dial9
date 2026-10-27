#![cfg(feature = "s3")]
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

#[derive(Debug, Clone)]
struct ResponseFixture {
    status: u16,
    body: String,
    etag: bool,
    modified: Option<&'static str>,
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
        if self.etag {
            response.headers_mut().insert("etag", "\"fixture\"");
        }
        if let Some(modified) = self.modified {
            response.headers_mut().insert("last-modified", modified);
        }
        HttpConnectorFuture::new(async move { Ok(response) })
    }
}
fn store(fixture: ResponseFixture) -> S3SamplingStore {
    let http = http_client_fn(move |_, _| SharedHttpConnector::new(fixture.clone()));
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1))
            .http_client(http)
            .force_path_style(true)
            .build(),
    );
    S3SamplingStore::builder()
        .client(client)
        .bucket("traces")
        .prefix("coord/")
        .build()
}
fn response(status: u16) -> ResponseFixture {
    ResponseFixture {
        status,
        body: String::new(),
        etag: true,
        modified: Some("Sun, 01 Jan 2023 00:00:00 GMT"),
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
    let store = store(response(status));
    let error = match operation {
        "get" => store.get("a".into()).await.unwrap_err(),
        "list" => store
            .list(ListRequest::builder().prefix("a").build())
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
        .put(PutRequest::builder().key("a").body("data".into()).build())
        .await
        .unwrap();
    assert_eq!(
        matches!(result, PutOutcome::RetryableConflict { .. }),
        uncertain
    );
    assert_eq!(
        matches!(result, PutOutcome::PreconditionFailed { .. }),
        !uncertain
    );
}
#[rstest::rstest]
#[case("etag", "invalid")]
#[case("modified", "invalid")]
#[case("negative-time", "invalid")]
#[case("body", "unavailable")]
#[tokio::test(start_paused = true)]
async fn malformed_get_is_not_a_missing_object(#[case] field: &str, #[case] expected: &str) {
    let mut fixture = response(200);
    match field {
        "etag" => fixture.etag = false,
        "modified" => fixture.modified = None,
        "negative-time" => fixture.modified = Some("Fri, 01 Jan 1960 00:00:00 GMT"),
        "body" => fixture.broken_body = true,
        _ => panic!("unknown field"),
    }
    assert_eq!(
        kind(&store(fixture).get("a".into()).await.unwrap_err()),
        expected
    );
}
#[tokio::test(start_paused = true)]
async fn missing_object_and_missing_put_ack_are_distinct() {
    assert!(
        store(response(404))
            .get("a".into())
            .await
            .unwrap()
            .is_none()
    );
    let mut fixture = response(200);
    fixture.etag = false;
    let error = store(fixture)
        .put(PutRequest::builder().key("a").body("data".into()).build())
        .await
        .unwrap_err();
    assert_eq!(kind(&error), "unknown");
}
#[rstest::rstest]
#[case("<Contents><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>")]
#[case("<Contents><Key>coord/a</Key></Contents>")]
#[case(
    "<Contents><Key>outside/a</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>"
)]
#[case("<Contents><Key>coord/z</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>")]
#[case("<IsTruncated>true</IsTruncated>")]
#[case(
    "<Contents><Key>coord/a2</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents><Contents><Key>coord/a1</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>"
)]
#[case(
    "<Contents><Key>coord/a1</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents><Contents><Key>coord/a1</Key><LastModified>2023-01-01T00:00:00Z</LastModified></Contents>"
)]
#[tokio::test(start_paused = true)]
async fn malformed_list_cannot_become_a_fleet_count(#[case] contents: &str) {
    let mut fixture = response(200);
    fixture.body = format!("<ListBucketResult>{contents}</ListBucketResult>");
    let error = store(fixture)
        .list(ListRequest::builder().prefix("a").build())
        .await
        .unwrap_err();
    assert_eq!(kind(&error), "invalid");
}
