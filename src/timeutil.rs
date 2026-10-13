//! Instants are stored as milliseconds since the Unix epoch, in UTC. Input is
//! RFC3339 only; bare unix timestamps are intentionally *not* accepted.

use chrono::{DateTime, SecondsFormat, Utc};

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn parse_instant(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

pub fn format_ms(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || "1970-01-01T00:00:00.000Z".to_string(),
        |dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_and_normalizes_offsets() {
        assert_eq!(parse_instant("1970-01-01T00:00:00Z"), Some(0));
        // +08:00 is midnight UTC minus 8h.
        assert_eq!(parse_instant("1970-01-01T08:00:00+08:00"), Some(0));
        assert!(parse_instant("2020-01-01").is_none());
        assert!(parse_instant("1609459200000").is_none());
    }

    #[test]
    fn formats_with_millis_and_z() {
        assert_eq!(format_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_ms(1_577_836_800_000), "2020-01-01T00:00:00.000Z");
    }
}
