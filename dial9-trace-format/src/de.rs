//! serde [`Deserializer`] for raw trace events.
//!
//! This module presents a [`RawEvent`](crate::decoder::RawEvent) (the
//! callback type yielded by [`Decoder::for_each_event`](crate::decoder::Decoder::for_each_event))
//! as a flat map compatible with serde's internally-tagged-enum representation.
//!
//! # Mapping
//!
//! The deserializer presents each event as a map with these entries (in order):
//!
//! 1. `"event"` → the schema name (the discriminant for `#[serde(tag = "event")]`).
//! 2. `"timestamp_ns"` → the absolute frame-header timestamp (only if the
//!    schema has `has_timestamp = true`).
//! 3. One entry per schema field, keyed by field name.
//!
//! Pool resolution is automatic:
//!
//! - [`FieldValueRef::PooledString`](crate::types::FieldValueRef::PooledString)
//!   resolves through the decoder's
//!   [`StringPool`](crate::decoder::StringPool) and presents as a string.
//! - [`FieldValueRef::PooledStackFrames`](crate::types::FieldValueRef::PooledStackFrames)
//!   resolves through the decoder's
//!   [`StackPool`](crate::decoder::StackPool) and presents as a sequence of
//!   `u64`.
//! - [`FieldValueRef::None`](crate::types::FieldValueRef::None) presents as
//!   serde `None`, so optional fields work transparently.
//!
//! # Example
//!
//! ```no_run
//! use dial9_trace_format::decoder::Decoder;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! #[serde(tag = "event")]
//! enum MyEvent {
//!     #[serde(rename = "PollStart")]
//!     PollStart {
//!         timestamp_ns: u64,
//!         worker_id: u64,
//!     },
//!     #[serde(other)]
//!     Other,
//! }
//!
//! # let bytes: &[u8] = &[];
//! let mut dec = Decoder::new(bytes).unwrap();
//! dec.for_each_event(|raw| {
//!     match raw.deserialize::<MyEvent>() {
//!         Ok(MyEvent::PollStart { timestamp_ns, worker_id }) => {
//!             println!("poll {worker_id} at {timestamp_ns}");
//!         }
//!         Ok(MyEvent::Other) => {}
//!         Err(e) => eprintln!("decode error: {e}"),
//!     }
//! }).unwrap();
//! ```

use crate::decoder::{RawEvent, StackPool, StringPool};
use crate::schema::SchemaEntry;
use crate::types::{FieldValueRef, StringMapIter};
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Error returned when deserializing a trace event into a typed value fails.
///
/// This wraps a message produced by serde or by the deserializer itself
/// (e.g. for missing fields or pool lookup failures).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeserError(String);

impl DeserError {
    /// Construct a new error with the given message.
    pub(crate) fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    /// The error message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeserError {}

impl de::Error for DeserError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

// ── Deserializer entry point ───────────────────────────────────────────────

/// Deserialize a [`RawEvent`] into `T`.
///
/// Most callers should use [`RawEvent::deserialize`] instead, which is a thin
/// wrapper over this function.
pub fn from_raw_event<'a, 'f, T: serde::de::DeserializeOwned>(
    raw: &RawEvent<'a, 'f>,
) -> Result<T, DeserError> {
    T::deserialize(RawEventDeserializer {
        name: raw.name,
        timestamp_ns: raw.timestamp_ns,
        fields: raw.fields,
        schema: raw.schema,
        string_pool: raw.string_pool,
        stack_pool: raw.stack_pool,
    })
}

// ── RawEventDeserializer: the top-level deserializer for a raw event ───────

/// Deserializer that presents a [`RawEvent`] as an internally-tagged enum
/// or struct.
struct RawEventDeserializer<'a, 'f> {
    name: &'f str,
    timestamp_ns: Option<u64>,
    fields: &'f [FieldValueRef<'a>],
    schema: &'f SchemaEntry,
    string_pool: &'f StringPool,
    stack_pool: &'f StackPool,
}

