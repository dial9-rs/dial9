//! Native S3 conditional operations, independent of upload circuit breaking.
use crate::backend::{
    ContinuationToken, ListPage, ListRequest, ListedObject, PutOutcome, PutRequest, SamplingStore,
    StoreError, StoreErrorKind, StoreFuture, StoredObject, VersionToken, WriteCondition,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Native S3 implementation of the sampler's conditional storage primitives.
///
/// Supply an SDK client and bucket. The prefix defaults to `_dial9_sampling/`
/// and can be overridden; an explicit prefix is used verbatim and should end
/// in `/`. Its lifecycle and IAM rules must preserve live leases and shared
/// configuration while permitting GET, conditional PUT, LIST and membership
/// DELETE.
#[derive(Clone, Debug, bon::Builder)]
pub struct S3SamplingStore {
    client: aws_sdk_s3::Client,
    #[builder(into)]
    bucket: String,
    #[builder(default = "_dial9_sampling/".to_string(), into)]
    prefix: String,
}
impl S3SamplingStore {
    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
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
            let version = VersionToken::new(
                object
                    .e_tag()
                    .ok_or_else(|| invalid("S3 GET omitted ETag"))?,
            );
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
                WriteCondition::Unconditional { .. } => put,
                WriteCondition::Absent { .. } => put.if_none_match("*"),
                WriteCondition::Matches { version, .. } => put.if_match(version.as_str()),
            };
            // A retry after a committed write with a lost response can return
            // 412. Reporting that as a definitive failed claim would let the
            // engine probe another slot or skip cleanup of its own allocation.
            // Surface each attempt so the engine can reconcile unknown writes.
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
                    result.e_tag().ok_or_else(|| {
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
                        Some(409) => Ok(PutOutcome::retryable_conflict()),
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
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            let result = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(self.key(request.prefix()))
                .max_keys(request.page_size().clamp(1, 1000) as i32)
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
                if !full.starts_with(&self.key(request.prefix())) {
                    return Err(invalid("S3 list key outside requested prefix"));
                }
                let key = full
                    .strip_prefix(&self.prefix)
                    .ok_or_else(|| invalid("S3 list key outside sampling namespace"))?;
                if objects
                    .last()
                    .is_some_and(|previous: &ListedObject| previous.key() >= key)
                {
                    return Err(invalid("S3 list keys are not strictly ordered"));
                }
                let modified = timestamp(
                    object
                        .last_modified()
                        .ok_or_else(|| invalid("S3 list omitted LastModified"))?
                        .secs(),
                )?;
                objects.push(
                    ListedObject::builder()
                        .key(key)
                        .modified_at(modified)
                        .build(),
                );
            }
            if result.is_truncated() == Some(true) && result.next_continuation_token().is_none() {
                return Err(invalid("truncated S3 list omitted continuation token"));
            }
            Ok(ListPage::builder()
                .objects(objects)
                .maybe_continuation(result.next_continuation_token().map(ContinuationToken::new))
                .build())
        })
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(self.key(&key))
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
