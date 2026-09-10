//! Shared helpers for this crate's own `#[cfg(test)]` unit tests.
//!
//! Distinct from `test_util`, which is `pub` under the `test-util` feature
//! for sibling-crate tests; this module is crate-internal and feature-free,
//! so it's always available wherever `#[cfg(test)]` is.

use std::path::{Path, PathBuf};

/// The one sealed (non-`.active`) `.bin` segment in `dir`.
pub(crate) fn sealed_segment(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .expect("trace dir readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.ends_with(".bin") && !name.ends_with(".active")
        })
        .expect("a sealed .bin segment")
}
