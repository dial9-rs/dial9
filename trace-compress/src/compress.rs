//! Format-aware compressor for dial9 trace files.
//!
//! The input is tokenized frame-by-frame and re-arranged into homogeneous
//! streams:
//!
//! - a frame-sequence stream (one symbol per frame),
//! - per-event-type timestamp-delta streams,
//! - per-schema per-field value columns with per-column transforms
//!   (varint, zigzag-delta, or a recent-id cache for span ids),
//! - pool streams (UUID-packed string data, delta-coded stack addresses).
//!
//! The concatenation is entropy-coded with a single zstd pass. Decompression
//! replays the frame sequence and reconstructs the original file byte-exactly.
//!
//! Columns are keyed by a *schema identity* (hash of the schema definition,
//! assigned dense ids in order of first appearance) rather than the wire type
//! id: wire ids are only stable within one segment, and the same id can refer
//! to different schemas in different segments.

use crate::wire::*;
use std::collections::HashMap;

pub const CONTAINER_MAGIC: [u8; 4] = *b"D9TC";
pub const CONTAINER_VERSION: u8 = 1;

// Frame-sequence symbols. Events are encoded as EVENT_SYM_BASE + type_id.
const SYM_SEGMENT_HEADER: u64 = 0;
const SYM_SCHEMA: u64 = 1;
const SYM_STRING_POOL: u64 = 2;
const SYM_STACK_POOL: u64 = 3;
const SYM_TS_RESET: u64 = 4;
const SYM_ANNOTATIONS: u64 = 5;
const EVENT_SYM_BASE: u64 = 6;

// Per-column encodings for plain varint columns (first byte of the stream).
const MODE_PLAIN: u8 = 0;
const MODE_DELTA: u8 = 1;

#[derive(Debug)]
pub enum Error {
    Parse(ParseError),
    Io(std::io::Error),
    Corrupt(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt container: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Error::Parse(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type CResult<T> = std::result::Result<T, Error>;

// --- varint / zigzag helpers ---

#[inline]
fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

#[inline]
fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Deterministic FNV-1a 64 (std hashers are randomly seeded).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Read-side cursor over a stream slice.
#[derive(Default)]
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn varint(&mut self) -> CResult<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self
                .data
                .get(self.pos)
                .ok_or_else(|| Error::Corrupt("varint past end of stream".into()))?;
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 70 {
                return Err(Error::Corrupt("varint too long".into()));
            }
        }
    }

    fn take(&mut self, n: usize) -> CResult<&'a [u8]> {
        let s = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or_else(|| Error::Corrupt(format!("stream truncated: wanted {n} bytes")))?;
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> CResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

// --- span-id recent-value cache ---

/// Fields holding span ids repeat the same value a handful of times across
/// event types (enter/exit/close, parent references). Encoding a repeat as
/// "distance since last mention" turns an ~9-byte varint into 1–2 bytes.
///
/// Encoding per value: `varint(distance)` for a hit (distance >= 1 counts
/// entries appended since the value's last mention), or `0` followed by
/// `varint(value)` for a miss. Every encoded value (hit or miss) is
/// (re-)appended, keeping repeat distances short.
#[derive(Default)]
struct IdCacheEnc {
    last_seq: HashMap<u64, u64>,
    len: u64,
}

impl IdCacheEnc {
    fn encode(&mut self, v: u64, out: &mut Vec<u8>) {
        match self.last_seq.get(&v) {
            Some(&seq) => put_varint(out, self.len - seq),
            None => {
                out.push(0);
                put_varint(out, v);
            }
        }
        self.last_seq.insert(v, self.len);
        self.len += 1;
    }
}

#[derive(Default)]
struct IdCacheDec {
    log: Vec<u64>,
}

impl IdCacheDec {
    fn decode(&mut self, r: &mut Reader<'_>) -> CResult<u64> {
        let d = r.varint()?;
        let v = if d == 0 {
            r.varint()?
        } else {
            let idx = (self.log.len() as u64)
                .checked_sub(d)
                .ok_or_else(|| Error::Corrupt("id-cache distance out of range".into()))?;
            self.log[idx as usize]
        };
        self.log.push(v);
        Ok(v)
    }
}

fn is_cached_field(field_name: &str, tag: u8) -> bool {
    tag & !OPTIONAL_BIT == FT_VARINT && matches!(field_name, "span_id" | "parent_span_id")
}

// --- UUID packing ---

/// Returns the 16 packed bytes if `s` is a canonical lowercase hex UUID.
fn pack_uuid(s: &[u8]) -> Option<[u8; 16]> {
    if s.len() != 36 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut nibbles = 0usize;
    for (i, &c) in s.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return None;
            }
            continue;
        }
        let n = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        out[nibbles / 2] = (out[nibbles / 2] << 4) | n;
        nibbles += 1;
    }
    Some(out)
}

fn unpack_uuid(b: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(36);
    for (i, &byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push(b'-');
        }
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0xf) as usize]);
    }
    out
}

// --- column model ---

#[derive(Clone, Copy, PartialEq)]
enum ColKind {
    /// FT_VARINT column: values buffered in `vals`, encoding chosen at
    /// assembly time (plain vs zigzag-delta).
    VarintPlain,
    /// span-id-like FT_VARINT column: encoded through the global id cache
    /// at tokenization time (ordering across columns matters).
    VarintCached,
    /// Everything else: bytes appended to `main`/`aux` at tokenization time.
    Other,
}

