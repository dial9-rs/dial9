//! Shared helpers for this crate's own `#[cfg(test)]` unit tests.
//!
//! Distinct from `test_util`, which is `pub` under the `test-util` feature
//! for sibling-crate tests; this module is crate-internal and feature-free,
//! so it's always available wherever `#[cfg(test)]` is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dial9_trace_format::decoder::{DecodedFrameRef, Decoder};
use dial9_trace_format::types::FieldValueRef;

#[cfg(all(test, shuttle, feature = "pipeline"))]
pub(crate) use pipeline_helpers::*;

/// Shuttle-only pipeline test helpers, gated as one module so the imports
/// they need carry the same `#[cfg]` as the functions using them.
#[cfg(all(test, shuttle, feature = "pipeline"))]
mod pipeline_helpers {
    use crate::buffer::MemoryBuffer;
    use crate::clock::clock_monotonic_ns;
    use crate::fs::Fs;
    use crate::primitives::sync::Arc;
    use crate::recording::Recorder;
    use crate::shared_state::SharedState;
    use crate::source::Source;

    /// Start a `Recorder` over an in-memory writer with `sources` registered.
    ///
    /// Drives `Recorder::start`/`SharedState` construction directly, bypassing
    /// `RecorderBuilder`'s `SoleRecorderGuard` (a process-wide singleton that
    /// doesn't tolerate a shuttle scenario's many same-process iterations).
    pub(crate) fn start_shuttle_memory_recorder(
        sources: Vec<Box<dyn Source>>,
    ) -> (Recorder, Arc<Fs>) {
        let writer = MemoryBuffer::builder()
            .max_total_size(100 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let fs = writer.fs_handle().expect("in-memory writer exposes its fs");
        let shared = Arc::new(SharedState::new(clock_monotonic_ns()));
        for source in sources {
            shared.push_source(source);
        }
        let recorder = Recorder::start(shared, writer, None, || || {});
        recorder.handle().enable();
        (recorder, fs)
    }

    /// Call `f` with each sealed segment's raw bytes, draining `fs` until empty.
    pub(crate) fn for_each_sealed_segment(fs: &Arc<Fs>, mut f: impl FnMut(Vec<u8>)) {
        loop {
            let taken = fs.take_files();
            if taken.segments.is_empty() {
                break;
            }
            for seg in taken.segments {
                let (_seg_ref, payload, _accounting) = seg.load().expect("load sealed segment");
                f(payload.into_vec());
            }
        }
    }
}

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
/// Returns an empty map for a fully-empty/unparseable buffer (e.g. a segment
/// with no header). A mid-stream frame decode error panics: the input is
/// assumed to be real flush output, so a corrupt frame means the encoder is
/// broken, not that the segment is legitimately empty.
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
