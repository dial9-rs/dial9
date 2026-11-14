//! Keeps generic source routes out of the server-owned aggregate cache.
//!
//! When aggregate output persists to an S3 bucket, a caller could name that
//! bucket as a source and read or list cached part-files through `/api/object`,
//! `/api/prefixes`, `/api/browse`, or `/api/services`, bypassing the account
//! namespace. [`CacheGuard`] wraps every request source backend and refuses keys
//! and listing prefixes in the protected subtrees, `{output_prefix}/account=…`
//! and the legacy unscoped `{output_prefix}/v{N}`, in every bucket. Matching
//! ignores the bucket name because an S3 access point ARN or alias can name the
//! output bucket under another spelling. Other keys remain readable, so a
//! bucket that also holds raw traces keeps serving them.

use crate::storage::{
    BucketInfo, ListPage, ObjectInfo, ObjectStream, StorageBackend, StorageError,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StorageError>> + Send + 'a>>;

/// The protected aggregate subtrees under one output prefix, reserved in
/// every source bucket.
#[derive(Clone, Debug)]
pub(crate) struct ProtectedCache {
    /// The output prefix's path segments, without empty or `.` segments.
    base: Vec<String>,
}

impl ProtectedCache {
    pub(crate) fn new(output_prefix: &str) -> Self {
        Self {
            base: segments(output_prefix)
                .map(|segments| segments.into_iter().map(str::to_string).collect())
                // An operator prefix containing `..` cannot name a cache root.
                .unwrap_or_default(),
        }
    }

    /// True when `key`, an object key or listing prefix in any bucket, is at or
    /// under a protected subtree. Matching ignores empty and `.` segments, and
    /// any `..` segment counts as protected, so path-normalizing backends cannot
    /// reach the cache through an alias.
    pub(crate) fn protects(&self, key: &str) -> bool {
        let Some(segments) = segments(key) else {
            return true;
        };
        segments.len() > self.base.len()
            && segments.iter().zip(&self.base).all(|(s, b)| s == b)
            && is_cache_root(segments[self.base.len()])
    }
}

/// `/`-separated segments without empty or `.` ones; `None` when any is `..`.
fn segments(key: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = key
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    (!segments.contains(&"..")).then_some(segments)
}

/// An account namespace (`account=…`, including a partial `account=` listing
/// prefix) or a legacy unscoped format generation (`v{N}`).
fn is_cache_root(segment: &str) -> bool {
    segment.starts_with("account=")
        || segment
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Indistinguishable from a missing object, so a refusal discloses nothing.
fn not_found(bucket: &str, key: &str) -> StorageError {
    StorageError::NotFound(format!("{bucket}/{key}"))
}

/// A source backend that refuses the protected aggregate cache. Reads of a
/// protected key fail as not found before reaching `inner`; listings at or
/// under a protected prefix are refused, and listings above one drop protected
/// entries.
pub(crate) struct CacheGuard {
    inner: Arc<dyn StorageBackend>,
    protected: ProtectedCache,
}

impl CacheGuard {
    pub(crate) fn wrap(
        inner: Arc<dyn StorageBackend>,
        protected: ProtectedCache,
    ) -> Arc<dyn StorageBackend> {
        Arc::new(Self { inner, protected })
    }

    fn check(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        if self.protected.protects(key) {
            tracing::debug!(%bucket, "refusing a source read of the aggregate cache");
            return Err(not_found(bucket, key));
        }
        Ok(())
    }

    fn retain_objects(&self, objects: &mut Vec<ObjectInfo>) {
        objects.retain(|object| !self.protected.protects(&object.key));
    }
}

impl StorageBackend for CacheGuard {
    fn list_buckets(&self) -> BoxFuture<'_, Vec<BucketInfo>> {
        self.inner.list_buckets()
    }

    fn list_objects(&self, bucket: &str, prefix: &str, cap: usize) -> BoxFuture<'_, ListPage> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            self.check(&bucket, &prefix)?;
            let mut page = self.inner.list_objects(&bucket, &prefix, cap).await?;
            self.retain_objects(&mut page.objects);
            Ok(page)
        })
    }

    fn list_objects_all(&self, bucket: &str, prefix: &str) -> BoxFuture<'_, Vec<ObjectInfo>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            self.check(&bucket, &prefix)?;
            let mut objects = self.inner.list_objects_all(&bucket, &prefix).await?;
            self.retain_objects(&mut objects);
            Ok(objects)
        })
    }

    fn list_prefixes(&self, bucket: &str, prefix: &str) -> BoxFuture<'_, Vec<String>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            self.check(&bucket, &prefix)?;
            let mut prefixes = self.inner.list_prefixes(&bucket, &prefix).await?;
            prefixes.retain(|child| !self.protected.protects(child));
            Ok(prefixes)
        })
    }

    fn get_object(&self, bucket: &str, key: &str) -> BoxFuture<'_, Vec<u8>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            self.check(&bucket, &key)?;
            self.inner.get_object(&bucket, &key).await
        })
    }

    fn put_object(&self, bucket: &str, key: &str, data: Vec<u8>) -> BoxFuture<'_, ()> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            self.check(&bucket, &key)?;
            self.inner.put_object(&bucket, &key, data).await
        })
    }

    fn get_object_stream(&self, bucket: &str, key: &str) -> BoxFuture<'_, ObjectStream> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            self.check(&bucket, &key)?;
            self.inner.get_object_stream(&bucket, &key).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::namespace("flamegraph-data/account=aws.111111111111/v11/x.parquet", true)]
    #[case::partial_namespace("flamegraph-data/account=", true)]
    #[case::legacy("flamegraph-data/v11/bucket=src/samples/x.parquet", true)]
    #[case::double_slash("flamegraph-data//account=aws.1/x", true)]
    #[case::dot_segment("./flamegraph-data/./account=aws.1/x", true)]
    #[case::leading_slash("/flamegraph-data/v11/x", true)]
    #[case::parent_segment("traces/../flamegraph-data/account=aws.1/x", true)]
    #[case::base("flamegraph-data/", false)]
    #[case::ancestor("", false)]
    #[case::partial_ancestor("flamegraph-data/acc", false)]
    #[case::bare_v("flamegraph-data/v", false)]
    #[case::non_digit_version("flamegraph-data/vx/x", false)]
    #[case::other_root("traces/account=aws.1/v11/x.bin.gz", false)]
    #[case::raw_source("2026-04-09/2100/svc/host/1-0.bin.gz", false)]
    fn protects_cache_subtrees(#[case] key: &str, #[case] protected: bool) {
        let cache = ProtectedCache::new("flamegraph-data");
        assert_eq!(cache.protects(key), protected, "{key}");
    }

    #[rstest::rstest]
    #[case::empty_prefix("")]
    #[case::trailing_slash("/")]
    fn root_output_prefix_protects_top_level_roots(#[case] prefix: &str) {
        let cache = ProtectedCache::new(prefix);
        assert!(cache.protects("account=aws.1/v11/x"));
        assert!(cache.protects("v11/x"));
        assert!(!cache.protects("2026-04-09/x.bin.gz"));
    }
}