impl<'de, 'a, 'f> de::Deserializer<'de> for RawEventDeserializer<'a, 'f> {
    type Error = DeserError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // For internally-tagged enums and plain structs, present as a map.
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_map(RawEventMapAccess {
            name: self.name,
            timestamp_ns: self.timestamp_ns,
            fields: self.fields,
            schema: self.schema,
            string_pool: self.string_pool,
            stack_pool: self.stack_pool,
            index: 0,
            pending_value: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // For internally-tagged enums, serde first deserializes the tag from a
        // map representation, so route through deserialize_map.
        self.deserialize_map(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct identifier ignored_any
    }
}

// ── MapAccess: walks "event" → "timestamp_ns" → schema fields ──────────────

struct RawEventMapAccess<'a, 'f> {
    name: &'f str,
    timestamp_ns: Option<u64>,
    fields: &'f [FieldValueRef<'a>],
    schema: &'f SchemaEntry,
    string_pool: &'f StringPool,
    stack_pool: &'f StackPool,
    /// Cursor into the synthetic map:
    ///   0                                     → "event"
    ///   1 (if timestamp_ns is Some)           → "timestamp_ns"
    ///   1 + (timestamp offset) + field_idx    → schema field
    index: usize,
    /// Value paired with the most recently yielded key. Cleared after
    /// `next_value_seed` consumes it.
    pending_value: Option<PendingValue<'a, 'f>>,
}

/// A value temporarily held between `next_key_seed` and `next_value_seed`.
enum PendingValue<'a, 'f> {
    /// Tag value: the event name, deserialized as a `&str`.
    Name(&'f str),
    /// Synthetic timestamp, deserialized as `u64`.
    Timestamp(u64),
    /// A schema field. The pools are needed to resolve `PooledString`
    /// and `PooledStackFrames`.
    Field {
        value: &'f FieldValueRef<'a>,
        string_pool: &'f StringPool,
        stack_pool: &'f StackPool,
    },
}

impl<'a, 'f> RawEventMapAccess<'a, 'f> {
    /// Total number of map entries (tag + optional timestamp + fields).
    fn total_entries(&self) -> usize {
        1 + (self.timestamp_ns.is_some() as usize) + self.fields.len()
    }

    /// Number of leading "synthetic" entries (1 for tag, +1 if timestamp).
    fn synthetic_offset(&self) -> usize {
        1 + (self.timestamp_ns.is_some() as usize)
    }
}

impl<'de, 'a, 'f> MapAccess<'de> for RawEventMapAccess<'a, 'f> {
    type Error = DeserError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if self.index >= self.total_entries() {
            return Ok(None);
        }

        let synthetic = self.synthetic_offset();
        let (key, pending) = match self.index {
            0 => ("event", PendingValue::Name(self.name)),
            1 if self.timestamp_ns.is_some() => (
                "timestamp_ns",
                PendingValue::Timestamp(self.timestamp_ns.unwrap()),
            ),
            i => {
                let field_idx = i - synthetic;
                let field_def = self.schema.fields().get(field_idx).ok_or_else(|| {
                    DeserError::new(format!(
                        "schema for event '{}' has fewer fields than the wire stream",
                        self.name
                    ))
                })?;
                let value = self.fields.get(field_idx).ok_or_else(|| {
                    DeserError::new(format!(
                        "wire stream for event '{}' has fewer fields than the schema",
                        self.name
                    ))
                })?;
                (
                    field_def.name(),
                    PendingValue::Field {
                        value,
                        string_pool: self.string_pool,
                        stack_pool: self.stack_pool,
                    },
                )
            }
        };

        self.pending_value = Some(pending);
        self.index += 1;
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let pending = self
            .pending_value
            .take()
            .ok_or_else(|| DeserError::new("next_value_seed called without next_key_seed"))?;
        match pending {
            PendingValue::Name(s) => seed.deserialize(StrDeserializer(s)),
            PendingValue::Timestamp(t) => seed.deserialize(U64Deserializer(t)),
            PendingValue::Field {
                value,
                string_pool,
                stack_pool,
            } => seed.deserialize(FieldValueDeserializer {
                value,
                string_pool,
                stack_pool,
            }),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.total_entries() - self.index)
    }
}

// ── Trivial leaf deserializers for the synthetic entries ───────────────────

/// Deserializer that always yields a borrowed `&str`.
struct StrDeserializer<'f>(&'f str);

