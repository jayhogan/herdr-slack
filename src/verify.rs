//! `herdr-slack verify` (spec §7 "verify"): runs each search source raw and says, per source,
//! how many of Slack's matches the list explains. Prints channel labels and ts only, never text.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::model::{Kind, Msg, RowKey, Ts};
use crate::refresh::{mention_queries, search_after};
use crate::slack::{Slack, SlackError};
use crate::sources;
use crate::state::{Cache, Clears};

pub struct Report {
    pub text: String,
    pub unexplained: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Class {
    Mine,
    Ignored,
    Cleared,
    Row,
    Answered,
    Unexplained,
}

/// Reads already made for the answered check: (channel, thread root, read as DM history).
type Memo = BTreeMap<(String, Ts, bool), Vec<Msg>>;

pub fn run(
    s: &dyn Slack,
    cfg: &Config,
    cache: &Cache,
    clears: &Clears,
    now: DateTime<Utc>,
) -> Result<Report, SlackError> {
    let me = sources::me(s)?.user_id;
    let after = search_after(cfg, now);
    let queries = mention_queries(&me, cfg)
        .into_iter()
        .chain(["to:me".to_owned(), "from:me".to_owned()]);
    let mut memo = Memo::new();
    let mut lines = Vec::new();
    let mut unexplained = Vec::new();
    for query in queries {
        let query = format!("{query} {after}");
        let found = sources::search(s, &query)?;
        let mut counts = [0usize; 6];
        for m in &found.msgs {
            let class = classify(s, cfg, cache, clears, &me, m, &mut memo)?;
            counts[class as usize] += 1;
            if class == Class::Unexplained {
                unexplained.push(format!("  {} {}", label(cache, &m.channel), m.ts.0));
            }
        }
        let [mine, ignored, cleared, row, answered, none] = counts;
        let cut = if found.truncated {
            " · truncated at the search page cap"
        } else {
            ""
        };
        lines.push(format!(
            "{query}: {} matches · {row} in list · {mine} mine · {answered} answered · {cleared} cleared · {ignored} ignored · {none} unexplained{cut}",
            found.msgs.len(),
        ));
    }
    let in_list = cache
        .rows
        .iter()
        .filter(|r| r.kinds.contains(&Kind::Watch))
        .count();
    let (watched, _) = sources::watches(s, &me, &cfg.watch_emoji)?;
    lines.push(format!(
        "watches: {in_list} in list / {} watched",
        watched.len()
    ));
    let count = unexplained.len();
    lines.extend(unexplained);
    Ok(Report {
        text: lines.join("\n"),
        unexplained: count,
    })
}

/// Row keys a match can belong to, as `collect` keys them: its thread, and for a top-level
/// message also its DM row.
fn keys(m: &Msg) -> Vec<RowKey> {
    let mut keys = vec![RowKey::thread(&m.channel, &m.root())];
    if m.thread_ts.is_none() {
        keys.push(RowKey::dm(&m.channel));
    }
    keys
}

fn classify(
    s: &dyn Slack,
    cfg: &Config,
    cache: &Cache,
    clears: &Clears,
    me: &str,
    m: &Msg,
    memo: &mut Memo,
) -> Result<Class, SlackError> {
    let keys = keys(m);
    let cleared = keys
        .iter()
        .any(|k| clears.entries.get(k).is_some_and(|c| m.ts <= c.wm));
    Ok(if m.user == me {
        Class::Mine
    } else if cfg.ignore_users.contains(&m.user) {
        Class::Ignored
    } else if cleared {
        Class::Cleared
    } else if cache.rows.iter().any(|r| keys.contains(&r.key)) {
        Class::Row
    } else if answered(s, me, m, memo)? {
        Class::Answered
    } else {
        Class::Unexplained
    })
}

/// Whether `me` posted after `m` in its thread, or after it in a DM when it is top-level.
fn answered(s: &dyn Slack, me: &str, m: &Msg, memo: &mut Memo) -> Result<bool, SlackError> {
    // 1:1 and group DM ids; a top-level DM message is answered by any later message there.
    let dm = m.thread_ts.is_none() && m.channel.starts_with(['D', 'G']);
    let id = (m.channel.clone(), m.root(), dm);
    if !memo.contains_key(&id) {
        let read = if dm {
            sources::history(s, &m.channel, &m.ts)?
        } else {
            sources::replies(s, &m.channel, &m.root())?
        };
        memo.insert(id.clone(), read);
    }
    Ok(memo[&id].iter().any(|r| r.user == me && r.ts > m.ts))
}

fn label(cache: &Cache, channel: &str) -> String {
    cache
        .names
        .channels
        .get(channel)
        .cloned()
        .unwrap_or_else(|| channel.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Row, parse_search_match};
    use crate::slack::fake::FakeSlack;
    use crate::state::Clear;
    use chrono::TimeZone;
    use serde_json::{Value, json};

    const AFTER: &str = "after:2026-10-10";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
    }

    fn hit(channel: &str, ts: &str, user: &str, thread: Option<&str>) -> Value {
        let link = match thread {
            Some(t) => format!("https://heard.slack.com/archives/{channel}/p1?thread_ts={t}"),
            None => format!("https://heard.slack.com/archives/{channel}/p1"),
        };
        json!({"ts": ts, "text": "invented", "user": user, "channel": {"id": channel}, "permalink": link})
    }

