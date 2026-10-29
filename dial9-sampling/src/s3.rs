//! S3-backed implementation of  `SamplingStore`
use crate::backend::{
    ContinuationToken, ListPage, ListRequest, ListedObject, PutOutcome, PutRequest, SamplingStore,
    StoreError, StoreErrorKind, StoreFuture, StoredObject, VersionToken, WriteCondition,
};
use dial9_destinations_s3::S3Config;
use dial9_trace_format::object_key::hive_escape;
use std::{
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// TODO_REWRITE
///
/// Native S3 implementation of the sampler's conditional storage primitives.
///
/// Reuses the recorder's [`S3Config`], including its bucket, prefix and service.
/// Coordination keys live below `{prefix}/_dial9_sampling/service={escaped_service}/`;
/// configured services are isolated without a caller-supplied sampling scope.
/// Initialize inside [`crate::Sampling::with_storage`] so SDK resources belong
/// to the sampler's long-lived runtime.
///
/// Lifecycle and IAM rules must preserve shared rosters while permitting GET,
/// conditional PUT, sorted LIST and membership DELETE. General purpose buckets
/// provide the required listing order; directory buckets and their access points
/// are rejected because they do not.
#[derive(Clone, Debug)]
pub struct S3SamplingStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

#[bon::bon]
impl S3SamplingStore {
    /// TODO_REWRITE
    ///
    /// Initializes storage using the trace uploader's configuration.
    ///
    /// An optional SDK client preserves custom credential and endpoint setup.
    /// Construct it inside the storage future. Otherwise initialization uses
    /// the default AWS configuration chain and the config's attempt timeout.
    /// An explicit config region is used directly; otherwise `HeadBucket`
    /// discovers the region, including redirect response headers.
    ///
    /// # Errors
    ///
    /// Returns an error for blank bucket/service configuration, a directory
    /// bucket or directory-bucket access point, or when bucket region discovery
    /// fails or omits the region. The surrounding sampling startup budget bounds
    /// this asynchronous initialization.
    #[builder(finish_fn = build)]
    pub async fn builder(
        /// TODO_REWRITE
        config: S3Config,
        /// TODO_REWRITE
        client: Option<aws_sdk_s3::Client>,
    ) -> io::Result<Self> {
        if config.bucket().trim().is_empty() || config.service_name().trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 bucket and service_name must be nonblank",
            ));
        }
        if config.bucket().ends_with("--x-s3")
            || config.bucket().ends_with("--xa-s3")
            || config.bucket().split(':').nth(2) == Some("s3express")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sampling requires sorted S3 listings; directory buckets and their access points are unsupported",
            ));
        }
        if config
            .region()
            .is_some_and(|region| region.trim().is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 region must be nonblank when configured",
            ));
        }
        let bootstrap = match client {
            Some(client) => client,
            None => {
                let timeout = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_attempt_timeout(config.operation_attempt_timeout())
                    .build();
                let region = aws_config::meta::region::RegionProviderChain::first_try(
                    config
                        .region()
                        .map(|region| aws_sdk_s3::config::Region::new(region.to_owned())),
                )
                .or_default_provider()
                // The bootstrap endpoint is only used for region discovery.
                .or_else("us-east-1");
                let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(region)
                    .timeout_config(timeout)
                    .load()
                    .await;
                aws_sdk_s3::Client::new(&sdk)
            }
        };
        let region = match config.region() {
            Some(region) => region.to_owned(),
            None => bucket_region(&bootstrap, config.bucket()).await?,
        };
        let client = aws_sdk_s3::Client::from_conf(
            bootstrap
                .config()
                .to_builder()
                .region(aws_sdk_s3::config::Region::new(region))
                .build(),
        );
        let sampling_prefix = match config.prefix() {
            Some(prefix) => format!("{prefix}/_dial9_sampling/"),
            None => "_dial9_sampling/".to_owned(),
        };
        let service = hive_escape(config.service_name());
        Ok(Self {
            client,
            bucket: config.bucket().to_owned(),
            prefix: format!("{sampling_prefix}service={service}/"),
        })
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }
}

