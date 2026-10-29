//! Storage primitives for implementing a custom sampling backend.
//! A failed or cancelled write may have committed.
use bytes::Bytes;
use std::{future::Future, pin::Pin, time::SystemTime};

/// A boxed, sendable storage operation; it borrows the store for its lifetime.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

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
    /// Maximum amount by which a returned modification time precedes the
    /// actual write time, in addition to the configured clock-skew allowance.
    /// Exact-time backends can use zero; second-resolution timestamps need 1s.
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
    /// Atomically writes under the requested condition; uncertainty requires reconciliation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the write fails. An uncertain commit must use
    /// [`StoreErrorKind::WriteOutcomeUnknown`]; a definitive precondition failure
    /// is a [`PutOutcome::PreconditionFailed`], rather than an error.
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome>;
    /// Lists a page including completed writes, with an opaque continuation cursor.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if listing fails or the response cannot satisfy
    /// the namespace, pagination or modification-time contract.
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage>;
    /// Deletes a process's unique membership object; deleting an absent object succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the deletion fails. Cancellation or an error
    /// does not prove that the object still exists.
    fn delete(&self, key: String) -> StoreFuture<'_, ()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Opaque object version used for conditional writes; callers must not interpret it.
pub struct VersionToken(String);
impl VersionToken {
    /// Wraps the backend's opaque object-version token.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Returns the backend-provided opaque token string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Debug)]
/// Opaque backend cursor for continuing a listing.
pub struct ContinuationToken(String);
impl ContinuationToken {
    /// Wraps the backend's opaque listing cursor.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Returns the backend-provided opaque token string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[non_exhaustive]
#[derive(Clone, Debug)]
/// The atomic precondition required for a write.
pub enum WriteCondition {
    #[non_exhaustive]
    /// Writes regardless of the current object version.
    Unconditional {},
    #[non_exhaustive]
    /// Writes only if the object is absent.
    Absent {},
    #[non_exhaustive]
    /// Writes only if the current version matches.
    Matches {
        /// Version that must still identify the current object.
        version: VersionToken,
    },
}
impl WriteCondition {
    /// Allows replacing any value; shared roster writes use stricter conditions.
    pub fn unconditional() -> Self {
        Self::Unconditional {}
    }
    /// Allows writing only when the object does not exist.
    pub fn absent() -> Self {
        Self::Absent {}
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
    /// Every attempt definitively failed its precondition without changing the object.
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
/// Object contents, conditional-write version and server modification time.
pub struct StoredObject {
    body: Bytes,
    version: VersionToken,
    modified_at: SystemTime,
}
impl StoredObject {
    /// Returns the stored or requested object bytes.
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
/// Prefix, page size and optional continuation cursor for a listing.
pub struct ListRequest {
    #[builder(into)]
    prefix: String,
    #[builder(default = 256)]
    page_size: usize,
    continuation: Option<ContinuationToken>,
}
impl ListRequest {
    /// Returns the relative key prefix to list.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    /// Returns the requested maximum number of objects in one page.
    pub fn page_size(&self) -> usize {
        self.page_size
    }
    /// Returns the opaque cursor for continuing this listing, if present.
    pub fn continuation(&self) -> Option<&ContinuationToken> {
        self.continuation.as_ref()
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// Object key returned by listing.
pub struct ListedObject {
    #[builder(into)]
    key: String,
}
impl ListedObject {
    /// Returns the key relative to the backend namespace.
    pub fn key(&self) -> &str {
        &self.key
    }
}
#[derive(Clone, Debug, bon::Builder)]
/// One listing page, in any order, and an optional cursor for the next page.
pub struct ListPage {
    #[builder(default)]
    objects: Vec<ListedObject>,
    continuation: Option<ContinuationToken>,
}
impl ListPage {
    /// Returns the objects in this page, without an ordering guarantee.
    pub fn objects(&self) -> &[ListedObject] {
        &self.objects
    }
    /// Returns the opaque cursor for continuing this listing, if present.
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
    /// A write may have committed; do not treat this as a definitive failed claim.
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
    fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        (**self).list(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        (**self).delete(key)
    }
}
