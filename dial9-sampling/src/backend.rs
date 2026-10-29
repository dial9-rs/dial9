//! TODO_REWRITE
//!
//! Storage primitives for implementing a custom sampling backend.
//! A failed or cancelled write may have committed.
use bytes::Bytes;
use std::{future::Future, pin::Pin, time::SystemTime};

/// TODO_REWRITE
///
/// A boxed, sendable storage operation; it borrows the store for its lifetime.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// TODO_REWRITE
///
/// Strong per-key reads and atomic conditional writes are required.
///
/// Keys are relative to this store's coordination namespace. Implementations
/// sharing a physical backend must isolate unrelated services themselves.
/// Sampling dimensions further group processes within that namespace.
///
/// Lists include completed writes, but neither page order nor an atomic fleet
/// snapshot is required. Continuation cursors must permit complete enumeration.
/// Versions identify the observed body and may repeat if the body is repeated;
/// shared roster revisions therefore include a fresh nonce on every write.
/// Cancellation never proves that a write did not commit. Delete is only for
/// unique process membership; shared rosters are never deleted.
/// A precondition failure must mean no attempt in that call committed.
/// Backends must not hide an uncertain earlier attempt behind a retry's
/// precondition failure; return an unknown write outcome for reconciliation.
pub trait SamplingStore: Send + Sync + 'static {
    /// TODO_REWRITE
    ///
    /// Maximum amount by which a returned modification time precedes the
    /// actual write time, in addition to the configured clock-skew allowance.
    /// Exact-time backends can use zero; second-resolution timestamps need 1s.
    fn modification_time_uncertainty(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    /// TODO_REWRITE
    ///
    /// Reads an object's current contents and version, or returns `None` if absent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the object cannot be read or its contents,
    /// version or modification time cannot satisfy the storage contract.
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>>;
    /// TODO_REWRITE
    ///
    /// Atomically writes under the requested condition; uncertainty requires reconciliation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the write fails. An uncertain commit must use
    /// [`StoreErrorKind::WriteOutcomeUnknown`]; a definitive precondition failure
    /// is a [`PutOutcome::PreconditionFailed`], rather than an error.
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome>;
    /// TODO_REWRITE
    ///
    /// Lists one page of keys under the requested prefix, including completed writes.
    ///
    /// Keys are relative to the store namespace and must match the requested prefix.
    /// Order is unspecified within and across pages; duplicate keys may appear.
    /// Continue with each opaque cursor until [`ListPage::continuation`] returns
    /// `None` to enumerate the complete listing. A failed or cyclic continuation
    /// leaves an incomplete listing and must not be used as a complete census.
    /// Listing does not provide an atomic snapshot of concurrent fleet changes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if listing fails or the response cannot satisfy
    /// the namespace or complete-pagination contract.
    fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage>;
    /// TODO_REWRITE
    ///
    /// Deletes a process's unique membership object; deleting an absent object succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the deletion fails. Cancellation or an error
    /// does not prove that the object still exists.
    fn delete(&self, key: String) -> StoreFuture<'_, ()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// TODO_REWRITE
///
/// Opaque object version used for conditional writes; callers must not interpret it.
pub struct VersionToken(String);
impl VersionToken {
    /// TODO_REWRITE
    ///
    /// Wraps the backend's opaque object-version token.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// TODO_REWRITE
    ///
    /// Returns the backend-provided opaque token string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Debug)]
/// TODO_REWRITE
///
/// Opaque backend cursor for continuing a listing.
pub struct ContinuationToken(String);
impl ContinuationToken {
    /// TODO_REWRITE
    ///
    /// Wraps the backend's opaque listing cursor.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// TODO_REWRITE
    ///
    /// Returns the backend-provided opaque token string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[non_exhaustive]
#[derive(Clone, Debug)]
/// TODO_REWRITE
///
/// The atomic precondition required for a write.
pub enum WriteCondition {
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// Writes regardless of the current object version.
    Unconditional {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// Writes only if the object is absent.
    Absent {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// Writes only if the current version matches.
    Matches {
        /// TODO_REWRITE
        ///
        /// Version that must still identify the current object.
        version: VersionToken,
    },
}
impl WriteCondition {
    /// TODO_REWRITE
    ///
    /// Allows replacing any value; shared roster writes use stricter conditions.
    pub fn unconditional() -> Self {
        Self::Unconditional {}
    }
    /// TODO_REWRITE
    ///
    /// Allows writing only when the object does not exist.
    pub fn absent() -> Self {
        Self::Absent {}
    }
    /// TODO_REWRITE
    ///
    /// Allows writing only while the supplied object version still matches.
    pub fn matching(version: VersionToken) -> Self {
        Self::Matches { version }
    }
}

#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// Object key, body and conditional-write requirement.
pub struct PutRequest {
    /// TODO_REWRITE
    #[builder(into)]
    key: String,
    /// TODO_REWRITE
    body: Bytes,
    /// TODO_REWRITE
    #[builder(default = WriteCondition::absent())]
    condition: WriteCondition,
}
impl PutRequest {
    /// TODO_REWRITE
    ///
    /// Returns the key relative to the backend namespace.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// TODO_REWRITE
    ///
    /// Returns the stored or requested object bytes.
    pub fn body(&self) -> &Bytes {
        &self.body
    }
    /// TODO_REWRITE
    ///
    /// Returns the atomic precondition for this write.
    pub fn condition(&self) -> &WriteCondition {
        &self.condition
    }
}
#[non_exhaustive]
#[derive(Clone, Debug)]
/// TODO_REWRITE
///
/// A write result that distinguishes definitive failure from uncertainty.
pub enum PutOutcome {
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// The write succeeded and returned its new version.
    Stored {
        /// TODO_REWRITE
        ///
        /// New object version returned by the completed write.
        version: VersionToken,
    },
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// Every attempt definitively failed its precondition without changing the object.
    PreconditionFailed {},
}
impl PutOutcome {
    /// TODO_REWRITE
    ///
    /// Constructs a successful write result with the new version.
    pub fn stored(version: VersionToken) -> Self {
        Self::Stored { version }
    }
    /// TODO_REWRITE
    ///
    /// Constructs a definitive failure: no attempt in this call changed the object.
    pub fn precondition_failed() -> Self {
        Self::PreconditionFailed {}
    }
}

#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// Object contents, conditional-write version and server modification time.
pub struct StoredObject {
    /// TODO_REWRITE
    body: Bytes,
    /// TODO_REWRITE
    version: VersionToken,
    /// TODO_REWRITE
    modified_at: SystemTime,
}
impl StoredObject {
    /// TODO_REWRITE
    ///
    /// Returns the stored or requested object bytes.
    pub fn body(&self) -> &Bytes {
        &self.body
    }
    /// TODO_REWRITE
    ///
    /// Returns the opaque version required to conditionally replace the observed object.
    pub fn version(&self) -> &VersionToken {
        &self.version
    }
    /// TODO_REWRITE
    ///
    /// Returns the server modification time; consumers must account for backend timestamp uncertainty.
    pub fn modified_at(&self) -> SystemTime {
        self.modified_at
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// Prefix, page size and optional continuation cursor for a listing.
pub struct ListRequest {
    /// TODO_REWRITE
    #[builder(into)]
    prefix: String,
    /// TODO_REWRITE
    #[builder(default = 256)]
    page_size: usize,
    /// TODO_REWRITE
    continuation: Option<ContinuationToken>,
}
impl ListRequest {
    /// TODO_REWRITE
    ///
    /// Returns the relative key prefix to list.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    /// TODO_REWRITE
    ///
    /// Returns the requested maximum number of objects in one page.
    pub fn page_size(&self) -> usize {
        self.page_size
    }
    /// TODO_REWRITE
    ///
    /// Returns the opaque cursor for continuing this listing, if present.
    pub fn continuation(&self) -> Option<&ContinuationToken> {
        self.continuation.as_ref()
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// Object key returned by listing.
pub struct ListedObject {
    /// TODO_REWRITE
    #[builder(into)]
    key: String,
}
impl ListedObject {
    /// TODO_REWRITE
    ///
    /// Returns the key relative to the backend namespace.
    pub fn key(&self) -> &str {
        &self.key
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// One listing page, in any order, and an optional cursor for the next page.
pub struct ListPage {
    /// TODO_REWRITE
    #[builder(default)]
    objects: Vec<ListedObject>,
    /// TODO_REWRITE
    continuation: Option<ContinuationToken>,
}
impl ListPage {
    /// TODO_REWRITE
    ///
    /// Returns the objects in this page, without an ordering guarantee.
    pub fn objects(&self) -> &[ListedObject] {
        &self.objects
    }
    /// TODO_REWRITE
    ///
    /// Returns the opaque cursor for continuing this listing, if present.
    pub fn continuation(&self) -> Option<&ContinuationToken> {
        self.continuation.as_ref()
    }
}

#[non_exhaustive]
#[derive(Clone, Debug)]
/// TODO_REWRITE
///
/// Storage failure classification; unknown writes may have committed.
pub enum StoreErrorKind {
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// The backend or transport is unavailable.
    Unavailable {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// The request was denied.
    Unauthorized {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// Required metadata or response structure is invalid.
    InvalidResponse {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// The backend cannot honor this operation.
    Unsupported {},
    #[non_exhaustive]
    /// TODO_REWRITE
    ///
    /// A write may have committed; do not treat this as a definitive failed claim.
    WriteOutcomeUnknown {},
}
impl StoreErrorKind {
    /// TODO_REWRITE
    ///
    /// Classifies a transport/service availability failure.
    pub fn unavailable() -> Self {
        Self::Unavailable {}
    }
    /// TODO_REWRITE
    ///
    /// Classifies a denied request.
    pub fn unauthorized() -> Self {
        Self::Unauthorized {}
    }
    /// TODO_REWRITE
    ///
    /// Classifies missing or malformed storage response data.
    pub fn invalid_response() -> Self {
        Self::InvalidResponse {}
    }
    /// TODO_REWRITE
    ///
    /// Classifies an unsupported backend primitive or response.
    pub fn unsupported() -> Self {
        Self::Unsupported {}
    }
    /// TODO_REWRITE
    ///
    /// Classifies a write that may have committed despite the observed failure.
    pub fn write_outcome_unknown() -> Self {
        Self::WriteOutcomeUnknown {}
    }
}
#[derive(Debug)]
/// TODO_REWRITE
///
/// A classified storage failure with a displayable explanation.
pub struct StoreError {
    kind: StoreErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}
impl StoreError {
    /// TODO_REWRITE
    ///
    /// Combines a storage failure classification with its underlying error.
    pub fn new(
        kind: StoreErrorKind,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }
    /// TODO_REWRITE
    ///
    /// Returns the storage failure classification.
    pub fn kind(&self) -> &StoreErrorKind {
        &self.kind
    }
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.source)
    }
}
impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl<T: SamplingStore + ?Sized> SamplingStore for std::sync::Arc<T> {
    fn modification_time_uncertainty(&self) -> std::time::Duration {
        (**self).modification_time_uncertainty()
    }
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        (**self).get(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        (**self).put(request)
    }
    fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        (**self).list_page(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        (**self).delete(key)
    }
}
