//! Sampling backend storage primitives
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
/// Lists include completed writes in UTF-8 byte lexicographic key order, across
/// pages as well as within each page. They need not form an atomic fleet snapshot.
/// Continuation cursors must permit complete enumeration.
/// Versions identify the observed body and may repeat if the body is repeated;
/// shared roster revisions therefore include a fresh nonce on every write.
/// Cancellation never proves that a write did not commit. Delete is only for
/// unique process membership; shared rosters are never deleted.
/// A precondition failure must mean no attempt in that call committed.
/// Backends must not hide an uncertain earlier attempt behind a retry's
/// precondition failure; return an unknown write outcome for reconciliation.
pub trait SamplingStore: Send + Sync + 'static {
    /// Resolution of the `modified_at` field of stored objects
    fn modification_time_uncertainty(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    /// Reads an object's current contents and version, or returns `None` if absent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the object cannot be read or its contents,
    /// version or modification time cannot satisfy the storage contract.
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>>;

    /// Writes `request` to the store
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the write fails. An uncertain commit must use
    /// [`StoreErrorKind::WriteOutcomeUnknown`]; a definitive precondition failure
    /// is a [`PutOutcome::PreconditionFailed`], rather than an error.
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome>;

    /// TODO_REWRITE
    ///
    /// Lists a bounded page of keys in UTF-8 byte lexicographic order.
    ///
    /// Keys are relative to the store namespace and must match the requested
    /// prefix. Each page contains at most [`ListRequest::page_size`] objects;
    /// page size must be positive. Keys strictly increase within a page and
    /// across successive pages of the same listing. Sorting individual pages
    /// of an unordered backend does not satisfy this contract.
    ///
    /// Continue with each opaque cursor until [`ListPage::continuation`] returns
    /// `None` to enumerate the complete listing. A short or empty page with a
    /// continuation is not exhaustion. A failed or cyclic continuation leaves
    /// an incomplete listing and must not be used as a complete census.
    /// Listing does not provide an atomic snapshot of concurrent fleet changes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if page size is zero, listing fails, or the response
    /// cannot satisfy the namespace, order, page bound or pagination contract.
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage>;

    /// Deletes the object at `key`
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the deletion fails.
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
/// The atomic precondition required for a write.
pub enum WriteCondition {
    /// Writes regardless of the current object version.
    Unconditional,
    /// Writes only if the object is absent.
    Absent,
    /// Writes only if the current version matches.
    Matches {
        /// Version that must still identify the current object.
        version: VersionToken,
    },
}
impl WriteCondition {
    /// Writes regardless of the current object version.
    pub fn unconditional() -> Self {
        Self::Unconditional
    }
    /// Allows writing iff the object does not exist.
    pub fn absent() -> Self {
        Self::Absent
    }
    /// Allows writing only while the supplied object version still matches.
    pub fn matching(version: VersionToken) -> Self {
        Self::Matches { version }
    }
}

#[derive(Clone, Debug, bon::Builder)]
/// Object key, body and conditional-write requirement.
pub struct PutRequest {
    #[builder(into)]
    key: String,
    body: Bytes,
    #[builder(default = WriteCondition::absent())]
    condition: WriteCondition,
}
impl PutRequest {
    /// Returns the key relative to the backend namespace.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the stored or requested object bytes.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Returns the atomic precondition for this write.
    pub fn condition(&self) -> &WriteCondition {
        &self.condition
    }
}
#[non_exhaustive]
#[derive(Clone, Debug)]
/// A write result that distinguishes definitive failure from uncertainty.
pub enum PutOutcome {
    #[non_exhaustive]
    /// The write succeeded and returned its new version.
    Stored {
        /// New object version returned by the completed write.
        version: VersionToken,
    },
    #[non_exhaustive]
    /// The precondition prevented this object from being inserted
    PreconditionFailed {},
}
impl PutOutcome {
    /// Constructs a successful write result with the new version.
    pub fn stored(version: VersionToken) -> Self {
        Self::Stored { version }
    }
    /// Constructs a definitive failure: no attempt in this call changed the object.
    pub fn precondition_failed() -> Self {
        Self::PreconditionFailed {}
    }
}

#[derive(Clone, Debug, bon::Builder)]
/// Stored object
pub struct StoredObject {
    body: Bytes,
    version: VersionToken,
    modified_at: SystemTime,
}
impl StoredObject {
    /// Returns the stored object bytes.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Returns the opaque version required to conditionally replace the observed object.
    pub fn version(&self) -> &VersionToken {
        &self.version
    }

    /// Returns the server modification time; consumers must account for backend timestamp uncertainty.
    pub fn modified_at(&self) -> SystemTime {
        self.modified_at
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// ListRequest
pub struct ListRequest {
    /// Prefix to filter the List by
    #[builder(into)]
    prefix: String,
    /// TODO_REWRITE
    ///
    /// Positive maximum number of objects returned in one page.
    #[builder(default = 256)]
    page_size: usize,
    /// Optional continuation token to paginate
    continuation: Option<ContinuationToken>,
}
impl ListRequest {
    /// Returns the relative key prefix to list.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// TODO_REWRITE
    ///
    /// Returns the requested maximum number of objects in one page; listing
    /// requires a positive value.
    /// Backends may return fewer objects and provide a continuation cursor.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns the opaque cursor for continuing this listing, if present.
    pub fn continuation(&self) -> Option<&ContinuationToken> {
        self.continuation.as_ref()
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// Object key
pub struct ListedObject {
    #[builder(into)]
    key: String,
}
impl ListedObject {
    /// Listed object key
    pub fn key(&self) -> &str {
        &self.key
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// TODO_REWRITE
///
/// One bounded listing page in key order, and an optional continuation cursor.
pub struct ListPage {
    #[builder(default)]
    objects: Vec<ListedObject>,
    continuation: Option<ContinuationToken>,
}
impl ListPage {
    /// TODO_REWRITE
    ///
    /// Returns objects in strictly increasing UTF-8 byte lexicographic key order.
    pub fn objects(&self) -> &[ListedObject] {
        &self.objects
    }

    /// TODO_REWRITE
    ///
    /// Returns the opaque cursor for continuing this listing, if present.
    /// Only `None` indicates exhaustion, including when this page is short or empty.
    pub fn continuation(&self) -> Option<&ContinuationToken> {
        self.continuation.as_ref()
    }
}

#[non_exhaustive]
#[derive(Clone, Debug)]
/// Storage failure classification; unknown writes may have committed.
pub enum StoreErrorKind {
    #[non_exhaustive]
    /// The backend or transport is unavailable.
    Unavailable {},
    #[non_exhaustive]
    /// The request was denied.
    Unauthorized {},
    #[non_exhaustive]
    /// Required metadata or response structure is invalid.
    InvalidResponse {},
    #[non_exhaustive]
    /// The backend cannot honor this operation.
    Unsupported {},
    #[non_exhaustive]
    /// A write may have committed but exact outcome is unknown
    WriteOutcomeUnknown {},
}
impl StoreErrorKind {
    /// Classifies a transport/service availability failure.
    pub fn unavailable() -> Self {
        Self::Unavailable {}
    }

    /// Classifies a denied request.
    pub fn unauthorized() -> Self {
        Self::Unauthorized {}
    }

    /// Classifies missing or malformed storage response data.
    pub fn invalid_response() -> Self {
        Self::InvalidResponse {}
    }

    /// Classifies an unsupported backend primitive or response.
    pub fn unsupported() -> Self {
        Self::Unsupported {}
    }

    /// Classifies a write that may have committed despite the observed failure.
    pub fn write_outcome_unknown() -> Self {
        Self::WriteOutcomeUnknown {}
    }
}
#[derive(Debug)]
/// A classified storage failure with a displayable explanation.
pub struct StoreError {
    kind: StoreErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}
impl StoreError {
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
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        (**self).list_sorted_page(request)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        (**self).delete(key)
    }
}