impl<'de, 'f> de::Deserializer<'de> for StrDeserializer<'f> {
    type Error = DeserError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_str(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// Deserializer that always yields a `u64`.
struct U64Deserializer(u64);

impl<'de> de::Deserializer<'de> for U64Deserializer {
    type Error = DeserError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_u64(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

// ── FieldValueDeserializer: deserializer for one schema field's value ──────

/// Deserializer for a single [`FieldValueRef`], with pool context for
/// transparent resolution of `PooledString` / `PooledStackFrames`.
struct FieldValueDeserializer<'a, 'f> {
    value: &'f FieldValueRef<'a>,
    string_pool: &'f StringPool,
    stack_pool: &'f StackPool,
}

impl<'de, 'a, 'f> de::Deserializer<'de> for FieldValueDeserializer<'a, 'f> {
    type Error = DeserError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            FieldValueRef::I64(v) => visitor.visit_i64(*v),
            FieldValueRef::F64(v) => visitor.visit_f64(*v),
            FieldValueRef::Bool(v) => visitor.visit_bool(*v),
            FieldValueRef::String(v) => visitor.visit_str(v),
            FieldValueRef::Bytes(v) => visitor.visit_bytes(v),
            FieldValueRef::Varint(v) => visitor.visit_u64(*v),
            FieldValueRef::PooledString(id) => match self.string_pool.get(*id) {
                Some(s) => visitor.visit_str(s),
                None => Err(DeserError::new(format!(
                    "PooledString id {id:?} not found in string pool"
                ))),
            },
            FieldValueRef::PooledStackFrames(id) => match self.stack_pool.get(*id) {
                Some(frames) => visitor.visit_seq(U64SeqAccess {
                    iter: frames.iter().copied(),
                }),
                None => Err(DeserError::new(format!(
                    "PooledStackFrames id {id:?} not found in stack pool"
                ))),
            },
            FieldValueRef::StackFrames(frames) => visitor.visit_seq(U64SeqAccess {
                iter: frames.iter(),
            }),
            FieldValueRef::None => visitor.visit_none(),
            FieldValueRef::List(list) => visitor.visit_seq(FieldValueSeqAccess {
                iter: list.iter(),
                string_pool: self.string_pool,
                stack_pool: self.stack_pool,
            }),
            FieldValueRef::StringMap(map) => visitor.visit_map(StringMapAccess {
                iter: map.iter(),
                pending_value: None,
            }),
            FieldValueRef::Map(map) => visitor.visit_map(DynamicMapAccess {
                iter: map.iter(),
                pending_value: None,
                string_pool: self.string_pool,
                stack_pool: self.stack_pool,
            }),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            FieldValueRef::None => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            FieldValueRef::Bytes(v) => visitor.visit_seq(U64SeqAccess {
                iter: v.iter().map(|&b| b as u64),
            }),
            // A `StringMap` field can also be deserialized as a sequence of
            // `(key, value)` tuples. This makes `Vec<(String, String)>` work
            // naturally without `#[serde(deserialize_with = "...")]`, which is
            // useful for ordered or duplicate-tolerant key-value data.
            FieldValueRef::StringMap(map) => {
                visitor.visit_seq(StringMapSeqAccess { iter: map.iter() })
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        unit unit_struct newtype_struct tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

// ── Helpers: SeqAccess wrapper for u64 sequences (stack frames) ───────────

/// `SeqAccess` over an iterator of `u64`. Used for both pooled stack frames
/// (`&[u64]` via `.iter().copied()`) and inline stack frames (`StackFrameIter`).
struct U64SeqAccess<I: Iterator<Item = u64>> {
    iter: I,
}

impl<'de, I: Iterator<Item = u64>> SeqAccess<'de> for U64SeqAccess<I> {
    type Error = DeserError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.iter.next() {
            Some(v) => seed.deserialize(v.into_deserializer()).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        let (lower, upper) = self.iter.size_hint();
        if Some(lower) == upper {
            Some(lower)
        } else {
            None
        }
    }
}

// ── Helpers: SeqAccess for DynamicList ─────────────────────────────────────

/// `SeqAccess` over an iterator of `&FieldValueRef`, delegating each element
/// to `FieldValueDeserializer`.
struct FieldValueSeqAccess<'f, I> {
    iter: I,
    string_pool: &'f StringPool,
    stack_pool: &'f StackPool,
}

impl<'de, 'a: 'f, 'f, I> SeqAccess<'de> for FieldValueSeqAccess<'f, I>
where
    I: Iterator<Item = &'f FieldValueRef<'a>>,
{
    type Error = DeserError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.iter.next() {
            Some(value) => seed
                .deserialize(FieldValueDeserializer {
                    value,
                    string_pool: self.string_pool,
                    stack_pool: self.stack_pool,
                })
                .map(Some),
            None => Ok(None),
        }
    }
}

// ── Helpers: MapAccess for StringMap ───────────────────────────────────────

/// `MapAccess` over a `StringMapIter`, yielding `(&str, &str)` pairs.
struct StringMapAccess<'a> {
    iter: StringMapIter<'a>,
    pending_value: Option<&'a str>,
}

impl<'de, 'a> MapAccess<'de> for StringMapAccess<'a> {
    type Error = DeserError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.iter.next() {
            Some((k, v)) => {
                self.pending_value = Some(v);
                seed.deserialize(StrDeserializer(k)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let v = self
            .pending_value
            .take()
            .ok_or_else(|| DeserError::new("next_value called without preceding next_key"))?;
        seed.deserialize(StrDeserializer(v))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

// ── Helpers: SeqAccess for StringMap (for `Vec<(String, String)>` decode) ──

/// `SeqAccess` over a `StringMapIter`, yielding each `(key, value)` pair as
/// a 2-element tuple deserializer. Together with [`StringMapPairDeserializer`]
/// this lets `StringMap` fields decode into `Vec<(String, String)>` (or any
/// shape serde can produce from a sequence of 2-tuples).
struct StringMapSeqAccess<'a> {
    iter: StringMapIter<'a>,
}

impl<'de, 'a> SeqAccess<'de> for StringMapSeqAccess<'a> {
    type Error = DeserError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.iter.next() {
            Some((k, v)) => seed
                .deserialize(StringMapPairDeserializer { k, v })
                .map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Deserializer for one `(key, value)` pair of a `StringMap`, presented as
/// a 2-element sequence so that `(String, String)` (and other 2-tuple shapes)
/// decode naturally.
struct StringMapPairDeserializer<'a> {
    k: &'a str,
    v: &'a str,
}

impl<'de, 'a> de::Deserializer<'de> for StringMapPairDeserializer<'a> {
    type Error = DeserError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // Pairs are tuples by default — present as a 2-element sequence.
        visitor.visit_seq(StringMapPairSeqAccess {
            entries: [Some(self.k), Some(self.v)],
            idx: 0,
        })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        str string bytes byte_buf option unit unit_struct newtype_struct
        map struct enum identifier ignored_any
    }
}

/// `SeqAccess` walking the two entries `[key, value]` of a `StringMap` pair.
struct StringMapPairSeqAccess<'a> {
    entries: [Option<&'a str>; 2],
    idx: usize,
}

impl<'de, 'a> SeqAccess<'de> for StringMapPairSeqAccess<'a> {
    type Error = DeserError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        if self.idx >= self.entries.len() {
            return Ok(None);
        }
        let entry = self.entries[self.idx]
            .take()
            .ok_or_else(|| DeserError::new("tuple pair entry already consumed"))?;
        self.idx += 1;
        seed.deserialize(StrDeserializer(entry)).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len() - self.idx)
    }
}

// ── Helpers: MapAccess for DynamicMap ──────────────────────────────────────

/// `MapAccess` over a `DynamicMapRef` iterator, delegating both keys and values
/// to `FieldValueDeserializer`.
struct DynamicMapAccess<'a, 'f, I> {
    iter: I,
    pending_value: Option<&'f FieldValueRef<'a>>,
    string_pool: &'f StringPool,
    stack_pool: &'f StackPool,
}

impl<'de, 'a: 'f, 'f, I> MapAccess<'de> for DynamicMapAccess<'a, 'f, I>
where
    I: Iterator<Item = (&'f FieldValueRef<'a>, &'f FieldValueRef<'a>)>,
{
    type Error = DeserError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.iter.next() {
            Some((k, v)) => {
                self.pending_value = Some(v);
                seed.deserialize(FieldValueDeserializer {
                    value: k,
                    string_pool: self.string_pool,
                    stack_pool: self.stack_pool,
                })
                .map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .pending_value
            .take()
            .ok_or_else(|| DeserError::new("next_value called without preceding next_key"))?;
        seed.deserialize(FieldValueDeserializer {
            value,
            string_pool: self.string_pool,
            stack_pool: self.stack_pool,
        })
    }
}