struct Column {
    kind: ColKind,
    presence: Vec<u8>,
    main: Vec<u8>,
    aux: Vec<u8>,
    vals: Vec<u64>,
}

impl Column {
    fn new(kind: ColKind) -> Self {
        Column {
            kind,
            presence: Vec::new(),
            main: Vec::new(),
            aux: Vec::new(),
            vals: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Streams {
    frame_seq: Vec<u8>,
    schema_frames: Vec<u8>,
    annotation_frames: Vec<u8>,
    /// Per-schema-group timestamp-delta streams. Gap distributions are
    /// strongly conditioned on the event type, so separate streams compress
    /// far better than one interleaved stream.
    ts_delta: HashMap<u16, Vec<u8>>,
    ts_reset: Vec<u8>,
    strpool_counts: Vec<u8>,
    strpool_ids: Vec<u8>,
    strpool_lens: Vec<u8>,
    strpool_flags: Vec<u8>,
    strpool_uuids: Vec<u8>,
    strpool_data: Vec<u8>,
    stkpool_counts: Vec<u8>,
    stkpool_ids: Vec<u8>,
    stkpool_framecounts: Vec<u8>,
    stkpool_addrs: Vec<u8>,
    columns: HashMap<(u16, u8), Column>,
}

// Directory stream kinds.
const K_FRAME_SEQ: u8 = 0;
const K_SCHEMA_FRAMES: u8 = 1;
const K_ANNOTATION_FRAMES: u8 = 2;
const K_TS_DELTA: u8 = 3;
const K_TS_RESET: u8 = 4;
const K_STRPOOL_COUNTS: u8 = 5;
const K_STRPOOL_IDS: u8 = 6;
const K_STRPOOL_LENS: u8 = 7;
const K_STRPOOL_DATA: u8 = 8;
const K_STKPOOL_COUNTS: u8 = 9;
const K_STKPOOL_IDS: u8 = 10;
const K_STKPOOL_FRAMECOUNTS: u8 = 11;
const K_STKPOOL_ADDRS: u8 = 12;
const K_COL_PRESENCE: u8 = 13;
const K_COL_MAIN: u8 = 14;
const K_COL_AUX: u8 = 15;
const K_STRPOOL_FLAGS: u8 = 16;
const K_STRPOOL_UUIDS: u8 = 17;

/// Tracks schema-identity groups: the same schema definition (independent of
/// wire type id) always maps to the same dense group id, in order of first
/// appearance. Both compressor and decompressor derive this identically.
#[derive(Default)]
struct SchemaGroups {
    by_hash: HashMap<u64, u16>,
}

impl SchemaGroups {
    /// `body` is the schema frame bytes *excluding* the frame tag and wire
    /// type id (those vary across segments for the same schema).
    fn group_for(&mut self, body: &[u8]) -> CResult<u16> {
        let h = fnv1a(body);
        let next = self.by_hash.len();
        match self.by_hash.entry(h) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(*e.get()),
            std::collections::hash_map::Entry::Vacant(e) => {
                let id: u16 = next
                    .try_into()
                    .map_err(|_| Error::Corrupt("more than 65535 distinct schemas".into()))?;
                e.insert(id);
                Ok(id)
            }
        }
    }
}

/// Per-type registration: parsed meta + column group + per-field column kinds.
struct RegisteredSchema {
    meta: SchemaMeta,
    group: u16,
    col_kinds: Vec<ColKind>,
}

fn register(meta: SchemaMeta, group: u16) -> RegisteredSchema {
    let col_kinds = meta
        .tags
        .iter()
        .zip(&meta.field_names)
        .map(|(&tag, name)| {
            if tag & !OPTIONAL_BIT == FT_VARINT {
                if is_cached_field(name, tag) {
                    ColKind::VarintCached
                } else {
                    ColKind::VarintPlain
                }
            } else {
                ColKind::Other
            }
        })
        .collect();
    RegisteredSchema {
        meta,
        group,
        col_kinds,
    }
}

/// Tokenize the raw trace into streams.
fn build_streams(data: &[u8]) -> CResult<Streams> {
    let mut cur = Cursor::new(data);
    let version = check_header(&mut cur)?;

    let mut s = Streams::default();
    let mut groups = SchemaGroups::default();
    let mut schemas: HashMap<u16, RegisteredSchema> = HashMap::new();
    let mut id_cache = IdCacheEnc::default();

    // The initial file header is encoded the same way as a mid-stream
    // segment-reset header (version byte lives in schema_frames).
    put_varint(&mut s.frame_seq, SYM_SEGMENT_HEADER);
    s.schema_frames.push(version);

    let mut prev_strpool_id: u32 = 0;
    let mut prev_ts_reset: u64 = 0;
    let mut prev_stkpool_id: u32 = 0;
    let mut prev_first_addr: u64 = 0;

    while !cur.eof() {
        // Mid-stream header = segment reset.
        if cur.data.len() - cur.pos >= HEADER_SIZE && cur.data[cur.pos..cur.pos + 4] == MAGIC {
            let hdr = cur.take(HEADER_SIZE)?;
            put_varint(&mut s.frame_seq, SYM_SEGMENT_HEADER);
            s.schema_frames.push(hdr[4]);
            schemas.clear();
            continue;
        }
        let frame_start = cur.pos;
        let tag = cur.u8()?;
        match tag {
            TAG_SCHEMA => {
                let (type_id, meta) = parse_schema(&mut cur)?;
                put_varint(&mut s.frame_seq, SYM_SCHEMA);
                s.schema_frames
                    .extend_from_slice(&cur.data[frame_start..cur.pos]);
                let group = groups.group_for(&cur.data[frame_start + 3..cur.pos])?;
                schemas.insert(type_id, register(meta, group));
            }
            TAG_EVENT => {
                let type_id = cur.u16()?;
                let reg = schemas.get(&type_id).ok_or_else(|| ParseError {
                    pos: frame_start,
                    msg: format!("event with unknown type_id {type_id}"),
                })?;
                put_varint(&mut s.frame_seq, EVENT_SYM_BASE + type_id as u64);
                if reg.meta.has_timestamp {
                    let delta = cur.u24()?;
                    put_varint(s.ts_delta.entry(reg.group).or_default(), delta as u64);
                }
                for (i, &ftag) in reg.meta.tags.iter().enumerate() {
                    let kind = reg.col_kinds[i];
                    let col = s
                        .columns
                        .entry((reg.group, i as u8))
                        .or_insert_with(|| Column::new(kind));
                    if ftag & OPTIONAL_BIT != 0 {
                        // Record the raw presence byte so reconstruction is
                        // byte-exact even for nonstandard nonzero prefixes.
                        let prefix = *cur.data.get(cur.pos).ok_or_else(|| ParseError {
                            pos: cur.pos,
                            msg: "truncated optional prefix".into(),
                        })?;
                        col.presence.push(prefix);
                    }
                    let val = read_val(&mut cur, ftag)?;
                    match val {
                        Val::None => {}
                        Val::Varint(v) => match kind {
                            ColKind::VarintCached => id_cache.encode(v, &mut col.main),
                            _ => col.vals.push(v),
                        },
                        Val::I64(v) => col.main.extend_from_slice(&v.to_le_bytes()),
                        Val::F64Bits(v) => col.main.extend_from_slice(&v.to_le_bytes()),
                        Val::Bool(b) => col.main.push(b),
                        Val::Blob(b) => {
                            put_varint(&mut col.main, b.len() as u64);
                            col.aux.extend_from_slice(b);
                        }
                        Val::PoolId(v) => put_varint(&mut col.main, v as u64),
                        Val::U8(v) => col.main.push(v),
                        Val::U16(v) => col.main.extend_from_slice(&v.to_le_bytes()),
                        Val::U32(v) => put_varint(&mut col.main, v as u64),
                        Val::StackFrames(addrs) => {
                            let count = addrs.len() / 8;
                            put_varint(&mut col.main, count as u64);
                            let mut prev = 0u64;
                            for c in addrs.chunks_exact(8) {
                                let a = u64::from_le_bytes(c.try_into().unwrap());
                                put_varint(&mut col.main, zigzag(a.wrapping_sub(prev) as i64));
                                prev = a;
                            }
                        }
                        Val::Raw(span) => col.main.extend_from_slice(span),
                    }
                }
            }
            TAG_STRING_POOL => {
                put_varint(&mut s.frame_seq, SYM_STRING_POOL);
                let count = cur.u32()?;
                put_varint(&mut s.strpool_counts, count as u64);
                for _ in 0..count {
                    let pool_id = cur.u32()?;
                    put_varint(
                        &mut s.strpool_ids,
                        zigzag(pool_id.wrapping_sub(prev_strpool_id) as i32 as i64),
                    );
                    prev_strpool_id = pool_id;
                    let len = cur.u32()? as usize;
                    put_varint(&mut s.strpool_lens, len as u64);
                    let bytes = cur.take(len)?;
                    match pack_uuid(bytes) {
                        Some(packed) => {
                            s.strpool_flags.push(1);
                            s.strpool_uuids.extend_from_slice(&packed);
                        }
                        None => {
                            s.strpool_flags.push(0);
                            s.strpool_data.extend_from_slice(bytes);
                        }
                    }
                }
            }
            TAG_STACK_POOL => {
                put_varint(&mut s.frame_seq, SYM_STACK_POOL);
                let count = cur.u32()?;
                put_varint(&mut s.stkpool_counts, count as u64);
                for _ in 0..count {
                    let pool_id = cur.u32()?;
                    put_varint(
                        &mut s.stkpool_ids,
                        zigzag(pool_id.wrapping_sub(prev_stkpool_id) as i32 as i64),
                    );
                    prev_stkpool_id = pool_id;
                    let fc = cur.u32()? as usize;
                    put_varint(&mut s.stkpool_framecounts, fc as u64);
                    let addrs = cur.take(fc * 8)?;
                    // First address is delta'd against the previous entry's
                    // first address; subsequent frames against their
                    // predecessor within the stack.
                    let mut prev = prev_first_addr;
                    for (i, c) in addrs.chunks_exact(8).enumerate() {
                        let a = u64::from_le_bytes(c.try_into().unwrap());
                        put_varint(&mut s.stkpool_addrs, zigzag(a.wrapping_sub(prev) as i64));
                        if i == 0 {
                            prev_first_addr = a;
                        }
                        prev = a;
                    }
                }
            }
            TAG_TIMESTAMP_RESET => {
                put_varint(&mut s.frame_seq, SYM_TS_RESET);
                let ts = cur.u64()?;
                put_varint(
                    &mut s.ts_reset,
                    zigzag(ts.wrapping_sub(prev_ts_reset) as i64),
                );
                prev_ts_reset = ts;
            }
            TAG_SCHEMA_ANNOTATIONS => {
                skip_annotations(&mut cur)?;
                put_varint(&mut s.frame_seq, SYM_ANNOTATIONS);
                s.annotation_frames
                    .extend_from_slice(&cur.data[frame_start..cur.pos]);
            }
            other => {
                return Err(ParseError {
                    pos: frame_start,
                    msg: format!("unknown frame tag {other:#x}"),
                }
                .into());
            }
        }
    }
    finalize_varint_columns(&mut s);
    Ok(s)
}

/// Serialize buffered varint columns, choosing plain vs zigzag-delta by raw
/// varint size (delta must win clearly: it can obscure repeat structure that
/// the entropy pass would otherwise find).
fn finalize_varint_columns(s: &mut Streams) {
    for col in s.columns.values_mut() {
        if col.kind != ColKind::VarintPlain || col.vals.is_empty() {
            continue;
        }
        let mut plain = 0usize;
        let mut delta = 0usize;
        let mut prev = 0u64;
        for &v in &col.vals {
            plain += varint_len(v);
            delta += varint_len(zigzag(v.wrapping_sub(prev) as i64));
            prev = v;
        }
        let use_delta = delta * 10 <= plain * 8;
        col.main.reserve(1 + if use_delta { delta } else { plain });
        col.main
            .push(if use_delta { MODE_DELTA } else { MODE_PLAIN });
        let mut prev = 0u64;
        for &v in &col.vals {
            if use_delta {
                put_varint(&mut col.main, zigzag(v.wrapping_sub(prev) as i64));
                prev = v;
            } else {
                put_varint(&mut col.main, v);
            }
        }
        col.vals = Vec::new();
    }
}

fn serialize_dir(dir: &[(u8, u16, u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, dir.len() as u64);
    for (kind, type_id, field, payload) in dir {
        out.push(*kind);
        out.extend_from_slice(&type_id.to_le_bytes());
        out.push(*field);
        put_varint(&mut out, payload.len() as u64);
    }
    for (_, _, _, payload) in dir {
        out.extend_from_slice(payload);
    }
    out
}

/// Serialize the stream directories + payloads into two buffers for entropy
/// coding: `structured` (large, LZ-friendly; its level is the speed knob) and
/// `entropy` (small, near-incompressible streams — timestamp gaps, packed
/// UUIDs, reset jumps — always compressed at a high level, which is cheap
/// there and buys the most bytes per millisecond).
fn assemble(s: &Streams) -> (Vec<u8>, Vec<u8>) {
    // Order columns deterministically and group presence / main / aux so
    // similar data sits together in the zstd window.
    let mut keys: Vec<&(u16, u8)> = s.columns.keys().collect();
    keys.sort();

    let mut ts_keys: Vec<&u16> = s.ts_delta.keys().collect();
    ts_keys.sort();

    let mut dir: Vec<(u8, u16, u8, &[u8])> = vec![
        (K_FRAME_SEQ, 0, 0, &s.frame_seq),
        (K_SCHEMA_FRAMES, 0, 0, &s.schema_frames),
        (K_ANNOTATION_FRAMES, 0, 0, &s.annotation_frames),
        (K_STRPOOL_COUNTS, 0, 0, &s.strpool_counts),
        (K_STRPOOL_IDS, 0, 0, &s.strpool_ids),
        (K_STRPOOL_LENS, 0, 0, &s.strpool_lens),
        (K_STRPOOL_FLAGS, 0, 0, &s.strpool_flags),
        (K_STKPOOL_COUNTS, 0, 0, &s.stkpool_counts),
        (K_STKPOOL_IDS, 0, 0, &s.stkpool_ids),
        (K_STKPOOL_FRAMECOUNTS, 0, 0, &s.stkpool_framecounts),
        (K_STKPOOL_ADDRS, 0, 0, &s.stkpool_addrs),
    ];
    for &&(t, f) in &keys {
        let col = &s.columns[&(t, f)];
        if !col.presence.is_empty() {
            dir.push((K_COL_PRESENCE, t, f, &col.presence));
        }
    }
    for &&(t, f) in &keys {
        let col = &s.columns[&(t, f)];
        if !col.main.is_empty() {
            dir.push((K_COL_MAIN, t, f, &col.main));
        }
    }
    // Textual payloads at the end, next to their kin.
    for &&(t, f) in &keys {
        let col = &s.columns[&(t, f)];
        if !col.aux.is_empty() {
            dir.push((K_COL_AUX, t, f, &col.aux));
        }
    }
    dir.push((K_STRPOOL_DATA, 0, 0, &s.strpool_data));

    let mut entropy_dir: Vec<(u8, u16, u8, &[u8])> = Vec::new();
    for &&t in &ts_keys {
        entropy_dir.push((K_TS_DELTA, t, 0, &s.ts_delta[&t]));
    }
    entropy_dir.push((K_TS_RESET, 0, 0, &s.ts_reset));
    entropy_dir.push((K_STRPOOL_UUIDS, 0, 0, &s.strpool_uuids));

    (serialize_dir(&dir), serialize_dir(&entropy_dir))
}

/// Report per-stream raw and individually-zstd'd sizes, for tuning.
pub fn stream_report(data: &[u8], level: i32) -> CResult<String> {
    use std::fmt::Write;
    let s = build_streams(data)?;
    let mut rows: Vec<(String, usize, usize)> = Vec::new();
    let mut add = |name: String, payload: &[u8]| {
        if payload.is_empty() {
            return;
        }
        let c = zstd::bulk::compress(payload, level).unwrap();
        rows.push((name, payload.len(), c.len()));
    };
    add("frame_seq".into(), &s.frame_seq);
    {
        let mut all_ts: Vec<u8> = Vec::new();
        let mut ts_keys: Vec<&u16> = s.ts_delta.keys().collect();
        ts_keys.sort();
        for t in ts_keys {
            all_ts.extend_from_slice(&s.ts_delta[t]);
        }
        add("ts_delta (all types)".into(), &all_ts);
    }
    add("ts_reset".into(), &s.ts_reset);
    add("schema_frames".into(), &s.schema_frames);
    add("annotation_frames".into(), &s.annotation_frames);
    add("strpool_counts".into(), &s.strpool_counts);
    add("strpool_ids".into(), &s.strpool_ids);
    add("strpool_lens".into(), &s.strpool_lens);
    add("strpool_flags".into(), &s.strpool_flags);
    add("strpool_uuids".into(), &s.strpool_uuids);
    add("strpool_data".into(), &s.strpool_data);
    add("stkpool_counts".into(), &s.stkpool_counts);
    add("stkpool_ids".into(), &s.stkpool_ids);
    add("stkpool_framecounts".into(), &s.stkpool_framecounts);
    add("stkpool_addrs".into(), &s.stkpool_addrs);
    let mut keys: Vec<&(u16, u8)> = s.columns.keys().collect();
    keys.sort();
    for &&(t, f) in &keys {
        let col = &s.columns[&(t, f)];
        add(format!("col {t}.{f} presence"), &col.presence);
        add(format!("col {t}.{f} main"), &col.main);
        add(format!("col {t}.{f} aux"), &col.aux);
    }
    rows.sort_by_key(|(_, _, c)| std::cmp::Reverse(*c));
    let mut out = String::new();
    let total_raw: usize = rows.iter().map(|r| r.1).sum();
    let total_c: usize = rows.iter().map(|r| r.2).sum();
    writeln!(out, "total: raw {total_raw}, indiv-zstd {total_c}").unwrap();
    for (name, raw, c) in rows.iter().take(40) {
        writeln!(out, "{name:<28} raw {raw:>9}  zstd {c:>9}").unwrap();
    }
    Ok(out)
}

/// Default zstd level for the structured stream group. Measured on the demo
/// trace, this is the speed knob: 15 keeps total compression comfortably
/// faster than `gzip -6`; 19 is still faster than `gzip -9`.
pub const DEFAULT_LEVEL: i32 = 15;

/// Level for the entropy-bound stream group (timestamp gaps, packed UUIDs).
/// Counterintuitively this group earns a high level: it is small (~1.4 MB)
/// and level 19's optimal parse buys ~94 KB there for ~100 ms, a far better
/// trade than raising the structured group's level. Overridable via
/// D9TC_ENTROPY_LEVEL for tuning experiments.
fn entropy_level() -> i32 {
    std::env::var("D9TC_ENTROPY_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(19)
}

/// Compress a raw dial9 trace. `level` is the zstd level for the structured
/// stream group (the entropy-bound group always uses [`ENTROPY_LEVEL`]).
pub fn compress(data: &[u8], level: i32) -> CResult<Vec<u8>> {
    let timing = std::env::var_os("D9TC_TIMING").is_some();
    let t0 = std::time::Instant::now();
    let streams = build_streams(data)?;
    let t1 = std::time::Instant::now();
    let (structured, entropy) = assemble(&streams);
    let t2 = std::time::Instant::now();
    let mut out = Vec::with_capacity(structured.len() / 3 + entropy.len() / 2 + 16);
    out.extend_from_slice(&CONTAINER_MAGIC);
    out.push(CONTAINER_VERSION);
    let a = zstd::bulk::compress(&structured, level)?;
    let t3 = std::time::Instant::now();
    let b = zstd::bulk::compress(&entropy, entropy_level())?;
    let t4 = std::time::Instant::now();
    if timing {
        eprintln!(
            "build {:.1}ms | assemble {:.1}ms | zstd-A({}) {} -> {} in {:.1}ms | zstd-B({}) {} -> {} in {:.1}ms",
            (t1 - t0).as_secs_f64() * 1000.0,
            (t2 - t1).as_secs_f64() * 1000.0,
            level,
            structured.len(),
            a.len(),
            (t3 - t2).as_secs_f64() * 1000.0,
            entropy_level(),
            entropy.len(),
            b.len(),
            (t4 - t3).as_secs_f64() * 1000.0,
        );
    }
    put_varint(&mut out, a.len() as u64);
    out.extend_from_slice(&a);
    out.extend_from_slice(&b);
    Ok(out)
}

/// Read-side view of the streams.
struct StreamsView<'a> {
    frame_seq: Reader<'a>,
    schema_frames: Reader<'a>,
    annotation_frames: Reader<'a>,
    ts_delta: HashMap<u16, Reader<'a>>,
    ts_reset: Reader<'a>,
    strpool_counts: Reader<'a>,
    strpool_ids: Reader<'a>,
    strpool_lens: Reader<'a>,
    strpool_flags: Reader<'a>,
    strpool_uuids: Reader<'a>,
    strpool_data: Reader<'a>,
    stkpool_counts: Reader<'a>,
    stkpool_ids: Reader<'a>,
    stkpool_framecounts: Reader<'a>,
    stkpool_addrs: Reader<'a>,
    columns: HashMap<(u16, u8), ColumnView<'a>>,
}

#[derive(Default)]
struct ColumnView<'a> {
    presence: Reader<'a>,
    main: Reader<'a>,
    aux: Reader<'a>,
    /// Decoded lazily from the first byte of `main` for plain varint columns.
    mode: Option<u8>,
    /// Running previous value for MODE_DELTA columns.
    prev: u64,
}

impl<'a> ColumnView<'a> {
    /// Read the next plain-varint-column value, honoring the column mode.
    fn next_varint(&mut self) -> CResult<u64> {
        let mode = match self.mode {
            Some(m) => m,
            None => {
                let m = self.main.byte()?;
                self.mode = Some(m);
                m
            }
        };
        let raw = self.main.varint()?;
        match mode {
            MODE_PLAIN => Ok(raw),
            MODE_DELTA => {
                let v = self.prev.wrapping_add(unzigzag(raw) as u64);
                self.prev = v;
                Ok(v)
            }
            other => Err(Error::Corrupt(format!("unknown column mode {other}"))),
        }
    }
}

fn parse_blob_into<'a>(blob: &'a [u8], view: &mut StreamsView<'a>) -> CResult<()> {
    let mut r = Reader::new(blob);
    let n = r.varint()? as usize;
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = r.byte()?;
        let type_id = u16::from_le_bytes(r.take(2)?.try_into().unwrap());
        let field = r.byte()?;
        let len = r.varint()? as usize;
        entries.push((kind, type_id, field, len));
    }
    for (kind, type_id, field, len) in entries {
        let payload = Reader::new(r.take(len)?);
        match kind {
            K_FRAME_SEQ => view.frame_seq = payload,
            K_SCHEMA_FRAMES => view.schema_frames = payload,
            K_ANNOTATION_FRAMES => view.annotation_frames = payload,
            K_TS_DELTA => {
                view.ts_delta.insert(type_id, payload);
            }
            K_TS_RESET => view.ts_reset = payload,
            K_STRPOOL_COUNTS => view.strpool_counts = payload,
            K_STRPOOL_IDS => view.strpool_ids = payload,
            K_STRPOOL_LENS => view.strpool_lens = payload,
            K_STRPOOL_FLAGS => view.strpool_flags = payload,
            K_STRPOOL_UUIDS => view.strpool_uuids = payload,
            K_STRPOOL_DATA => view.strpool_data = payload,
            K_STKPOOL_COUNTS => view.stkpool_counts = payload,
            K_STKPOOL_IDS => view.stkpool_ids = payload,
            K_STKPOOL_FRAMECOUNTS => view.stkpool_framecounts = payload,
            K_STKPOOL_ADDRS => view.stkpool_addrs = payload,
            K_COL_PRESENCE => view.columns.entry((type_id, field)).or_default().presence = payload,
            K_COL_MAIN => view.columns.entry((type_id, field)).or_default().main = payload,
            K_COL_AUX => view.columns.entry((type_id, field)).or_default().aux = payload,
            other => return Err(Error::Corrupt(format!("unknown stream kind {other}"))),
        }
    }
    Ok(())
}

