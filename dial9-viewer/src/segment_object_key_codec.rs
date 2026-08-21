//! Viewer-side codec for Hive-style segment object keys.
//!
//! Encoding must mirror `dial9-destinations-s3/src/segment_object_key.rs`;
//! both crates pin the persisted layout with matching golden tests.

/// Escape a Hive partition-path value using Hive's canonical ASCII table.
pub(crate) fn hive_escape(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii() && needs_hive_escape(c as u8) {
            let byte = c as u8;
            escaped.push('%');
            escaped.push(HEX[(byte >> 4) as usize] as char);
            escaped.push(HEX[(byte & 0x0f) as usize] as char);
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// Decode one layer of `%HH` escaping from a Hive partition-path value.
pub(crate) fn hive_unescape(value: &str) -> Option<String> {
    if !value.as_bytes().contains(&b'%') {
        return Some(value.to_string());
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            decoded.push(bytes[i]);
            i += 1;
            continue;
        }
        let high = hex_value(*bytes.get(i + 1)?)?;
        let low = hex_value(*bytes.get(i + 2)?)?;
        decoded.push((high << 4) | low);
        i += 3;
    }
    String::from_utf8(decoded).ok()
}

/// Format the canonical Hive-style segment object key.
pub(crate) fn format_v1_segment_object_key(
    prefix: Option<&str>,
    date: &str,
    service: &str,
    time: &str,
    instance: &str,
    boot_id: &str,
    filename: &str,
) -> String {
    let suffix = format!(
        "version=1/date={}/service={}/time={}/instance={}/boot={}/{}",
        hive_escape(date),
        hive_escape(service),
        hive_escape(time),
        hive_escape(instance),
        hive_escape(boot_id),
        filename,
    );
    match prefix {
        Some(prefix) => format!("{prefix}/{suffix}"),
        None => suffix,
    }
}

fn needs_hive_escape(byte: u8) -> bool {
    byte < b' '
        || byte == 0x7f
        || matches!(
            byte,
            b'"' | b'#'
                | b'%'
                | b'\''
                | b'*'
                | b'/'
                | b':'
                | b'='
                | b'?'
                | b'\\'
                | b'{'
                | b'['
                | b']'
                | b'^'
        )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
