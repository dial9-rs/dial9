use super::*;

/// Map an S3 SDK error to a [`StorageError`], collapsing all
/// authentication/authorization failures to [`StorageError::Unauthorized`] so
/// the secret, token, and access key id are never reflected to the client.
///
/// Uses the structured error code (via `ProvideErrorMetadata`) rather than
/// string matching, plus the HTTP status as a backstop.
fn classify_s3_error<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> StorageError
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
    R: std::fmt::Debug,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    match err.code() {
        Some(
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "ExpiredToken"
            | "ExpiredTokenException"
            | "InvalidToken"
            | "AccessDenied"
            | "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidClientTokenId"
            | "AuthorizationHeaderMalformed",
        ) => StorageError::Unauthorized,
        // Account-level: the credentials are valid but the account isn't signed
        // up for S3 in this region — typically the wrong identity signed it.
        Some("NotSignedUp" | "OptInRequired") => StorageError::AccountNotSignedUp,
        // Region mismatch: the bucket lives in another region than the client
        // was built for, so S3 refuses with `PermanentRedirect` (the classic
        // form) or the generic `Redirect`. Surface a clear message rather than
        // the opaque "unclassified S3 error" this used to fall through to.
        //
        // `IllegalLocationConstraintException` is the same mismatch reported
        // differently: an opt-in region (e.g. `af-south-1`) whose bucket is
        // addressed through a region-specific endpoint the request wasn't signed
        // for returns this (HTTP 400) instead of the 301 `PermanentRedirect`. It
        // is still "wrong region", so classify it alongside the others rather
        // than letting it fall through to the `Other` arm's 500 `fault`.
        Some("PermanentRedirect" | "Redirect" | "IllegalLocationConstraintException") => {
            StorageError::WrongRegion
        }
        // Missing bucket/key: the user pointed at something that does not exist
        // (typo'd bucket name, deleted object). This is a client mistake, so map
        // it to 404 rather than letting it fall through to the `Other` arm —
        // which logged an "unclassified S3 error" and returned a 500 `fault`,
        // polluting the fault metric with user input. The list/prefix paths hit
        // this via the bucket-level `NoSuchBucket`; `GetObject`'s `NoSuchKey` is
        // additionally handled at the call site (with the bucket/key in the
        // message), so plain code matching here is the fallback.
        Some("NoSuchBucket" | "NoSuchKey" | "NotFound") => {
            StorageError::NotFound("the specified bucket or object does not exist".to_string())
        }
        // Malformed bucket name: the name itself violates S3's naming rules (bad
        // characters, wrong length), so S3 rejects it with `InvalidBucketName`
        // (HTTP 400) before it can look anything up. Like `NoSuchBucket` this is
        // user input, not a server fault — but it's a *bad request*, not a
        // missing resource, so map it to 400 rather than 404 and don't let it
        // fall through to the `Other` arm's 500 `fault`.
        Some("InvalidBucketName") => {
            StorageError::BadRequest("the bucket name is not valid".to_string())
        }
        // Unmapped error: keep the full SDK detail in the server log (it can
        // embed the access key id, region, and endpoint — server-eyes only) and
        // hand the client a generic message rather than reflecting it back.
        _ => {
            tracing::warn!(
                error = %aws_sdk_s3::error::DisplayErrorContext(err),
                "unclassified S3 error"
            );
            StorageError::Other("could not complete the S3 request".to_string())
        }
    }
}

/// Optional plumbing for building ephemeral (bring-your-own-credentials) S3
/// clients. In production this is `None` and clients use the default HTTPS
/// connector. Tests inject the in-process `s3s` HTTP client plus an endpoint
/// override so the header → ephemeral-client → fake-S3 path is exercisable.
///
/// This is a test seam, not part of the public API surface — it is `pub` only
/// so integration tests in another crate can construct it.
#[doc(hidden)]
#[derive(Clone)]
pub struct EphemeralS3Config {
    /// Shared HTTP client/connector reused across ephemeral clients.
    pub http_client: aws_sdk_s3::config::SharedHttpClient,
    /// Endpoint override (test-only — never wired to user input; that would be
    /// an SSRF vector).
    pub endpoint_url: Option<String>,
    /// Path-style addressing — required by the `s3s` fake, never for real S3.
    pub force_path_style: bool,
}