    fn search_body(hits: &[Value]) -> String {
        json!({"ok": true, "messages": {
            "paging": {"total": hits.len(), "pages": 1}, "matches": hits}})
        .to_string()
    }

    fn row_for(hit: &Value) -> Row {
        let shown = parse_search_match(hit).unwrap();
        Row {
            key: RowKey::thread(&shown.channel, &shown.root()),
            channel: shown.channel.clone(),
            kinds: vec![Kind::Mention],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        }
    }

    fn fake(mentions: &[Value], to_me: &[Value]) -> FakeSlack {
        let q = |q: &str| format!("{q} {AFTER}");
        FakeSlack::new()
            .on(
                "auth.test",
                &[],
                Ok(r#"{"ok":true,"user_id":"U0ME","team_id":"T0HEARD"}"#),
            )
            .on("search.messages", &[], Ok(&search_body(&[])))
            .on(
                "search.messages",
                &[("query", &q("<@U0ME>"))],
                Ok(&search_body(mentions)),
            )
            .on(
                "search.messages",
                &[("query", &q("to:me"))],
                Ok(&search_body(to_me)),
            )
            .on("reactions.list", &[], Ok(r#"{"ok":true,"items":[]}"#))
    }

    #[test]
    fn verify_counts() {
        let listed = hit("C0CORE", "1792096026.000100", "U0DANA", None);
        let mine = hit("C0CORE", "1792096030.000100", "U0ME", None);
        let cleared = hit(
            "C0CORE",
            "1792096100.000300",
            "U0MARCUS",
            Some("1792096040.000200"),
        );
        let lost = hit("C0INC", "1792096200.000100", "U0PRIYA", None);
        let slack = fake(&[listed.clone(), mine, cleared, lost], &[]).on(
            "conversations.replies",
            &[("channel", "C0INC")],
            Ok(r#"{"ok":true,"messages":[{"ts":"1792096200.000100","user":"U0PRIYA","text":"x"}]}"#),
        );
        let mut cache = Cache::default();
        cache.rows.push(row_for(&listed));
        cache.names.channels.insert("C0CORE".into(), "#core".into());
        let mut clears = Clears::default();
        clears.entries.insert(
            RowKey::thread("C0CORE", &Ts("1792096040.000200".into())),
            Clear {
                wm: Ts("1792096100.000300".into()),
                keep_watch: false,
                at: now(),
            },
        );
        let report = run(&slack, &Config::default(), &cache, &clears, now()).unwrap();
        let lines: Vec<&str> = report.text.lines().collect();
        assert_eq!(
            lines[0],
            "<@U0ME> after:2026-10-10: 4 matches · 1 in list · 1 mine · 0 answered · 1 cleared · 0 ignored · 1 unexplained"
        );
        assert_eq!(
            lines[1],
            "to:me after:2026-10-10: 0 matches · 0 in list · 0 mine · 0 answered · 0 cleared · 0 ignored · 0 unexplained"
        );
        assert_eq!(lines[3], "watches: 0 in list / 0 watched");
        assert_eq!(lines[4], "  C0INC 1792096200.000100");
        assert_eq!(report.unexplained, 1);
        assert!(!report.text.contains("invented"));
    }

    #[test]
    fn verify_answered_ignored_and_dm_row() {
        let answered = hit(
            "C0CORE",
            "1792096026.000100",
            "U0DANA",
            Some("1792096000.000100"),
        );
        let ignored = hit("C0CORE", "1792096027.000100", "U0BOT", None);
        let dm = hit("D0DANA", "1792096028.000100", "U0DANA", None);
        let slack = fake(&[answered, ignored], &[dm]).on(
            "conversations.replies",
            &[("channel", "C0CORE")],
            Ok(r#"{"ok":true,"messages":[{"ts":"1792096000.000100","user":"U0DANA"},{"ts":"1792096026.000100","user":"U0DANA"},{"ts":"1792096050.000100","user":"U0ME"}]}"#),
        );
        let mut cache = Cache::default();
        let mut row = row_for(&hit("D0DANA", "1792096028.000100", "U0DANA", None));
        row.key = RowKey::dm("D0DANA");
        cache.rows.push(row);
        let cfg = Config {
            ignore_users: vec!["U0BOT".into()],
            ..Config::default()
        };
        let report = run(&slack, &cfg, &cache, &Clears::default(), now()).unwrap();
        let lines: Vec<&str> = report.text.lines().collect();
        assert!(lines[0].contains(
            "2 matches · 0 in list · 0 mine · 1 answered · 0 cleared · 1 ignored · 0 unexplained"
        ));
        assert!(lines[1].contains("1 matches · 1 in list"));
        assert_eq!(report.unexplained, 0);
    }

    #[test]
    fn verify_search_error_propagates() {
        let slack = FakeSlack::new().on(
            "auth.test",
            &[],
            Ok(r#"{"ok":true,"user_id":"U0ME","team_id":"T0HEARD"}"#),
        );
        let err = run(
            &slack,
            &Config::default(),
            &Cache::default(),
            &Clears::default(),
            now(),
        );
        assert!(err.is_err());
    }
}
