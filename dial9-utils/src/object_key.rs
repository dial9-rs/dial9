//! Shared Hive partition-path escaping for trace storage and sampling keys.

/// Escape a Hive partition-path value using Hive's canonical ASCII table.
///
/// This intentionally preserves empty strings instead of substituting Hive's
/// SQL null-partition sentinel: dial9 values are ordinary strings, not nullable
/// table cells.
pub fn hive_escape(value: &str) -> String {
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
}
