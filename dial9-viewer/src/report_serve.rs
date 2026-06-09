//! Static-file server for agent-generated report folders.
//!
//! Reports embed iframes that fetch trace `.bin` files from relative paths.
//! Browsers block `fetch()` over `file://`, so reports must be served via
//! HTTP. This module provides a minimal `axum::Router` that serves a
//! single directory.
//!
//! Wired up to the CLI as `dial9 report serve <dir>`.

use std::path::Path;

use axum::Router;
use tower_http::services::ServeDir;

/// Build an `axum::Router` that serves static files from `dir`.
///
/// Requests to `/` are mapped to `index.html` (if it exists). Missing
/// files return `404 Not Found`. The server enforces no auth and follows
/// no symlinks outside `dir` (default `ServeDir` behavior).
pub fn report_serve_router(dir: &Path) -> Router {
    Router::new().fallback_service(ServeDir::new(dir).append_index_html_on_directories(true))
}
