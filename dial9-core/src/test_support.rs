//! Shared helpers for this crate's own `#[cfg(test)]` unit tests.
//!
//! Distinct from `test_util`, which is `pub` under the `test-util` feature
//! for sibling-crate tests; this module is crate-internal and feature-free,
//! so it's always available wherever `#[cfg(test)]` is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dial9_trace_format::decoder::{DecodedFrameRef, Decoder};
use dial9_trace_format::types::FieldValueRef;

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

/// Decode a segment's `SegmentMetadataEvent` entries into a merged map.
/// Returns an empty map if `data` doesn't decode (e.g. an empty segment).
pub(crate) fn decode_segment_metadata(data: &[u8]) -> HashMap<String, String> {
    let Some(mut dec) = Decoder::new(data) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    while let Some(frame) = dec.next_frame_ref().expect("decode frame") {
        let DecodedFrameRef::Event {
            type_id, values, ..
        } = frame
        else {
            continue;
        };
        if dec.registry().get(type_id).map(|s| s.name()) != Some("SegmentMetadataEvent") {
            continue;
        }
        if let Some(FieldValueRef::StringMap(m)) = values.first() {
            out.extend(m.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        }
    }
    out
}
