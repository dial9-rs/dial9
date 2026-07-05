//! Structure/byte-budget analysis of a raw trace file.

use crate::wire::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct FieldStat {
    bytes: u64,
    absent: u64,
    max: u64,
    monotonic_nondec: bool,
    prev: u64,
    seen: u64,
}

#[derive(Default)]
struct TypeStat {
    count: u64,
    bytes: u64,
    tags: Vec<u8>,
    field_names: Vec<String>,
    fields: Vec<FieldStat>,
}

pub fn analyze(data: &[u8]) -> Result<String> {
    use std::fmt::Write;
    let mut cur = Cursor::new(data);
    check_header(&mut cur)?;

    let mut schemas: SchemaMap = SchemaMap::new();
    let mut types: BTreeMap<String, TypeStat> = BTreeMap::new();
    let mut segments = 1u64;

    let mut kind_bytes: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new(); // (count, bytes)
    let mut ts_delta_hist = [0u64; 4]; // varint byte-length histogram for u24 deltas
    let mut string_pool_entries = 0u64;
    let mut string_pool_data_bytes = 0u64;
    let mut stack_pool_entries = 0u64;
    let mut stack_pool_frames = 0u64;
    let mut pool_id_monotonic = true;
    let mut prev_pool_id: Option<u32> = None;

    while !cur.eof() {
        let frame_start = cur.pos;
        // Mid-stream header = segment reset (concatenated thread-local batches).
        if cur.data.len() - cur.pos >= HEADER_SIZE && cur.data[cur.pos..cur.pos + 4] == MAGIC {
            cur.take(HEADER_SIZE)?;
            schemas.clear();
            segments += 1;
            prev_pool_id = None;
            let e = kind_bytes.entry("segment_header").or_default();
            e.0 += 1;
            e.1 += HEADER_SIZE as u64;
            continue;
        }
        let tag = cur.u8()?;
        let kind = match tag {
            TAG_SCHEMA => {
                let (type_id, meta) = parse_schema(&mut cur)?;
                let stat = types.entry(meta.name.clone()).or_default();
                if stat.fields.is_empty() {
                    stat.fields = (0..meta.tags.len()).map(|_| FieldStat::default()).collect();
                    stat.tags = meta.tags.clone();
                    stat.field_names = meta.field_names.clone();
                }
                schemas.insert(type_id, meta);
                "schema"
            }
            TAG_EVENT => {
                let type_id = cur.u16()?;
                let meta = schemas.get(&type_id).ok_or_else(|| ParseError {
                    pos: frame_start,
                    msg: format!("event with unknown type_id {type_id}"),
                })?;
                if meta.has_timestamp {
                    let delta = cur.u24()?;
                    let vlen = match delta {
                        0..=0x7f => 0,
                        0x80..=0x3fff => 1,
                        0x4000..=0x1f_ffff => 2,
                        _ => 3,
                    };
                    ts_delta_hist[vlen] += 1;
                }
                let stat = types.get_mut(&meta.name).unwrap();
                stat.count += 1;
                for (i, &tag) in meta.tags.iter().enumerate() {
                    let vstart = cur.pos;
                    let val = read_val(&mut cur, tag)?;
                    let fs = &mut stat.fields[i];
                    fs.bytes += (cur.pos - vstart) as u64;
                    let num = match val {
                        Val::None => {
                            fs.absent += 1;
                            None
                        }
                        Val::Varint(v) => Some(v),
                        Val::U8(v) => Some(v as u64),
                        Val::U16(v) => Some(v as u64),
                        Val::U32(v) => Some(v as u64),
                        Val::PoolId(v) => Some(v as u64),
                        Val::I64(v) => Some(v as u64),
                        _ => None,
                    };
                    if let Some(v) = num {
                        if fs.seen == 0 {
                            fs.monotonic_nondec = true;
                        } else if v < fs.prev {
                            fs.monotonic_nondec = false;
                        }
                        fs.prev = v;
                        fs.max = fs.max.max(v);
                        fs.seen += 1;
                    }
                }
                stat.bytes += (cur.pos - frame_start) as u64;
                "event"
            }
            TAG_STRING_POOL => {
                let count = cur.u32()?;
                for _ in 0..count {
                    let pool_id = cur.u32()?;
                    if let Some(p) = prev_pool_id
                        && pool_id <= p
                    {
                        pool_id_monotonic = false;
                    }
                    prev_pool_id = Some(pool_id);
                    let len = cur.u32()? as usize;
                    cur.take(len)?;
                    string_pool_entries += 1;
                    string_pool_data_bytes += len as u64;
                }
                "string_pool"
            }
            TAG_STACK_POOL => {
                let count = cur.u32()?;
                for _ in 0..count {
                    cur.u32()?; // pool_id
                    let fc = cur.u32()? as usize;
                    cur.take(fc * 8)?;
                    stack_pool_entries += 1;
                    stack_pool_frames += fc as u64;
                }
                "stack_pool"
            }
            TAG_TIMESTAMP_RESET => {
                cur.u64()?;
                "ts_reset"
            }
            TAG_SCHEMA_ANNOTATIONS => {
                skip_annotations(&mut cur)?;
                "annotations"
            }
            other => {
                return Err(ParseError {
                    pos: frame_start,
                    msg: format!("unknown frame tag {other:#x}"),
                });
            }
        };
        let e = kind_bytes.entry(kind).or_default();
        e.0 += 1;
        e.1 += (cur.pos - frame_start) as u64;
    }

    let mut out = String::new();
    writeln!(
        out,
        "total file bytes: {} in {segments} segments",
        data.len()
    )
    .unwrap();
    writeln!(out, "\n== frame kinds ==").unwrap();
    for (k, (count, bytes)) in &kind_bytes {
        writeln!(
            out,
            "{k:>14}: {count:>9} frames  {bytes:>10} bytes ({:.1}%)",
            *bytes as f64 / data.len() as f64 * 100.0
        )
        .unwrap();
    }
    writeln!(
        out,
        "\ntimestamp-delta varint sizes: 1B:{} 2B:{} 3B:{} 4B(u24 max):{}",
        ts_delta_hist[0], ts_delta_hist[1], ts_delta_hist[2], ts_delta_hist[3]
    )
    .unwrap();
    writeln!(
        out,
        "string pool: {string_pool_entries} entries, {string_pool_data_bytes} data bytes, ids monotonic within segment: {pool_id_monotonic}"
    )
    .unwrap();
    writeln!(
        out,
        "stack pool: {stack_pool_entries} entries, {stack_pool_frames} frames ({} bytes of addrs)",
        stack_pool_frames * 8
    )
    .unwrap();

    writeln!(out, "\n== event types ==").unwrap();
    let mut sorted: Vec<_> = types.iter().filter(|(_, s)| s.count > 0).collect();
    sorted.sort_by_key(|(_, s)| std::cmp::Reverse(s.bytes));
    for (name, stat) in sorted {
        writeln!(
            out,
            "\n{name} — {} events, {} bytes ({:.1}%)",
            stat.count,
            stat.bytes,
            stat.bytes as f64 / data.len() as f64 * 100.0,
        )
        .unwrap();
        for (i, fs) in stat.fields.iter().enumerate() {
            writeln!(
                out,
                "    .{i} tag={:<3} {:<28} {:>9} bytes  max={} absent={} mono={}",
                stat.tags[i],
                stat.field_names[i],
                fs.bytes,
                fs.max,
                fs.absent,
                fs.seen > 1 && fs.monotonic_nondec,
            )
            .unwrap();
        }
    }
    Ok(out)
}
