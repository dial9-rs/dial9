//! Canonical S3 object keys for trace segments.

use dial9_utils::object_key::hive_escape;

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

#[cfg(test)]
mod tests {
    use super::*;

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