/// Default region used when the user did not supply (and we could not detect)
/// one. S3 routes bucket operations regardless once the bucket region is known,
/// but a concrete region is required to build the client.
const DEFAULT_REGION: &str = "us-east-1";

/// Per-attempt timeout: how long a single HTTP attempt may take before the SDK
/// gives up on it (and possibly retries).
const OPERATION_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Overall operation timeout: the wall-clock budget for an entire S3 call,
/// including all retries. Bounds how long a request to a wrong region, a
/// black-holed endpoint, or unresponsive S3 can hang the viewer's request
/// handler.
const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// S3-backed storage using the AWS SDK.
pub struct S3Backend {
    client: aws_sdk_s3::Client,
}
impl S3Backend {
    pub async fn from_env() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self::from_client(aws_sdk_s3::Client::new(&config))
    }

    /// Create an ambient-credential client pinned to a request's bucket region.
    ///
    /// Deep links carry `aws_region` even when they rely on the server's ambient
    /// identity rather than explicit BYO credentials. The default SDK config may
    /// have no region at all, so reusing the process-wide backend would otherwise
    /// fail endpoint resolution before making an S3 request.
    pub async fn from_env_in_region(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region.to_string()))
            .load()
            .await;
        Self::from_client(aws_sdk_s3::Client::new(&config))
    }

    /// Create from an existing S3 client (useful for testing with s3s).
    pub fn from_client(client: aws_sdk_s3::Client) -> Self {
        Self { client }
    }

    async fn fetch_bucket_details(&self) -> Result<Vec<BucketInfo>, StorageError> {
        const MAX_BUCKETS: usize = 200;

        // S3 only includes BucketRegion when ListBuckets has at least one valid
        // parameter. Setting the page size both enables that field and keeps
        // pagination bounded by the viewer's existing display cap.
        let mut pages = self
            .client
            .list_buckets()
            .max_buckets(MAX_BUCKETS as i32)
            .into_paginator()
            .send();
        let mut buckets = Vec::new();
        let mut truncated = false;
        'pages: while let Some(page) = pages.next().await {
            let page = page.map_err(|e| classify_s3_error(&e))?;
            for bucket in page.buckets() {
                if let Some(name) = bucket.name() {
                    buckets.push(BucketInfo::new(
                        name,
                        bucket.bucket_region().map(str::to_string),
                    ));
                }
                if buckets.len() >= MAX_BUCKETS {
                    truncated = true;
                    break 'pages;
                }
            }
        }
        if truncated {
            tracing::warn!(
                max = MAX_BUCKETS,
                "bucket listing truncated at cap; some buckets are not shown"
            );
        }
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(buckets)
    }

    /// Build an ephemeral backend from user-supplied credentials.
    ///
    /// The credentials are passed as a concrete value, which acts as a *static*
    /// credential provider: it can never fall back to the server's IMDS/env
    /// identity. That is the core security property of bring-your-own-creds.
    pub fn from_credentials(
        credentials: aws_sdk_s3::config::Credentials,
        region: Option<&str>,
        ephemeral: &Option<EphemeralS3Config>,
    ) -> Self {
        Self::from_client(build_credentialed_client(credentials, region, ephemeral))
    }

    /// Paginate `ListObjectsV2` under `prefix`, accumulating up to `cap` objects
    /// and reporting whether more existed beyond the cap. The cap is a backstop
    /// against a prefix with unbounded fan-out producing an enormous response.
    async fn list_objects_paginated(
        &self,
        bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> Result<ListPage, StorageError> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .into_paginator()
            .send();

        let mut objects = Vec::new();
        let mut truncated = false;
        'pages: while let Some(page) = pages.next().await {
            let page = page.map_err(|e| classify_s3_error(&e))?;
            for obj in page.contents() {
                if objects.len() >= cap {
                    truncated = true;
                    break 'pages;
                }
                if let Some(key) = obj.key() {
                    objects.push(ObjectInfo {
                        key: key.to_string(),
                        size: obj.size().unwrap_or(0),
                        last_modified: obj.last_modified().map(|t| t.to_string()),
                    });
                }
            }
        }
        if truncated {
            tracing::warn!(
                bucket = %bucket,
                prefix = %prefix,
                cap,
                "object listing truncated at cap; some objects are not shown"
            );
        }

        Ok(ListPage { objects, truncated })
    }
}

