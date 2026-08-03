//! Request credential extractor used when the viewer is built without S3.
//!
//! Local storage ignores AWS credential headers. Keeping an infallible
//! extractor in the route signatures lets the local viewer expose the same
//! storage endpoints without pulling in the AWS SDK.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use std::convert::Infallible;

pub struct MaybeCreds;

impl<S: Send + Sync> FromRequestParts<S> for MaybeCreds {
    type Rejection = Infallible;

    async fn from_request_parts(_parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Self)
    }
}
