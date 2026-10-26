//! Time helpers: the whole codebase speaks RFC 3339 in UTC.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Current time as RFC 3339 in UTC.
pub fn now_rfc3339() -> String {
    format(OffsetDateTime::now_utc())
}

/// Now plus `days` days, as RFC 3339 in UTC.
pub fn rfc3339_in_days(days: i64) -> String {
    format(OffsetDateTime::now_utc() + time::Duration::days(days))
}

fn format(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// True when `timestamp` (RFC 3339, UTC, fixed precision) has passed.
pub fn is_expired(timestamp: &str) -> bool {
    timestamp <= now_rfc3339().as_str()
}
