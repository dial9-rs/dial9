//! Keeping dial9's own fields out of the other sinks in a `tee`.
//!
//! [`Dial9Context`](super::Dial9Context)'s fields are ordinary metrique
//! fields, so without filtering they also reach the EMF/JSON side of a
//! `tee`. Worker and task ids are at least arguable there; the monotonic
//! timestamps are noise. Metrique has no per-format field exclusion, so this
//! module wraps the other sink and drops the `dial9.`-prefixed fields on
//! their way in.

use std::borrow::Cow;
use std::io;
use std::time::SystemTime;

use metrique_writer::core::descriptor::Descriptors;
use metrique_writer::core::entry::SampleGroupElement;
use metrique_writer::{Entry, EntryConfig, EntryIoStream, EntryWriter, IoStreamError, Value};

/// The prefix every dial9-owned field name carries (see
/// [`Dial9Context`](super::Dial9Context)).
const DIAL9_PREFIX: &str = "dial9.";

/// An [`EntryIoStream`] wrapper that hides dial9's own fields from the stream
/// it wraps.
///
/// Built by [`Dial9Stream::tee`](super::Dial9Stream::tee); put it around the
/// non-dial9 side of a `tee` so `dial9.worker_id` and friends stay out of
/// your EMF/JSON output while the dial9 sink still sees them.
#[derive(Debug)]
pub struct WithoutDial9Fields<S> {
    inner: S,
}

impl<S> WithoutDial9Fields<S> {
    /// Wrap `inner` so entries reach it without their `dial9.` fields.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: EntryIoStream> EntryIoStream for WithoutDial9Fields<S> {
    fn next(&mut self, entry: &impl Entry) -> Result<(), IoStreamError> {
        self.inner.next(&Filtered(entry))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// An [`Entry`] view with the `dial9.` fields removed.
struct Filtered<'e, E>(&'e E);

impl<E: Entry> Entry for Filtered<'_, E> {
    fn write<'a>(&'a self, writer: &mut impl EntryWriter<'a>) {
        let mut writer = FilteringWriter(writer);
        self.0.write(&mut writer);
    }

    fn sample_group(&self) -> impl Iterator<Item = SampleGroupElement> {
        // Sample groups are keyed by name/value pairs the entry chooses;
        // dial9's context fields are not sample-group members, so there is
        // nothing to filter.
        self.0.sample_group()
    }

    fn descriptors(&self) -> Descriptors<'_> {
        filter_descriptors(self.0.descriptors())
    }
}

/// Drop whole descriptor segments that describe only dial9 fields, so a
/// descriptor-aware downstream sink sees a descriptor consistent with the
/// filtered value stream.
///
/// A segment covers one contiguous run of an entry's write output, and the
/// context is always its own segment (a flatten site gets one; `Dial9Event`
/// chains one), so dropping whole segments covers both supported shapes. A
/// mixed segment only arises from a user field literally named `dial9.*`;
/// those cannot be subset out of `&'static` descriptor storage, so rather
/// than hand out a descriptor whose fields no longer line up with the values,
/// the whole thing degrades to `Unavailable`.
fn filter_descriptors(descs: Descriptors<'_>) -> Descriptors<'_> {
    let Some(available) = descs.into_available() else {
        return Descriptors::Unavailable;
    };

    let mut kept = Vec::with_capacity(available.len());
    for seg in available.iter() {
        let mut dial9 = 0usize;
        let mut total = 0usize;
        for field in seg.fields() {
            total += 1;
            if is_dial9_field(field.name_parts()) {
                dial9 += 1;
            }
        }
        if dial9 == 0 {
            kept.push(seg.clone());
        } else if dial9 != total {
            return Descriptors::Unavailable;
        }
    }
    Descriptors::available(kept)
}

/// Whether a field's resolved name (prefixes then base name) is dial9-owned.
fn is_dial9_field<'n>(mut name_parts: impl Iterator<Item = &'n str>) -> bool {
    // The prefix lives entirely in the first part: flatten-site prefixes come
    // first, and a bare `Dial9Context` field's own name already starts with
    // `dial9.`. A user prefix in front of it (`req_dial9.worker_id`) is the
    // user's own field namespace, and is deliberately left alone.
    name_parts
        .next()
        .is_some_and(|p| p.starts_with(DIAL9_PREFIX))
}

/// An [`EntryWriter`] that swallows `value` callbacks for dial9's fields.
struct FilteringWriter<'w, W>(&'w mut W);

impl<'a, W: EntryWriter<'a>> EntryWriter<'a> for FilteringWriter<'_, W> {
    fn timestamp(&mut self, timestamp: SystemTime) {
        self.0.timestamp(timestamp);
    }

    fn value(&mut self, name: impl Into<Cow<'a, str>>, value: &(impl Value + ?Sized)) {
        let name = name.into();
        if name.starts_with(DIAL9_PREFIX) {
            return;
        }
        self.0.value(name, value);
    }

    fn config(&mut self, config: &'a dyn EntryConfig) {
        self.0.config(config);
    }
}
