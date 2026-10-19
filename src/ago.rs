//! Relative "2 min ago" labels for a row's first line (spec §9).

use chrono::{DateTime, Utc};

/// Spelled-out age: "just now", "5 min ago", "2 hours ago", "3 days ago". Future reads "just now".
pub fn long(ts: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - ts).num_seconds().max(0);
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    let mins = secs / 60;
    if mins < 1 {
        return "just now".into();
    }
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hrs = mins / 60;
    if hrs < 24 {
        return format!("{hrs} hour{} ago", plural(hrs));
    }
    let days = hrs / 24;
    format!("{days} day{} ago", plural(days))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn long_boundaries() {
        let s = |secs: i64| {
            let now = DateTime::parse_from_rfc3339("2026-10-18T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
            long(now - Duration::seconds(secs), now)
        };
        assert_eq!(s(0), "just now");
        assert_eq!(s(59), "just now");
        assert_eq!(s(60), "1 min ago");
        assert_eq!(s(59 * 60 + 59), "59 min ago");
        assert_eq!(s(3600), "1 hour ago");
        assert_eq!(s(2 * 3600), "2 hours ago");
        assert_eq!(s(24 * 3600), "1 day ago");
        assert_eq!(s(3 * 24 * 3600), "3 days ago");
        assert_eq!(s(-300), "just now");
    }
}
