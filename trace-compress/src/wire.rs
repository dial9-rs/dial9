//! Minimal raw parser for the dial9 trace wire format.
//!
//! This mirrors `dial9-trace-format`'s codec, but exposes raw byte spans and
//! decoded scalars so a compressor can re-arrange them without allocating an
//! owned `FieldValue` per field.

use std::collections::HashMap;
use std::fmt;

pub const MAGIC: [u8; 4] = [0x54, 0x52, 0x43, 0x00]; // TRC\0
pub const HEADER_SIZE: usize = 5;

pub const TAG_SCHEMA: u8 = 0x01;
pub const TAG_EVENT: u8 = 0x02;
pub const TAG_STRING_POOL: u8 = 0x03;
pub const TAG_STACK_POOL: u8 = 0x04;
pub const TAG_TIMESTAMP_RESET: u8 = 0x05;
pub const TAG_SCHEMA_ANNOTATIONS: u8 = 0x06;

// Field type tags (see dial9-trace-format types.rs)
pub const FT_I64: u8 = 1;
pub const FT_F64: u8 = 2;
pub const FT_BOOL: u8 = 3;
pub const FT_STRING: u8 = 4;
pub const FT_BYTES: u8 = 5;
pub const FT_POOLED_STACK: u8 = 6;
pub const FT_POOLED_STRING: u8 = 7;
pub const FT_STACK_FRAMES: u8 = 8;
pub const FT_VARINT: u8 = 9;
pub const FT_STRING_MAP: u8 = 10;
pub const FT_U8: u8 = 11;
pub const FT_U16: u8 = 12;
pub const FT_U32: u8 = 13;
pub const FT_DYN_LIST: u8 = 14;
pub const FT_DYN_MAP: u8 = 15;
pub const OPTIONAL_BIT: u8 = 0x80;

#[derive(Debug)]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for ParseError {}

pub type Result<T> = std::result::Result<T, ParseError>;

#[derive(Clone)]
pub struct Cursor<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(ParseError {
            pos: self.pos,
            msg: msg.into(),
        })
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        match self.data.get(self.pos..self.pos + n) {
            Some(s) => {
                self.pos += n;
                Ok(s)
            }
            None => self.err(format!("truncated: wanted {n} bytes")),
        }
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn leb(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        let start = self.pos;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            if self.pos - start >= 10 {
                return self.err("LEB128 longer than 10 bytes");
            }
        }
    }
}

