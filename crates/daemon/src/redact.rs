//! Redaction: keep known secret values out of everything the daemon emits.
//!
//! The enforceable rule from the spec is that logs, exec output, status, errors,
//! and the audit never carry a secret value. The API never returns one either
//! (only the human reveal does), so redaction is the second line: a value can
//! still reach a log line because a Resource printed it, because a command echoed
//! it, or because a driver put it in an error message.
//!
//! A [`Redactor`] holds the exact values a Project's secrets currently have, plus
//! their percent-encoded forms, and replaces every occurrence with [`MASK`].
//! Percent-encoding matters because a value pasted into a URL or a DSN is no
//! longer the same bytes, and the naive redactor would miss it.
//!
//! Very short values are not redacted: a three-character secret would mask
//! unrelated text everywhere and make logs useless, and a value that short cannot
//! be a generated secret anyway.

use serde_json::Value;

/// What a redacted value is replaced with.
pub const MASK: &str = "[REDACTED]";

/// Values shorter than this are left alone: masking every `a` and `ab` in a log
/// would destroy the log without protecting anything.
pub const MIN_LENGTH: usize = 8;

/// The known values of one Project's secrets.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Every form to replace, longest first, so a value that contains another
    /// value is masked whole.
    patterns: Vec<String>,
}

impl Redactor {
    /// Nothing to redact: the common case, and a no-op fast path.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a redactor over the values a Project's secrets hold right now.
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut patterns: Vec<String> = Vec::new();
        for value in values {
            let value = value.as_ref();
            if value.chars().count() < MIN_LENGTH {
                continue;
            }
            for form in forms(value) {
                if !patterns.contains(&form) {
                    patterns.push(form);
                }
            }
        }
        patterns.sort_by(|lhs, rhs| rhs.len().cmp(&lhs.len()));
        Self { patterns }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Replace every known value, and every percent-encoded form of it.
    pub fn redact(&self, text: &str) -> String {
        if self.patterns.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for pattern in &self.patterns {
            if out.contains(pattern.as_str()) {
                out = out.replace(pattern.as_str(), MASK);
            }
        }
        out
    }

    /// Redact an optional string, keeping the option's shape.
    pub fn redact_opt(&self, text: Option<&str>) -> Option<String> {
        text.map(|text| self.redact(text))
    }

    /// Redact every string inside a JSON value, in place. Used on a response body
    /// so a field nobody thought about cannot leak a value.
    pub fn redact_json(&self, value: &mut Value) {
        if self.patterns.is_empty() {
            return;
        }
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            _ => {}
        }
    }
}

/// The value itself, plus the shapes it takes once it goes through a URL.
fn forms(value: &str) -> Vec<String> {
    let mut forms = vec![value.to_string()];
    let upper = percent_encode(value, true);
    if upper != value && !forms.contains(&upper) {
        forms.push(upper);
    }
    let lower = percent_encode(value, false);
    if lower != value && !forms.contains(&lower) {
        forms.push(lower);
    }
    forms
}

/// Percent-encode every byte outside the RFC 3986 unreserved set. Both hex cases
/// are tried, because encoders disagree.
fn percent_encode(raw: &str, upper: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other if upper => out.push_str(&format!("%{other:02X}")),
            other => out.push_str(&format!("%{other:02x}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_value_is_replaced_everywhere_it_appears() {
        let redactor = Redactor::new(["a-long-secret-value"]);
        assert_eq!(
            redactor.redact("connecting with a-long-secret-value now"),
            format!("connecting with {MASK} now")
        );
        assert_eq!(
            redactor.redact("a-long-secret-value and a-long-secret-value"),
            format!("{MASK} and {MASK}")
        );
        assert_eq!(redactor.redact("nothing to see"), "nothing to see");
    }

    #[test]
    fn the_percent_encoded_forms_of_a_value_are_replaced_too() {
        let redactor = Redactor::new(["p@ss word/+"]);
        for encoded in ["p%40ss%20word%2F%2B", "p%40ss%20word%2f%2b"] {
            assert_eq!(
                redactor.redact(&format!("dsn=postgres://u:{encoded}@host/db")),
                format!("dsn=postgres://u:{MASK}@host/db"),
                "{encoded} is the same secret"
            );
        }
    }

    #[test]
    fn short_values_are_left_alone_so_logs_stay_useful() {
        let redactor = Redactor::new(["ab", ""]);
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact("absolute"), "absolute");
    }

    #[test]
    fn the_longest_form_wins_so_a_prefix_does_not_leave_a_tail() {
        let redactor = Redactor::new(["abcdefgh", "abcdefghij"]);
        assert_eq!(redactor.redact("x abcdefghij y"), format!("x {MASK} y"));
    }

    #[test]
    fn a_json_body_loses_every_value_at_any_depth() {
        let redactor = Redactor::new(["deep-secret-value"]);
        let mut value = serde_json::json!({
            "top": "deep-secret-value",
            "nested": { "list": ["ok", "deep-secret-value"] },
            "count": 3,
        });
        redactor.redact_json(&mut value);
        assert_eq!(value["top"], MASK);
        assert_eq!(value["nested"]["list"][1], MASK);
        assert_eq!(value["nested"]["list"][0], "ok");
        assert_eq!(value["count"], 3);
        assert!(!value.to_string().contains("deep-secret-value"));
    }
}