async fn bucket_region(client: &aws_sdk_s3::Client, bucket: &str) -> io::Result<String> {
    let response = client.head_bucket().bucket(bucket).send().await;
    let region = match &response {
        Ok(response) => response.bucket_region(),
        Err(error) => error
            .raw_response()
            .and_then(|response| response.headers().get("x-amz-bucket-region")),
    };
    match region.filter(|region| !region.trim().is_empty()) {
        Some(region) => Ok(region.to_owned()),
        None => Err(io::Error::other(match response {
            Ok(_) => "S3 HeadBucket omitted the bucket region".to_owned(),
            Err(error) => error.to_string(),
        })),
    }
}
fn invalid(message: &str) -> StoreError {
    StoreError::new(StoreErrorKind::invalid_response(), message.to_string())
}
fn timestamp(seconds: i64) -> Result<SystemTime, StoreError> {
    let seconds = u64::try_from(seconds).map_err(|_| invalid("negative S3 modification time"))?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| invalid("S3 modification time overflow"))
}
impl SamplingStore for S3SamplingStore {
    fn modification_time_uncertainty(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            let result = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(self.key(&key))
                .send()
                .await;
            let object = match result {
                Ok(object) => object,
                Err(error) => {
                    if error
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 404)
                    {
                        return Ok(None);
                    }
                    let kind = if error
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 403)
                    {
                        StoreErrorKind::unauthorized()
                    } else if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 400)
                    {
                        StoreErrorKind::invalid_response()
                    } else {
                        StoreErrorKind::unavailable()
                    };
                    return Err(StoreError::new(kind, error));
                }
            };
            let etag = object
                .e_tag()
                .filter(|etag| !etag.is_empty())
                .ok_or_else(|| invalid("S3 GET omitted a nonempty ETag"))?;
            let version = VersionToken::new(etag);
            let modified = timestamp(
                object
                    .last_modified()
                    .ok_or_else(|| invalid("S3 GET omitted LastModified"))?
                    .secs(),
            )?;
            let body = object
                .body
                .collect()
                .await
                .map_err(|error| StoreError::new(StoreErrorKind::unavailable(), error))?
                .into_bytes();
            Ok(Some(
                StoredObject::builder()
                    .body(body)
                    .version(version)
                    .modified_at(modified)
                    .build(),
            ))
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            let mut put = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(self.key(request.key()))
                .body(aws_sdk_s3::primitives::ByteStream::from(
                    request.body().clone(),
                ));
            put = match request.condition() {
                WriteCondition::Unconditional => put,
                WriteCondition::Absent => put.if_none_match("*"),
                WriteCondition::Matches { version, .. } => put.if_match(version.as_str()),
            };
            // A retry after a committed write with a lost response can return
            // 412. Reporting that as a definitive failed publication could hide
            // the new roster version. Surface each attempt for reconciliation.
            let retry = self
                .client
                .config()
                .retry_config()
                .cloned()
                .unwrap_or_else(aws_sdk_s3::config::retry::RetryConfig::standard)
                .with_max_attempts(1);
            match put
                .customize()
                .config_override(aws_sdk_s3::config::Builder::new().retry_config(retry))
                .send()
                .await
            {
                Ok(result) => Ok(PutOutcome::stored(VersionToken::new(
                    result
                        .e_tag()
                        .filter(|etag| !etag.is_empty())
                        .ok_or_else(|| {
                            StoreError::new(
                                StoreErrorKind::write_outcome_unknown(),
                                "S3 PUT succeeded without ETag; reconcile ownership",
                            )
                        })?,
                ))),
                Err(error) => {
                    let status = error
                        .raw_response()
                        .map(|response| response.status().as_u16());
                    match status {
                        Some(412 | 404) => Ok(PutOutcome::precondition_failed()),
                        Some(403) => Err(StoreError::new(StoreErrorKind::unauthorized(), error)),
                        _ => Err(StoreError::new(
                            StoreErrorKind::write_outcome_unknown(),
                            error,
                        )),
                    }
                }
            }
        })
    }
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            if request.page_size() == 0 {
                return Err(StoreError::new(
                    StoreErrorKind::unsupported(),
                    "LIST page size must be positive",
                ));
            }
            let page_size = request.page_size().min(1000);
            let result = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(self.key(request.prefix()))
                .max_keys(page_size as i32)
                .set_continuation_token(
                    request
                        .continuation()
                        .map(|token| token.as_str().to_string()),
                )
                .send()
                .await
                .map_err(|error| {
                    let kind = if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 403)
                    {
                        StoreErrorKind::unauthorized()
                    } else if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 400)
                    {
                        StoreErrorKind::invalid_response()
                    } else {
                        StoreErrorKind::unavailable()
                    };
                    StoreError::new(kind, error)
                })?;
            let mut objects = Vec::new();
            for object in result.contents() {
                let full = object.key().ok_or_else(|| invalid("S3 list omitted key"))?;
                let key = full
                    .strip_prefix(&self.prefix)
                    .ok_or_else(|| invalid("S3 list key outside sampling namespace"))?;
                if !key.starts_with(request.prefix()) {
                    return Err(invalid("S3 list key outside requested prefix"));
                }
                objects.push(ListedObject::builder().key(key).build());
            }
            if objects.len() > page_size {
                return Err(invalid("S3 list exceeded the requested page size"));
            }
            if objects
                .windows(2)
                .any(|pair| pair[0].key() >= pair[1].key())
            {
                return Err(invalid("S3 list keys were not strictly increasing"));
            }
            let truncated = result
                .is_truncated()
                .ok_or_else(|| invalid("S3 list omitted IsTruncated"))?;
            let continuation = match (truncated, result.next_continuation_token()) {
                (true, Some(token)) if !token.is_empty() => {
                    if request
                        .continuation()
                        .is_some_and(|previous| previous.as_str() == token)
                    {
                        return Err(invalid("S3 list repeated its continuation token"));
                    }
                    Some(ContinuationToken::new(token))
                }
                (true, _) => {
                    return Err(invalid(
                        "truncated S3 list omitted a nonempty continuation token",
                    ));
                }
                (false, Some(_)) => {
                    return Err(invalid("complete S3 list returned a continuation token"));
                }
                (false, None) => None,
            };
            Ok(ListPage::builder()
                .objects(objects)
                .maybe_continuation(continuation)
                .build())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(self.key(key))
                .send()
                .await
                .map_err(|error| {
                    let kind = if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 403)
                    {
                        StoreErrorKind::unauthorized()
                    } else {
                        StoreErrorKind::unavailable()
                    };
                    StoreError::new(kind, error)
                })?;
            Ok(())
        })
    }
}
