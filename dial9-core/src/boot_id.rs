//! Shared boot identifier for trace-segment namespacing and S3 keys.
//!
//! The on-disk per-process namespace (`{trace_dir}/{boot_id}/`, set up by the
//! telemetry crate) and the S3 object keys both stamp the same `boot_id`
//! format, so a process's segments are attributable across both.

/// Generate a boot identifier of the form `{4-alpha}-{pid}` (e.g. `qmxz-481`).
///
/// The 4 letters are derived from the current system-time nanoseconds; the pid
/// makes it unique among live processes.
pub fn generate_boot_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut v = nanos as u64;
    let mut s = String::with_capacity(10);
    for _ in 0..4 {
        s.push((b'a' + (v % 26) as u8) as char);
        v /= 26;
    }
    s.push_str(&format!("-{}", std::process::id()));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_boot_id_matches_pattern() {
        let id = generate_boot_id();
        let (alpha, pid) = id.split_once('-').unwrap();
        assert_eq!(alpha.len(), 4);
        assert!(alpha.chars().all(|c| c.is_ascii_lowercase()));
        assert!(!pid.is_empty());
        assert!(pid.chars().all(|c| c.is_ascii_digit()));
    }
}