/// Construct an `aws_sdk_s3::Client` from explicit credentials. Shared by the
/// ephemeral backend and the `/api/credentials/check` validation handler.
pub fn build_credentialed_client(
    credentials: aws_sdk_s3::config::Credentials,
    region: Option<&str>,
    ephemeral: &Option<EphemeralS3Config>,
) -> aws_sdk_s3::Client {
    let region = region.unwrap_or(DEFAULT_REGION).to_string();
    let timeouts = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
        .operation_attempt_timeout(OPERATION_ATTEMPT_TIMEOUT)
        .operation_timeout(OPERATION_TIMEOUT)
        .build();
    let mut cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .credentials_provider(credentials)
        .timeout_config(timeouts)
        .region(aws_sdk_s3::config::Region::new(region));

    if let Some(e) = ephemeral {
        cfg = cfg.http_client(e.http_client.clone());
        if let Some(url) = &e.endpoint_url {
            cfg = cfg.endpoint_url(url);
        }
        if e.force_path_style {
            cfg = cfg.force_path_style(true);
        }
    }

    aws_sdk_s3::Client::from_conf(cfg.build())
}
impl StorageBackend for S3Backend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<BucketInfo>, StorageError>> + Send + '_>> {
        Box::pin(self.fetch_bucket_details())
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> Pin<Box<dyn Future<Output = Result<ListPage, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move { self.list_objects_paginated(&bucket, &prefix, cap).await })
    }

    fn list_objects_all(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            // Same pagination loop as `list_objects`, but with NO cap: follow
            // continuation tokens to exhaustion so ingest sees every object.
            let mut objects = Vec::new();
            let mut continuation: Option<String> = None;

            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&prefix);
                if let Some(token) = continuation.take() {
                    req = req.continuation_token(token);
                }

                // Classify the error like the sibling listing methods do, so an
                // auth failure surfaces as `Unauthorized` rather than a generic
                // `Other` that callers can't distinguish from an empty listing.
                // This is what lets a caller (e.g. a future per-side
                // bring-your-own-credentials prompt) tell "needs different
                // credentials" apart from "empty window".
                let resp = req.send().await.map_err(|e| classify_s3_error(&e))?;

                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        objects.push(ObjectInfo {
                            key: key.to_string(),
                            size: obj.size().unwrap_or(0),
                            last_modified: obj.last_modified().map(|t| t.to_string()),
                        });
                    }
                }

                if resp.is_truncated() == Some(true) {
                    match resp.next_continuation_token() {
                        Some(token) => continuation = Some(token.to_string()),
                        None => {
                            // S3 says there's more but gave us no token to fetch
                            // it. Re-issuing the same request would loop forever,
                            // so stop here with what we have rather than spin.
                            tracing::warn!(
                                bucket = %bucket,
                                prefix = %prefix,
                                returned = objects.len(),
                                "list_objects_all: response truncated but no continuation token; \
                                 returning partial listing"
                            );
                            break;
                        }
                    }
                } else {
                    break;
                }
            }

            Ok(objects)
        })
    }

    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            // Bound the number of child prefixes returned, mirroring the caps on
            // the other listings, so a directory with an unbounded fan-out can't
            // produce an enormous response.
            const MAX_PREFIXES: usize = 1000;
            // Common prefixes count against MaxKeys per response, so a directory
            // with more than one page of children must be paginated or it would
            // silently truncate.
            let mut pages = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .delimiter("/")
                .into_paginator()
                .send();

            let mut prefixes = Vec::new();
            let mut truncated = false;
            'pages: while let Some(page) = pages.next().await {
                let page = page.map_err(|e| classify_s3_error(&e))?;
                for cp in page.common_prefixes() {
                    if let Some(p) = cp.prefix() {
                        prefixes.push(p.to_string());
                    }
                    if prefixes.len() >= MAX_PREFIXES {
                        truncated = true;
                        break 'pages;
                    }
                }
            }
            if truncated {
                tracing::warn!(
                    bucket = %bucket,
                    prefix = %prefix,
                    max = MAX_PREFIXES,
                    "prefix listing truncated at cap; some child prefixes are not shown"
                );
            }
            Ok(prefixes)
        })
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            // A single GET per object. Object-level concurrency comes from the
            // ingest work-queue (N workers each downloading one object), which
            // benchmarked ~2.4x faster than routing these ~37MB trace segments
            // through the transfer manager's multipart path (the part split +
            // reassembly overhead dominated, and a shared transfer-manager
            // instance throttled all workers to a flat wall time regardless of
            // worker count).
            let resp = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::operation::get_object::GetObjectError;

                    // Classify before unwrapping the service error so auth
                    // failures (which arrive as the redirect/4xx service error)
                    // collapse to Unauthorized rather than leaking the message.
                    let classified = classify_s3_error(&e);
                    match e.into_service_error() {
                        GetObjectError::NoSuchKey(_) => {
                            StorageError::NotFound(format!("{bucket}/{key}"))
                        }
                        _ => classified,
                    }
                })?;

            let bytes = resp
                .body
                .collect()
                .await
                .map_err(|e| StorageError::Other(e.to_string()))?;

            Ok(bytes.to_vec())
        })
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            self.client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(aws_sdk_s3::primitives::ByteStream::from(data))
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::error::DisplayErrorContext;
                    StorageError::Other(format!("{}", DisplayErrorContext(&e)))
                })?;
            Ok(())
        })
    }

    /// Stream the object body straight from S3 instead of buffering it. The
    /// `GetObject` request (and thus NoSuchKey / auth / not-found classification)
    /// still completes synchronously in the returned future, so the HTTP layer
    /// gets the right status before any body streams. The body is then handed
    /// back as a chunk stream — we do NOT call `.collect()`.
    fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectStream, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            let resp = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::operation::get_object::GetObjectError;

                    // Classify before unwrapping the service error so auth
                    // failures (which arrive as the redirect/4xx service error)
                    // collapse to Unauthorized rather than leaking the message.
                    let classified = classify_s3_error(&e);
                    match e.into_service_error() {
                        GetObjectError::NoSuchKey(_) => {
                            StorageError::NotFound(format!("{bucket}/{key}"))
                        }
                        _ => classified,
                    }
                })?;

            let content_length = resp.content_length();

            // Drive the body via the always-public `ByteStream::next()` (the
            // `Stream` impl on `ByteStream` itself is feature-gated/private in
            // this SDK). `unfold` yields one chunk per poll, mapping the SDK
            // chunk error into `io::Error` so the stream composes with
            // `Body::from_stream`. No `.collect()` — bytes flow as they arrive.
            let stream = futures::stream::unfold(resp.body, |mut body| async move {
                match body.next().await {
                    Some(Ok(chunk)) => Some((Ok(chunk), body)),
                    Some(Err(e)) => Some((Err(std::io::Error::other(e)), body)),
                    None => None,
                }
            });

            Ok(ObjectStream {
                stream: Box::pin(stream),
                content_length,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `S3Backend` whose HTTP layer replays the given canned responses
    /// in order, so multi-page pagination can be tested without a live S3 (the
    /// `s3s-fs` fake never emits a continuation token, so it can't drive page 2).
    fn replay_backend(
        responses: Vec<&str>,
    ) -> (
        S3Backend,
        aws_smithy_http_client::test_util::StaticReplayClient,
    ) {
        use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
        use aws_smithy_types::body::SdkBody;

        let events = responses
            .into_iter()
            .map(|body| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://s3.amazonaws.com/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(200)
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect();
        let http_client = StaticReplayClient::new(events);
        let cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http_client.clone())
            .build();
        (
            S3Backend::from_client(aws_sdk_s3::Client::from_conf(cfg)),
            http_client,
        )
    }
    /// Build an `S3Backend` whose HTTP layer replays a single error response
    /// (status + body) for the next request, so the error-classification path
    /// can be exercised without a live S3.
    fn replay_error_backend(status: u16, body: &str) -> S3Backend {
        use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
        use aws_smithy_types::body::SdkBody;

        let http_client = StaticReplayClient::new(vec![ReplayEvent::new(
            http::Request::builder()
                .uri("https://s3.amazonaws.com/")
                .body(SdkBody::empty())
                .unwrap(),
            http::Response::builder()
                .status(status)
                .body(SdkBody::from(body))
                .unwrap(),
        )]);
        let cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http_client)
            .build();
        S3Backend::from_client(aws_sdk_s3::Client::from_conf(cfg))
    }
    #[tokio::test]
    async fn list_buckets_requests_and_preserves_regions() {
        use aws_smithy_http_client::test_util::infallible_client_fn;

        let http_client = infallible_client_fn(|req: http::Request<_>| {
            assert!(
                req.uri()
                    .query()
                    .is_some_and(|query| query.contains("max-buckets=200")),
                "ListBuckets must include a valid parameter so S3 returns BucketRegion: {}",
                req.uri()
            );
            let body = r#"<?xml version="1.0" encoding="UTF-8"?>
                <ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
                  <Buckets>
                    <Bucket>
                      <Name>dial9-cape-town</Name>
                      <CreationDate>2026-07-21T00:00:00.000Z</CreationDate>
                      <BucketRegion>af-south-1</BucketRegion>
                    </Bucket>
                    <Bucket>
                      <Name>dial9-oregon</Name>
                      <CreationDate>2026-07-21T00:00:00.000Z</CreationDate>
                      <BucketRegion>us-west-2</BucketRegion>
                    </Bucket>
                  </Buckets>
                </ListAllMyBucketsResult>"#;
            http::Response::builder()
                .status(200)
                .header("content-type", "application/xml")
                .body(body)
                .unwrap()
        });
        let cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http_client)
            .build();
        let backend = S3Backend::from_client(aws_sdk_s3::Client::from_conf(cfg));

        let buckets = backend.list_buckets().await.unwrap();
        assert_eq!(
            buckets,
            vec![
                BucketInfo::new("dial9-cape-town", Some("af-south-1".to_string())),
                BucketInfo::new("dial9-oregon", Some("us-west-2".to_string())),
            ]
        );
    }
    /// A `PermanentRedirect` (the error S3 returns when a bucket is addressed in
    /// the wrong region) must classify to [`StorageError::WrongRegion`], not the
    /// opaque `Other` that produced the "unclassified S3 error" log. This is the
    /// regression guard for the cross-region bucket bug.
    #[tokio::test]
    async fn permanent_redirect_classifies_as_wrong_region() {
        // The XML S3 sends on a region mismatch (HTTP 301 + PermanentRedirect).
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error>
              <Code>PermanentRedirect</Code>
              <Message>The bucket you are attempting to access must be addressed using the specified endpoint.</Message>
              <Endpoint>my-bucket.s3.us-west-2.amazonaws.com</Endpoint>
            </Error>"#;
        let backend = replay_error_backend(301, body);

        let err = backend
            .list_prefixes("my-bucket", "")
            .await
            .expect_err("a PermanentRedirect must surface as an error");
        assert!(
            matches!(err, StorageError::WrongRegion),
            "expected WrongRegion, got {err:?}"
        );
        // The message points the user at the fix (set/detect the region).
        assert!(err.to_string().contains("region"), "message: {err}");
    }
    /// An `IllegalLocationConstraintException` is a region mismatch reported
    /// differently: an opt-in region (e.g. `af-south-1`) whose bucket is
    /// addressed through an endpoint the request wasn't signed for returns this
    /// (HTTP 400) instead of a 301 `PermanentRedirect`. It must classify to
    /// [`StorageError::WrongRegion`] like the redirect forms, not the opaque
    /// `Other` that produced the "unclassified S3 error" log and a 500 `fault`.
    #[tokio::test]
    async fn illegal_location_constraint_classifies_as_wrong_region() {
        // The XML S3 sends for an opt-in-region bucket hit through the wrong
        // regional endpoint (HTTP 400) — the exact shape seen in prod.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error>
              <Code>IllegalLocationConstraintException</Code>
              <Message>The af-south-1 location constraint is incompatible for the region specific endpoint this request was sent to.</Message>
            </Error>"#;
        let backend = replay_error_backend(400, body);

        let err = backend
            .list_prefixes("my-bucket", "")
            .await
            .expect_err("an IllegalLocationConstraintException must surface as an error");
        assert!(
            matches!(err, StorageError::WrongRegion),
            "expected WrongRegion, got {err:?}"
        );
        // The message points the user at the fix (set/detect the region).
        assert!(err.to_string().contains("region"), "message: {err}");
    }
    /// A `NoSuchBucket` (the error S3 returns when the bucket name does not
    /// exist — typically a user typo) must classify to [`StorageError::NotFound`]
    /// (→ HTTP 404), not the opaque `Other` that produced an "unclassified S3
    /// error" log and a 500 `fault`. Guards against user input polluting the
    /// server-fault metric.
    #[tokio::test]
    async fn no_such_bucket_classifies_as_not_found() {
        // The XML S3 sends when the bucket does not exist (HTTP 404).
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error>
              <Code>NoSuchBucket</Code>
              <Message>The specified bucket does not exist</Message>
              <BucketName>test-shanks</BucketName>
            </Error>"#;
        let backend = replay_error_backend(404, body);

        let err = backend
            .list_prefixes("test-shanks", "")
            .await
            .expect_err("a NoSuchBucket must surface as an error");
        assert!(
            matches!(err, StorageError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }
    /// An `InvalidBucketName` (S3's error for a syntactically invalid bucket
    /// name, HTTP 400) must classify to [`StorageError::BadRequest`] (→ HTTP
    /// 400), not the opaque `Other` that produced an "unclassified S3 error"
    /// log and a 500 `fault`. Like `NoSuchBucket` it's user input, but it's a
    /// malformed request rather than a missing resource.
    #[tokio::test]
    async fn invalid_bucket_name_classifies_as_bad_request() {
        // The XML S3 sends when the bucket name violates the naming rules.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error>
              <Code>InvalidBucketName</Code>
              <Message>The specified bucket is not valid.</Message>
              <BucketName>Not_A_Valid_Bucket</BucketName>
            </Error>"#;
        let backend = replay_error_backend(400, body);

        let err = backend
            .list_prefixes("Not_A_Valid_Bucket", "")
            .await
            .expect_err("an InvalidBucketName must surface as an error");
        assert!(
            matches!(err, StorageError::BadRequest(_)),
            "expected BadRequest, got {err:?}"
        );
    }
    #[tokio::test]
    async fn list_prefixes_follows_continuation_token() {
        // Page 1 is truncated and carries a NextContinuationToken; page 2 is the
        // final page. The fix must follow the token and merge both pages — the
        // old single-shot code would drop `c/`.
        let page1 = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>bucket</Name><Prefix></Prefix><Delimiter>/</Delimiter>
              <IsTruncated>true</IsTruncated>
              <NextContinuationToken>TOKEN_A</NextContinuationToken>
              <CommonPrefixes><Prefix>a/</Prefix></CommonPrefixes>
              <CommonPrefixes><Prefix>b/</Prefix></CommonPrefixes>
            </ListBucketResult>"#;
        let page2 = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>bucket</Name><Prefix></Prefix><Delimiter>/</Delimiter>
              <IsTruncated>false</IsTruncated>
              <CommonPrefixes><Prefix>c/</Prefix></CommonPrefixes>
            </ListBucketResult>"#;

        let (backend, http_client) = replay_backend(vec![page1, page2]);
        let prefixes = backend.list_prefixes("bucket", "").await.unwrap();
        assert_eq!(prefixes, vec!["a/", "b/", "c/"]);

        // Two HTTP calls were made, and the second carried the continuation token
        // from the first — proving pagination actually happened.
        let requests = http_client.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 2, "expected two list calls");
        assert!(
            requests[1].uri().contains("continuation-token=TOKEN_A"),
            "second request must carry the continuation token, got: {}",
            requests[1].uri()
        );
    }

    /// Build a backend that replays a single response with an explicit status
    /// and body — used to drive the SDK's error path (the success-only
    /// `replay_backend` always returns 200).
    fn replay_backend_status(status: u16, body: &str) -> S3Backend {
        use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
        use aws_smithy_types::body::SdkBody;
        let events = vec![ReplayEvent::new(
            http::Request::builder()
                .uri("https://s3.amazonaws.com/")
                .body(SdkBody::empty())
                .unwrap(),
            http::Response::builder()
                .status(status)
                .body(SdkBody::from(body))
                .unwrap(),
        )];
        let http_client = StaticReplayClient::new(events);
        let cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http_client)
            .build();
        S3Backend::from_client(aws_sdk_s3::Client::from_conf(cfg))
    }
    #[tokio::test]
    async fn list_objects_all_maps_auth_failure_to_unauthorized() {
        // S3 returns 403 InvalidAccessKeyId when the credentials can't read the
        // bucket. `list_objects_all` must classify this as `Unauthorized` (not
        // the generic `Other`), consistent with the sibling listing methods, so
        // callers can distinguish "needs different credentials" from "empty
        // window".
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Error><Code>InvalidAccessKeyId</Code>
            <Message>The AWS Access Key Id you provided does not exist in our records.</Message>
            </Error>"#;
        let backend = replay_backend_status(403, body);
        let err = backend
            .list_objects_all("bucket", "prefix")
            .await
            .expect_err("auth failure must be an error");
        assert!(
            matches!(err, StorageError::Unauthorized),
            "expected Unauthorized, got {err:?}"
        );
    }
}
