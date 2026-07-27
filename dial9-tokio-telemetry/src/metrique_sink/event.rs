//! Attaching dial9 context to an entry from the outside.
//!
//! [`Dial9Context`] can be flattened into an entry as a field, but that means
//! editing the metrics struct, which is intrusive when the struct is shared or
//! owned by another team. [`Dial9Event`] wraps an existing entry instead,
//! contributing the same context fields without touching its definition.

use std::ops::{Deref, DerefMut};

use metrique::{CloseValue, Identity, InflectableEntry, RootMetric};
use metrique_writer::core::descriptor::Descriptors;
use metrique_writer::core::entry::SampleGroupElement;
use metrique_writer::{EntrySink, EntryWriter};

use super::Dial9Context;

/// An entry wrapped with dial9 runtime context.
///
/// Recording an entry through this wrapper is equivalent to flattening a
/// [`Dial9Context`] into it: the same `dial9.`-prefixed context fields are
/// written, so the sink opts the entry in the same way. Reach for it when the
/// entry type is not yours to change, or when dial9 support should be
/// switchable from one call site (a `#[cfg]` around the wrap, rather than
/// around a struct field).
///
/// Derefs to the wrapped entry, so field access reads and writes straight
/// through:
///
/// ```ignore
/// let mut m = RequestMetrics { /* no dial9 field */ }
///     .append_on_drop_dial9(ServiceMetrics::sink());
/// m.latency_ms = 5; // reaches RequestMetrics through the wrapper
/// ```
#[derive(Debug)]
pub struct Dial9Event<E> {
    dial9: Dial9Context,
    event: E,
}

impl<E> Dial9Event<E> {
    /// Wrap `event`, capturing the calling thread's runtime context now (see
    /// [`Dial9Context::capture`]).
    pub fn new(event: E) -> Self {
        Self {
            dial9: Dial9Context::capture(),
            event,
        }
    }

    /// The wrapped entry.
    pub fn into_inner(self) -> E {
        self.event
    }
}

impl<E> Deref for Dial9Event<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.event
    }
}

impl<E> DerefMut for Dial9Event<E> {
    fn deref_mut(&mut self) -> &mut E {
        &mut self.event
    }
}

impl<E: CloseValue> CloseValue for Dial9Event<E> {
    type Closed = Dial9EventClosed<E::Closed>;

    fn close(self) -> Self::Closed {
        Dial9EventClosed {
            event: self.event.close(),
            // Closing the context reads the end timestamp, so it runs with
            // (not before) the entry's own close.
            dial9: self.dial9.close(),
        }
    }
}

/// The closed form of [`Dial9Event`]: what actually reaches a sink.
#[derive(Debug)]
pub struct Dial9EventClosed<C> {
    event: C,
    dial9: <Dial9Context as CloseValue>::Closed,
}

// Implemented for the default name style only. A wrapper is always a root
// entry (that is what `RootEntry`/`CloseEntry` require), never a flattened
// child, so there is no parent style to inflect to. Being generic over `NS`
// would mean requiring `C: InflectableEntry<NS>` for every `NS`, which is not
// expressible.
impl<C: InflectableEntry> InflectableEntry for Dial9EventClosed<C> {
    fn write<'a>(&'a self, writer: &mut impl EntryWriter<'a>) {
        // The entry first, then the context: `descriptors` below reports the
        // same order, which is the contract descriptor-aware sinks rely on.
        // The wrapped entry leading also means its descriptor segment is
        // first, so the event is named after it rather than after the
        // context, matching a flattened `Dial9Context`.
        self.event.write(writer);
        InflectableEntry::<Identity>::write(&self.dial9, writer);
    }

    fn sample_group(&self) -> impl Iterator<Item = SampleGroupElement> {
        // Sampling stays the wrapped entry's decision; context fields are not
        // sample-group members.
        self.event.sample_group()
    }

    fn descriptors(&self) -> Descriptors<'_> {
        self.event
            .descriptors()
            .chain(InflectableEntry::<Identity>::descriptors(&self.dial9))
    }
}

/// Extension trait adding dial9 opt-in to any metrique entry.
pub trait Dial9EntryExt: CloseValue + Sized {
    /// Wrap this entry with dial9 runtime context, capturing it now.
    fn with_dial9_context(self) -> Dial9Event<Self> {
        Dial9Event::new(self)
    }

    /// Like `append_on_drop`, but records the entry into the dial9 trace as
    /// well: context is captured now, and the entry closes and appends to
    /// `sink` on drop.
    ///
    /// ```ignore
    /// let mut m = RequestMetrics { /* ... */ }
    ///     .append_on_drop_dial9(ServiceMetrics::sink());
    /// ```
    fn append_on_drop_dial9<Q>(self, sink: Q) -> metrique::AppendAndCloseOnDrop<Dial9Event<Self>, Q>
    where
        Self: Send + Sync + 'static,
        Self::Closed: InflectableEntry,
        Q: EntrySink<RootMetric<Dial9Event<Self>>> + Send + Sync + 'static,
    {
        metrique::append_and_close(self.with_dial9_context(), sink)
    }
}

impl<E: CloseValue + Sized> Dial9EntryExt for E {}