fn empty_view<'a>() -> StreamsView<'a> {
    StreamsView {
        frame_seq: Reader::default(),
        schema_frames: Reader::default(),
        annotation_frames: Reader::default(),
        ts_delta: HashMap::new(),
        ts_reset: Reader::default(),
        strpool_counts: Reader::default(),
        strpool_ids: Reader::default(),
        strpool_lens: Reader::default(),
        strpool_flags: Reader::default(),
        strpool_uuids: Reader::default(),
        strpool_data: Reader::default(),
        stkpool_counts: Reader::default(),
        stkpool_ids: Reader::default(),
        stkpool_framecounts: Reader::default(),
        stkpool_addrs: Reader::default(),
        columns: HashMap::new(),
    }
}

fn put_u24(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
}

/// Re-encode a canonical LEB128 varint (matches dial9-trace-format's encoder).
fn put_leb(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Copy one self-describing dynamic value (StringMap/DynList/DynMap span)
/// from a column reader into `out`.
fn copy_dynamic(r: &mut Reader<'_>, tag: u8, out: &mut Vec<u8>) -> CResult<()> {
    let start = r.pos;
    let mut cur = Cursor::new(r.data);
    cur.pos = start;
    skip_dynamic(&mut cur, tag).map_err(Error::Parse)?;
    let span = &r.data[start..cur.pos];
    out.extend_from_slice(span);
    r.pos = cur.pos;
    Ok(())
}

/// Decompress a container produced by [`compress`] back into the original
/// raw trace bytes.
pub fn decompress(container: &[u8]) -> CResult<Vec<u8>> {
    if container.len() < 5 || container[..4] != CONTAINER_MAGIC {
        return Err(Error::Corrupt("bad container magic".into()));
    }
    if container[4] != CONTAINER_VERSION {
        return Err(Error::Corrupt(format!(
            "unsupported container version {}",
            container[4]
        )));
    }
    let mut hdr = Reader::new(&container[5..]);
    let a_len = hdr.varint()? as usize;
    let a_compressed = hdr.take(a_len)?;
    let b_compressed = &hdr.data[hdr.pos..];
    let blob_a = zstd::decode_all(a_compressed)?;
    let blob_b = zstd::decode_all(b_compressed)?;
    let mut v = empty_view();
    parse_blob_into(&blob_a, &mut v)?;
    parse_blob_into(&blob_b, &mut v)?;

    let mut out: Vec<u8> = Vec::new();
    let mut groups = SchemaGroups::default();
    let mut schemas: HashMap<u16, RegisteredSchema> = HashMap::new();
    let mut id_cache = IdCacheDec::default();

    let mut prev_strpool_id: u32 = 0;
    let mut prev_ts_reset: u64 = 0;
    let mut prev_stkpool_id: u32 = 0;
    let mut prev_first_addr: u64 = 0;

    // The initial file header arrives as the first SYM_SEGMENT_HEADER symbol.
    while !v.frame_seq.done() {
        let sym = v.frame_seq.varint()?;
        match sym {
            SYM_SEGMENT_HEADER => {
                let version = v.schema_frames.byte()?;
                out.extend_from_slice(&MAGIC);
                out.push(version);
                schemas.clear();
            }
            SYM_SCHEMA => {
                // Parse from the schema_frames stream to register the schema,
                // then copy the frame bytes verbatim.
                let start = v.schema_frames.pos;
                let mut cur = Cursor::new(v.schema_frames.data);
                cur.pos = start;
                let tag = cur.u8().map_err(Error::Parse)?;
                if tag != TAG_SCHEMA {
                    return Err(Error::Corrupt("schema stream out of sync".into()));
                }
                let (type_id, meta) = parse_schema(&mut cur).map_err(Error::Parse)?;
                out.extend_from_slice(&v.schema_frames.data[start..cur.pos]);
                let group = groups.group_for(&v.schema_frames.data[start + 3..cur.pos])?;
                v.schema_frames.pos = cur.pos;
                schemas.insert(type_id, register(meta, group));
            }
            SYM_ANNOTATIONS => {
                let start = v.annotation_frames.pos;
                let mut cur = Cursor::new(v.annotation_frames.data);
                cur.pos = start;
                let tag = cur.u8().map_err(Error::Parse)?;
                if tag != TAG_SCHEMA_ANNOTATIONS {
                    return Err(Error::Corrupt("annotation stream out of sync".into()));
                }
                skip_annotations(&mut cur).map_err(Error::Parse)?;
                out.extend_from_slice(&v.annotation_frames.data[start..cur.pos]);
                v.annotation_frames.pos = cur.pos;
            }
            SYM_STRING_POOL => {
                out.push(TAG_STRING_POOL);
                let count = v.strpool_counts.varint()? as u32;
                out.extend_from_slice(&count.to_le_bytes());
                for _ in 0..count {
                    let delta = unzigzag(v.strpool_ids.varint()?) as i32 as u32;
                    let pool_id = prev_strpool_id.wrapping_add(delta);
                    prev_strpool_id = pool_id;
                    out.extend_from_slice(&pool_id.to_le_bytes());
                    let len = v.strpool_lens.varint()? as u32;
                    out.extend_from_slice(&len.to_le_bytes());
                    match v.strpool_flags.byte()? {
                        1 => {
                            let packed = v.strpool_uuids.take(16)?;
                            out.extend_from_slice(&unpack_uuid(packed));
                        }
                        _ => out.extend_from_slice(v.strpool_data.take(len as usize)?),
                    }
                }
            }
            SYM_STACK_POOL => {
                out.push(TAG_STACK_POOL);
                let count = v.stkpool_counts.varint()? as u32;
                out.extend_from_slice(&count.to_le_bytes());
                for _ in 0..count {
                    let delta = unzigzag(v.stkpool_ids.varint()?) as i32 as u32;
                    let pool_id = prev_stkpool_id.wrapping_add(delta);
                    prev_stkpool_id = pool_id;
                    out.extend_from_slice(&pool_id.to_le_bytes());
                    let fc = v.stkpool_framecounts.varint()? as u32;
                    out.extend_from_slice(&fc.to_le_bytes());
                    let mut prev = prev_first_addr;
                    for i in 0..fc {
                        let d = unzigzag(v.stkpool_addrs.varint()?);
                        let a = prev.wrapping_add(d as u64);
                        out.extend_from_slice(&a.to_le_bytes());
                        if i == 0 {
                            prev_first_addr = a;
                        }
                        prev = a;
                    }
                }
            }
            SYM_TS_RESET => {
                out.push(TAG_TIMESTAMP_RESET);
                let d = unzigzag(v.ts_reset.varint()?);
                let ts = prev_ts_reset.wrapping_add(d as u64);
                prev_ts_reset = ts;
                out.extend_from_slice(&ts.to_le_bytes());
            }
            ev => {
                let raw = ev - EVENT_SYM_BASE;
                if raw > u16::MAX as u64 {
                    return Err(Error::Corrupt(format!("bad frame symbol {ev}")));
                }
                let type_id = raw as u16;
                let reg = schemas.get(&type_id).ok_or_else(|| {
                    Error::Corrupt(format!("event for unregistered type_id {type_id}"))
                })?;
                out.push(TAG_EVENT);
                out.extend_from_slice(&type_id.to_le_bytes());
                if reg.meta.has_timestamp {
                    let delta = v
                        .ts_delta
                        .get_mut(&reg.group)
                        .ok_or_else(|| Error::Corrupt("missing ts_delta stream".into()))?
                        .varint()? as u32;
                    put_u24(&mut out, delta);
                }
                for (i, &ftag) in reg.meta.tags.iter().enumerate() {
                    let col = v
                        .columns
                        .get_mut(&(reg.group, i as u8))
                        .ok_or_else(|| Error::Corrupt("missing column".into()))?;
                    let mut inner = ftag;
                    if ftag & OPTIONAL_BIT != 0 {
                        let prefix = col.presence.byte()?;
                        out.push(prefix);
                        if prefix == 0 {
                            continue;
                        }
                        inner = ftag & !OPTIONAL_BIT;
                    }
                    match inner {
                        FT_I64 | FT_F64 => out.extend_from_slice(col.main.take(8)?),
                        FT_BOOL | FT_U8 => out.push(col.main.byte()?),
                        FT_U16 => out.extend_from_slice(col.main.take(2)?),
                        FT_U32 | FT_POOLED_STACK | FT_POOLED_STRING => {
                            let val = col.main.varint()? as u32;
                            out.extend_from_slice(&val.to_le_bytes());
                        }
                        FT_VARINT => {
                            let val = match reg.col_kinds[i] {
                                ColKind::VarintCached => id_cache.decode(&mut col.main)?,
                                _ => col.next_varint()?,
                            };
                            put_leb(&mut out, val);
                        }
                        FT_STRING | FT_BYTES => {
                            let len = col.main.varint()? as u32;
                            out.extend_from_slice(&len.to_le_bytes());
                            out.extend_from_slice(col.aux.take(len as usize)?);
                        }
                        FT_STACK_FRAMES => {
                            let count = col.main.varint()? as u32;
                            out.extend_from_slice(&count.to_le_bytes());
                            let mut prev = 0u64;
                            for _ in 0..count {
                                let d = unzigzag(col.main.varint()?);
                                let a = prev.wrapping_add(d as u64);
                                out.extend_from_slice(&a.to_le_bytes());
                                prev = a;
                            }
                        }
                        FT_STRING_MAP | FT_DYN_LIST | FT_DYN_MAP => {
                            copy_dynamic(&mut col.main, inner, &mut out)?;
                        }
                        other => {
                            return Err(Error::Corrupt(format!("unknown field tag {other}")));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_trace() -> Vec<u8> {
        let gz = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../dial9-viewer/ui/demo-trace.bin"
        ))
        .expect("demo trace present");
        let mut out = Vec::new();
        let mut dec = flate2::read::GzDecoder::new(&gz[..]);
        std::io::Read::read_to_end(&mut dec, &mut out).unwrap();
        out
    }

    #[test]
    fn round_trip_demo_trace_byte_exact() {
        let raw = demo_trace();
        let compressed = compress(&raw, 3).unwrap();
        let restored = decompress(&compressed).unwrap();
        assert_eq!(raw.len(), restored.len());
        assert!(raw == restored, "round trip is not byte-exact");
        assert!(
            compressed.len() < raw.len() / 3,
            "expected at least 3x compression, got {} -> {}",
            raw.len(),
            compressed.len()
        );
    }

    #[test]
    fn malformed_input_errors_cleanly() {
        assert!(compress(&[], 3).is_err());
        assert!(compress(b"not a trace file at all", 3).is_err());
        assert!(decompress(&[]).is_err());
        assert!(decompress(b"D9TCxxxxxxx").is_err());
        // Truncated but valid-magic container.
        let raw = demo_trace();
        let c = compress(&raw, 3).unwrap();
        assert!(decompress(&c[..c.len() / 2]).is_err());
    }

    #[test]
    fn zigzag_round_trip() {
        for v in [0i64, 1, -1, i64::MAX, i64::MIN, 12345, -98765] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn uuid_pack_round_trip() {
        let s = b"74f5d739-7397-489f-9fb7-6f9aa3986df3";
        let packed = pack_uuid(s).unwrap();
        assert_eq!(unpack_uuid(&packed), s.to_vec());
        // Uppercase and malformed strings must not pack.
        assert!(pack_uuid(b"74F5D739-7397-489F-9FB7-6F9AA3986DF3").is_none());
        assert!(pack_uuid(b"74f5d739-7397-489f-9fb7-6f9aa3986df").is_none());
        assert!(pack_uuid(b"74f5d739x7397-489f-9fb7-6f9aa3986df3").is_none());
    }
}
