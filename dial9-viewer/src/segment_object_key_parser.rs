//! Parsing for dial9 trace-segment object keys.

use crate::segment_object_key_codec::hive_unescape;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentObjectKeyLayout {
    Version1,
    PositionalWithBoot,
    PositionalWithoutBoot,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSegmentObjectKey {
    pub(crate) layout: SegmentObjectKeyLayout,
    pub(crate) date: Option<String>,
    pub(crate) time: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) instance: Option<String>,
    pub(crate) boot_id: Option<String>,
    pub(crate) filename: String,
    pub(crate) epoch_secs: Option<i64>,
    pub(crate) segment_index: Option<u32>,
}

/// Parse a dial9 segment object key, including historical layouts.
pub(crate) fn parse_segment_object_key(key: &str) -> ParsedSegmentObjectKey {
    let path = strip_s3(key);
    let parts: Vec<&str> = path.split('/').collect();
    let filename = parts.last().copied().unwrap_or_default().to_string();
    let (epoch_secs, segment_index) = parse_filename(&filename)
        .map(|(epoch, index)| (Some(epoch), Some(index)))
        .unwrap_or((None, None));

    if let Some((date, service, time, instance, boot_id)) = parse_version1(&parts) {
        return ParsedSegmentObjectKey {
            layout: SegmentObjectKeyLayout::Version1,
            date: Some(date),
            time: Some(time),
            service: Some(service),
            instance: Some(instance),
            boot_id: Some(boot_id),
            filename,
            epoch_secs,
            segment_index,
        };
    }

    if parts.len() >= 6 {
        let start = parts.len() - 6;
        if is_date(parts[start]) && is_time(parts[start + 1]) {
            return ParsedSegmentObjectKey {
                layout: SegmentObjectKeyLayout::PositionalWithBoot,
                date: Some(parts[start].to_string()),
                time: Some(parts[start + 1].to_string()),
                service: Some(parts[start + 2].to_string()),
                instance: Some(parts[start + 3].to_string()),
                boot_id: Some(parts[start + 4].to_string()),
                filename,
                epoch_secs,
                segment_index,
            };
        }
    }

    if parts.len() >= 5 {
        let start = parts.len() - 5;
        if is_date(parts[start]) && is_time(parts[start + 1]) {
            return ParsedSegmentObjectKey {
                layout: SegmentObjectKeyLayout::PositionalWithoutBoot,
                date: Some(parts[start].to_string()),
                time: Some(parts[start + 1].to_string()),
                service: Some(parts[start + 2].to_string()),
                instance: Some(parts[start + 3].to_string()),
                boot_id: None,
                filename,
                epoch_secs,
                segment_index,
            };
        }
    }

    ParsedSegmentObjectKey {
        layout: SegmentObjectKeyLayout::Unknown,
        date: None,
        time: None,
        service: None,
        instance: None,
        boot_id: None,
        filename,
        epoch_secs,
        segment_index,
    }
}

fn strip_s3(key: &str) -> &str {
    if let Some(rest) = key.strip_prefix("s3://") {
        rest.split_once('/').map_or(rest, |(_, path)| path)
    } else {
        key
    }
}

fn parse_version1(parts: &[&str]) -> Option<(String, String, String, String, String)> {
    if parts.len() < 7 {
        return None;
    }
    let start = parts.len() - 7;
    if parts[start] != "version=1" {
        return None;
    }
    let date = decode_partition(parts[start + 1], "date")?.filter(|value| is_date(value))?;
    let service = decode_partition(parts[start + 2], "service")??;
    let time = decode_partition(parts[start + 3], "time")?.filter(|value| is_time(value))?;
    let instance = decode_partition(parts[start + 4], "instance")??;
    let boot_id = decode_partition(parts[start + 5], "boot")??;
    Some((date, service, time, instance, boot_id))
}

fn decode_partition(segment: &str, name: &str) -> Option<Option<String>> {
    let encoded = segment.strip_prefix(name)?.strip_prefix('=')?;
    Some(hive_unescape(encoded))
}

pub(crate) fn has_version1_anchor(key: &str) -> bool {
    strip_s3(key).split('/').any(|part| part == "version=1")
}

fn parse_filename(filename: &str) -> Option<(i64, u32)> {
    let stem = filename
        .strip_suffix(".bin.gz")
        .or_else(|| filename.strip_suffix(".bin"))?;
    let (epoch, index) = stem.split_once('-')?;
    if index.contains('-') {
        return None;
    }
    Some((epoch.parse().ok()?, index.parse().ok()?))
}

fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..].iter().all(u8::is_ascii_digit)
}

fn is_time(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment_object_key_codec::{format_v1_segment_object_key, hive_escape};

    #[test]
    fn hive_escape_uses_the_complete_ascii_table() {
        let mut input: String = (0..=31).map(char::from).collect();
        input.push_str("\u{7f}\"#%'*/:=?\\{[]^");

        let escaped = hive_escape(&input);
        let expected_controls: String = (0..=31).map(|byte| format!("%{byte:02X}")).collect();
        assert_eq!(
            escaped,
            format!("{expected_controls}%7F%22%23%25%27%2A%2F%3A%3D%3F%5C%7B%5B%5D%5E")
        );
        assert_eq!(hive_unescape(&escaped).as_deref(), Some(input.as_str()));
    }

    #[test]
    fn hive_escape_round_trips_unicode_and_reserved_characters() {
        let value = "payments/api%=é";
        assert_eq!(hive_escape(value), "payments%2Fapi%25%3Dé");
        assert_eq!(hive_unescape(&hive_escape(value)).as_deref(), Some(value));
        assert_eq!(hive_unescape("caf%C3%A9").as_deref(), Some("café"));
    }

    #[test]
    fn hive_unescape_rejects_malformed_or_non_utf8_escapes() {
        for value in ["%", "%2", "%GG", "%FF"] {
            assert_eq!(hive_unescape(value), None, "{value:?}");
        }
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

    #[test]
    fn parses_canonical_version1_layout() {
        let parsed = parse_segment_object_key(
            "company/date=archive/%25/version=1/date=2026-08-14/service=payments%2Fapi/time=1937/instance=cluster%2Fworker%3Dblue/boot=abcd-4242/1786736220-3.bin.gz",
        );
        assert_eq!(parsed.layout, SegmentObjectKeyLayout::Version1);
        assert_eq!(parsed.date.as_deref(), Some("2026-08-14"));
        assert_eq!(parsed.time.as_deref(), Some("1937"));
        assert_eq!(parsed.service.as_deref(), Some("payments/api"));
        assert_eq!(parsed.instance.as_deref(), Some("cluster/worker=blue"));
        assert_eq!(parsed.boot_id.as_deref(), Some("abcd-4242"));
        assert_eq!(parsed.epoch_secs, Some(1_786_736_220));
        assert_eq!(parsed.segment_index, Some(3));
    }

    #[test]
    fn rejects_reordered_or_partial_version1_layouts() {
        let parsed = parse_segment_object_key(
            "traces/version=1/date=2026-08-14/time=1937/service=svc/instance=host%2Fone/boot=boot/1786736220-3.bin.gz",
        );
        assert_eq!(parsed.layout, SegmentObjectKeyLayout::Unknown);
        assert_eq!(parsed.epoch_secs, Some(1_786_736_220));

        let missing_boot_key = "traces/version=1/date=2026-08-14/service=svc/time=1937/instance=host/1786736220-3.bin.gz";
        let missing_boot = parse_segment_object_key(missing_boot_key);
        assert_eq!(missing_boot.layout, SegmentObjectKeyLayout::Unknown);
        assert!(has_version1_anchor(missing_boot_key));
    }

    #[test]
    fn parses_both_historical_layouts() {
        let with_boot = parse_segment_object_key(
            "service=prefix/traces/2026-04-09/1910/service/host/boot/1744224000-3.bin.gz",
        );
        assert_eq!(with_boot.layout, SegmentObjectKeyLayout::PositionalWithBoot);
        assert_eq!(with_boot.service.as_deref(), Some("service"));
        assert_eq!(with_boot.instance.as_deref(), Some("host"));
        assert_eq!(with_boot.boot_id.as_deref(), Some("boot"));

        let legacy =
            parse_segment_object_key("traces/2026-04-09/1910/service/host/1744224000-3.bin.gz");
        assert_eq!(legacy.layout, SegmentObjectKeyLayout::PositionalWithoutBoot);
        assert_eq!(legacy.boot_id, None);
    }

    #[test]
    fn unknown_layout_keeps_filename_metadata() {
        let parsed = parse_segment_object_key("custom/path/1744224000-3.bin.gz");
        assert_eq!(parsed.layout, SegmentObjectKeyLayout::Unknown);
        assert_eq!(parsed.filename, "1744224000-3.bin.gz");
        assert_eq!(parsed.epoch_secs, Some(1_744_224_000));
        assert_eq!(parsed.segment_index, Some(3));
    }
}