/// A decoded field value borrowing from the input. Composite self-describing
/// values (StringMap / DynamicList / DynamicMap) are kept as raw spans.
#[derive(Debug, Clone, Copy)]
pub enum Val<'a> {
    /// Absent optional field.
    None,
    I64(i64),
    F64Bits(u64),
    Bool(u8),
    /// String or Bytes payload (without the u32 length prefix).
    Blob(&'a [u8]),
    /// PooledString / PooledStackFrames id.
    PoolId(u32),
    Varint(u64),
    U8(u8),
    U16(u16),
    U32(u32),
    /// StackFrames: raw `count * 8` bytes of u64 LE addresses.
    StackFrames(&'a [u8]),
    /// StringMap / DynamicList / DynamicMap: full raw span including count.
    Raw(&'a [u8]),
}

/// Skip a self-describing dynamic value with the given (non-optional) tag,
/// returning nothing; the caller captures the span.
pub fn skip_dynamic(cur: &mut Cursor<'_>, tag: u8) -> Result<()> {
    match tag {
        FT_I64 | FT_F64 => {
            cur.take(8)?;
        }
        FT_BOOL | FT_U8 => {
            cur.take(1)?;
        }
        FT_U16 => {
            cur.take(2)?;
        }
        FT_U32 | FT_POOLED_STACK | FT_POOLED_STRING => {
            cur.take(4)?;
        }
        FT_VARINT => {
            cur.leb()?;
        }
        FT_STRING | FT_BYTES => {
            let len = cur.u32()? as usize;
            cur.take(len)?;
        }
        FT_STACK_FRAMES => {
            let count = cur.u32()? as usize;
            cur.take(count * 8)?;
        }
        FT_STRING_MAP => {
            let count = cur.u32()? as usize;
            for _ in 0..count {
                let klen = cur.u32()? as usize;
                cur.take(klen)?;
                let vlen = cur.u32()? as usize;
                cur.take(vlen)?;
            }
        }
        FT_DYN_LIST => {
            let count = cur.u32()? as usize;
            for _ in 0..count {
                let t = cur.u8()?;
                skip_dynamic(cur, t & !OPTIONAL_BIT)?;
            }
        }
        FT_DYN_MAP => {
            let count = cur.u32()? as usize;
            for _ in 0..count {
                let kt = cur.u8()?;
                skip_dynamic(cur, kt & !OPTIONAL_BIT)?;
                let vt = cur.u8()?;
                skip_dynamic(cur, vt & !OPTIONAL_BIT)?;
            }
        }
        other => return cur.err(format!("unknown field tag {other}")),
    }
    Ok(())
}

/// Read one field value of (possibly optional) `tag`. Returns the value;
/// `Val::None` for an absent optional.
pub fn read_val<'a>(cur: &mut Cursor<'a>, tag: u8) -> Result<Val<'a>> {
    if tag & OPTIONAL_BIT != 0 {
        let present = cur.u8()?;
        if present == 0 {
            return Ok(Val::None);
        }
        return read_val(cur, tag & !OPTIONAL_BIT);
    }
    Ok(match tag {
        FT_I64 => Val::I64(i64::from_le_bytes(cur.take(8)?.try_into().unwrap())),
        FT_F64 => Val::F64Bits(u64::from_le_bytes(cur.take(8)?.try_into().unwrap())),
        FT_BOOL => Val::Bool(cur.u8()?),
        FT_STRING | FT_BYTES => {
            let len = cur.u32()? as usize;
            Val::Blob(cur.take(len)?)
        }
        FT_POOLED_STACK | FT_POOLED_STRING => Val::PoolId(cur.u32()?),
        FT_VARINT => Val::Varint(cur.leb()?),
        FT_U8 => Val::U8(cur.u8()?),
        FT_U16 => Val::U16(cur.u16()?),
        FT_U32 => Val::U32(cur.u32()?),
        FT_STACK_FRAMES => {
            let count = cur.u32()? as usize;
            Val::StackFrames(cur.take(count * 8)?)
        }
        FT_STRING_MAP | FT_DYN_LIST | FT_DYN_MAP => {
            let start = cur.pos;
            skip_dynamic(cur, tag)?;
            Val::Raw(&cur.data[start..cur.pos])
        }
        other => return cur.err(format!("unknown field tag {other}")),
    })
}

#[derive(Debug, Clone)]
pub struct SchemaMeta {
    pub name: String,
    pub has_timestamp: bool,
    pub tags: Vec<u8>,
    pub field_names: Vec<String>,
}

/// Parse the schema frame body (cursor positioned after the tag byte).
/// Returns (type_id, meta).
pub fn parse_schema(cur: &mut Cursor<'_>) -> Result<(u16, SchemaMeta)> {
    let type_id = cur.u16()?;
    let name_len = cur.u16()? as usize;
    let name = String::from_utf8_lossy(cur.take(name_len)?).into_owned();
    let has_timestamp = cur.u8()? != 0;
    let field_count = cur.u16()? as usize;
    let mut tags = Vec::with_capacity(field_count);
    let mut field_names = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let fname_len = cur.u16()? as usize;
        field_names.push(String::from_utf8_lossy(cur.take(fname_len)?).into_owned());
        tags.push(cur.u8()?);
    }
    Ok((
        type_id,
        SchemaMeta {
            name,
            has_timestamp,
            tags,
            field_names,
        },
    ))
}

/// Skip the schema-annotations frame body (cursor positioned after the tag byte).
pub fn skip_annotations(cur: &mut Cursor<'_>) -> Result<()> {
    cur.leb()?; // type_id
    let count = cur.u16()? as usize;
    for _ in 0..count {
        cur.u16()?; // field_index
        let klen = cur.u16()? as usize;
        cur.take(klen)?;
        let vlen = cur.u32()? as usize;
        cur.take(vlen)?;
    }
    Ok(())
}

pub type SchemaMap = HashMap<u16, SchemaMeta>;

pub fn check_header(cur: &mut Cursor<'_>) -> Result<u8> {
    let magic = cur.take(4)?;
    if magic != MAGIC {
        return Err(ParseError {
            pos: 0,
            msg: "bad magic (is this file still gzipped?)".into(),
        });
    }
    cur.u8()
}
