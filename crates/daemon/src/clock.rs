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

/// Now plus `millis` milliseconds, as RFC 3339 in UTC. An Approval's deadline is
/// measured this way: fifteen minutes by default, milliseconds in a test.
pub fn rfc3339_in_millis(millis: i64) -> String {
    format(OffsetDateTime::now_utc() + time::Duration::milliseconds(millis))
}

/// How long until `timestamp`, in milliseconds. `None` when it does not parse.
pub fn millis_until(timestamp: &str) -> Option<u64> {
    let at = parse(timestamp)?;
    let remaining = (at - OffsetDateTime::now_utc()).whole_milliseconds();
    Some(u64::try_from(remaining.max(0)).unwrap_or(0))
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

/// Parse an RFC 3339 timestamp written by this daemon.
pub fn parse(timestamp: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339).ok()
}

/// True when `timestamp` is older than `age`. An unparseable timestamp counts
/// as stale, so a corrupt row cannot make a replay live forever.
pub fn is_older_than(timestamp: &str, age: std::time::Duration) -> bool {
    let Some(at) = parse(timestamp) else {
        return true;
    };
    let age = time::Duration::seconds(age.as_secs().min(i64::MAX as u64) as i64);
    OffsetDateTime::now_utc() - at > age
}

/// Milliseconds between two RFC 3339 timestamps, when both parse.
pub fn millis_between(from: &str, to: &str) -> Option<u64> {
    let from = parse(from)?;
    let to = parse(to)?;
    let millis = (to - from).whole_milliseconds();
    u64::try_from(millis.max(0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_compares_rfc3339_timestamps() {
        assert!(is_expired("2000-01-01T00:00:00Z"));
        assert!(!is_expired(&rfc3339_in_days(1)));
    }
}
