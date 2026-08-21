//! Canonical S3 object keys for trace segments.

/// Escape a Hive partition-path value using Hive's canonical ASCII table.
///
/// This intentionally preserves empty strings instead of substituting Hive's
/// SQL null-partition sentinel: dial9 values are ordinary strings, not nullable
/// table cells.
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

/// Format the canonical Hive-style segment object key.
///
/// `prefix` and `filename` are inserted verbatim. Partition values are escaped
/// and emitted in the stable order used by time-scoped S3 listing.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hive_escape_uses_the_complete_ascii_table() {
        let mut input: String = (0..=31).map(char::from).collect();
        input.push_str("\u{7f}\"#%'*/:=?\\{[]^");

        let expected_controls: String = (0..=31).map(|byte| format!("%{byte:02X}")).collect();
        assert_eq!(
            hive_escape(&input),
            format!("{expected_controls}%7F%22%23%25%27%2A%2F%3A%3D%3F%5C%7B%5B%5D%5E")
        );
    }

    #[test]
    fn formats_canonical_version1_layout() {
        assert_eq!(
            format_v1_segment_object_key(
                Some("company/date=archive/%25"),
                "2026-08-14",
                "payments/api",
                "1937",
                "cluster/worker=blue",
                "boot%=1",
                "1786736220-3.bin.gz",
            ),
            "company/date=archive/%25/version=1/date=2026-08-14/service=payments%2Fapi/time=1937/instance=cluster%2Fworker%3Dblue/boot=boot%25%3D1/1786736220-3.bin.gz"
        );
    }
}
