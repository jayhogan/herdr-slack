//! Paged Slack sources (spec §4 findings, §6 "Refresh"): each function pages to the end of its
//! window and returns parsed `Msg`s.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::model::{Msg, Ts, parse_message, parse_search_match};
use crate::slack::{Slack, SlackError};
use crate::state::Me;

/// Slack stops serving `search.messages` past this page.
const SEARCH_PAGE_CAP: u64 = 100;
const WATCH_LIMIT: usize = 1000;

pub struct Searched {
    pub msgs: Vec<Msg>,
    pub total: u64,
    pub truncated: bool,
}

/// `after:YYYY-MM-DD` for the day before `floor`: Slack's filter is day-granular and exclusive.
pub fn after(floor: DateTime<Utc>) -> String {
    format!("after:{}", (floor - Duration::days(1)).format("%Y-%m-%d"))
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_str)
}

pub fn search(s: &dyn Slack, query: &str) -> Result<Searched, SlackError> {
    let mut out = Searched {
        msgs: Vec::new(),
        total: 0,
        truncated: false,
    };
    let mut page = 1;
    loop {
        let body = s.call(
            "search.messages",
            &[
                ("query", query.to_owned()),
                ("count", "100".into()),
                ("page", page.to_string()),
            ],
        )?;
        let messages = &body["messages"];
        let paging = &messages["paging"];
        let pages = paging["pages"].as_u64().unwrap_or(1);
        if page == 1 {
            out.total = paging["total"].as_u64().unwrap_or(0);
        }
        let matches = messages["matches"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        out.msgs
            .extend(matches.iter().filter_map(parse_search_match));
        if page >= pages {
            return Ok(out);
        }
        if page >= SEARCH_PAGE_CAP {
            out.truncated = true;
            return Ok(out);
        }
        page += 1;
    }
}

fn has_reaction(message: &Value, me: &str, emoji: &[String]) -> bool {
    message["reactions"].as_array().is_some_and(|rs| {
        rs.iter().any(|r| {
            let name = r["name"].as_str().unwrap_or("");
            emoji.iter().any(|e| e.trim_matches(':') == name)
                && r["users"]
                    .as_array()
                    .is_some_and(|us| us.iter().any(|u| u.as_str() == Some(me)))
        })
    })
}

/// The messages `me` reacted to with a watch emoji, from ONE `reactions.list` call (cursor paging
/// is broken, spec §4). The bool says the read held every reaction (fewer than the limit).
pub fn watches(s: &dyn Slack, me: &str, emoji: &[String]) -> Result<(Vec<Msg>, bool), SlackError> {
    let body = s.call(
        "reactions.list",
        &[("limit", WATCH_LIMIT.to_string()), ("full", "true".into())],
    )?;
    let items = body["items"].as_array().map_or(&[][..], Vec::as_slice);
    let msgs = items
        .iter()
        .filter(|i| i["type"].as_str() == Some("message"))
        .filter(|i| has_reaction(&i["message"], me, emoji))
        .filter_map(|i| parse_message(i["channel"].as_str()?, &i["message"]))
        .collect();
    Ok((msgs, items.len() < WATCH_LIMIT))
}

/// Whether `me` still has a watch emoji on one message.
pub fn reaction_present(
    s: &dyn Slack,
    me: &str,
    emoji: &[String],
    channel: &str,
    ts: &Ts,
) -> Result<bool, SlackError> {
    let body = s.call(
        "reactions.get",
        &[
            ("channel", channel.to_owned()),
            ("timestamp", ts.0.clone()),
            ("full", "true".into()),
        ],
    )?;
    Ok(has_reaction(&body["message"], me, emoji))
}

/// Runs `method` with `params` plus `limit=200`, following `next_cursor`, and returns every page's
/// `messages` array parsed for `channel`.
fn paged_messages(
    s: &dyn Slack,
    method: &str,
    channel: &str,
    params: &[(&str, String)],
) -> Result<Vec<Msg>, SlackError> {
    let mut msgs = Vec::new();
    let mut cursor = String::new();
    let mut pager = Pager::default();
    loop {
        let mut p = vec![("channel", channel.to_owned()), ("limit", "200".into())];
        p.extend(params.iter().cloned());
        if !cursor.is_empty() {
            p.push(("cursor", cursor.clone()));
        }
        let body = s.call(method, &p)?;
        let page = body["messages"].as_array().map_or(&[][..], Vec::as_slice);
        msgs.extend(page.iter().filter_map(|m| parse_message(channel, m)));
        cursor = pager.next(method, &body)?;
        if cursor.is_empty() {
            return Ok(msgs);
        }
    }
}

/// Slack's cursor paging, guarded: a repeated cursor or more than 1000 pages is an error
/// rather than an endless loop or a silent cap (§7).
#[derive(Default)]
struct Pager {
    seen: BTreeSet<String>,
    pages: u32,
}

const MAX_PAGES: u32 = 1000;

impl Pager {
    /// The cursor for the next page; empty at the end.
    fn next(&mut self, method: &str, body: &Value) -> Result<String, SlackError> {
        self.pages += 1;
        let cursor = str_at(body, &["response_metadata", "next_cursor"]).unwrap_or("");
        if cursor.is_empty() {
            return Ok(String::new());
        }
        if self.pages >= MAX_PAGES {
            return Err(SlackError::Api(format!("{method}: over {MAX_PAGES} pages")));
        }
        if !self.seen.insert(cursor.to_owned()) {
            return Err(SlackError::Api(format!("{method}: repeated cursor")));
        }
        Ok(cursor.to_owned())
    }
}

/// A thread: the root first, then its replies.
pub fn replies(s: &dyn Slack, channel: &str, root: &Ts) -> Result<Vec<Msg>, SlackError> {
    paged_messages(
        s,
        "conversations.replies",
        channel,
        &[("ts", root.0.clone())],
    )
}

/// Top-level messages (including thread roots) newer than `oldest`.
pub fn history(s: &dyn Slack, channel: &str, oldest: &Ts) -> Result<Vec<Msg>, SlackError> {
    let mut msgs = paged_messages(
        s,
        "conversations.history",
        channel,
        &[("oldest", oldest.0.clone())],
    )?;
    msgs.retain(|m| m.thread_ts.as_ref().is_none_or(|t| *t == m.ts));
    Ok(msgs)
}

/// Every `(id, name)` from a cursor-paged channel listing (`users.conversations`,
/// `conversations.list`) of `types`, archived excluded.
fn channel_list(
    s: &dyn Slack,
    method: &str,
    types: &str,
) -> Result<Vec<(String, String)>, SlackError> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    let mut pager = Pager::default();
    loop {
        let mut p = vec![
            ("types", types.to_owned()),
            ("exclude_archived", "true".into()),
            ("limit", "200".into()),
        ];
        if !cursor.is_empty() {
            p.push(("cursor", cursor.clone()));
        }
        let body = s.call(method, &p)?;
        let page = body["channels"].as_array().map_or(&[][..], Vec::as_slice);
        out.extend(page.iter().filter_map(|c| {
            let name = c["name"].as_str().unwrap_or("");
            Some((c["id"].as_str()?.to_owned(), name.to_owned()))
        }));
        cursor = pager.next(method, &body)?;
        if cursor.is_empty() {
            return Ok(out);
        }
    }
}

/// Ids of the group DMs `me` is in.
pub fn mpims(s: &dyn Slack) -> Result<Vec<String>, SlackError> {
    let list = channel_list(s, "users.conversations", "mpim")?;
    Ok(list.into_iter().map(|(id, _)| id).collect())
}

/// `(id, name)` of every public and private channel visible to the token.
pub fn channels(s: &dyn Slack) -> Result<Vec<(String, String)>, SlackError> {
    channel_list(s, "conversations.list", "public_channel,private_channel")
}

/// Display name, else real name, else handle.
pub fn user_name(s: &dyn Slack, id: &str) -> Result<String, SlackError> {
    let body = s.call("users.info", &[("user", id.to_owned())])?;
    let user = &body["user"];
    Ok([
        str_at(user, &["profile", "display_name"]),
        str_at(user, &["profile", "real_name"]),
        str_at(user, &["real_name"]),
        str_at(user, &["name"]),
    ]
    .into_iter()
    .flatten()
    .find(|n| !n.is_empty())
    .unwrap_or(id)
    .to_owned())
}

/// `@partner` for a DM, `@a, b` for a group DM (members minus `me`), `#name` otherwise.
/// `users` resolves a user id to a name (the caller caches).
pub fn channel_label(
    s: &dyn Slack,
    me: &str,
    id: &str,
    users: &mut dyn FnMut(&str) -> String,
) -> Result<String, SlackError> {
    let body = s.call("conversations.info", &[("channel", id.to_owned())])?;
    let ch = &body["channel"];
    if ch["is_im"].as_bool() == Some(true) {
        let partner = ch["user"].as_str().unwrap_or("");
        return Ok(format!("@{}", users(partner)));
    }
    if ch["is_mpim"].as_bool() == Some(true) {
        let members = s.call(
            "conversations.members",
            &[("channel", id.to_owned()), ("limit", "200".into())],
        )?;
        let names: Vec<String> = members["members"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(Value::as_str)
            .filter(|m| *m != me)
            .map(&mut *users)
            .collect();
        return Ok(format!("@{}", names.join(", ")));
    }
    let name = ch["name"].as_str().unwrap_or(id);
    Ok(format!("#{name}"))
}

pub fn me(s: &dyn Slack) -> Result<Me, SlackError> {
    let body = s.call("auth.test", &[])?;
    let field = |k: &str| {
        body[k]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| SlackError::Api(format!("auth.test: no {k}")))
    };
    Ok(Me {
        user_id: field("user_id")?,
        team_id: field("team_id")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::fake::FakeSlack;
    use chrono::TimeZone;
    use std::cell::RefCell;

    const SEARCH_P1: &str = include_str!("../tests/fixtures/search-p1.json");
    const SEARCH_P2: &str = include_str!("../tests/fixtures/search-p2.json");
    const REACTIONS: &str = include_str!("../tests/fixtures/reactions.json");
    const REACTIONS_GET: &str = include_str!("../tests/fixtures/reactions-get.json");
    const REPLIES: &str = include_str!("../tests/fixtures/replies.json");
    const HISTORY: &str = include_str!("../tests/fixtures/history.json");
    const HISTORY_P2: &str = include_str!("../tests/fixtures/history-p2.json");
    const MPIMS: &str = include_str!("../tests/fixtures/mpims.json");
    const USER: &str = include_str!("../tests/fixtures/user.json");

    fn eyes() -> Vec<String> {
        vec!["eyes".to_string()]
    }

    #[test]
    fn after_is_day_before() {
        let floor = Utc.with_ymd_and_hms(2026, 10, 19, 15, 30, 0).unwrap();
        assert_eq!(after(floor), "after:2026-10-18");
        let floor = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        assert_eq!(after(floor), "after:2026-02-28");
    }

    #[test]
    fn search_pages_to_end() {
        let fake = FakeSlack::new()
            .on("search.messages", &[("page", "1")], Ok(SEARCH_P1))
            .on("search.messages", &[("page", "2")], Ok(SEARCH_P2));
        let got = search(&fake, "<@U0ME>").unwrap();
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1.contains(&("page".into(), "1".into())));
        assert!(calls[0].1.contains(&("query".into(), "<@U0ME>".into())));
        assert!(calls[0].1.contains(&("count".into(), "100".into())));
        assert!(calls[1].1.contains(&("page".into(), "2".into())));
        assert_eq!(got.msgs.len(), 3);
        assert_eq!(got.total, 3);
        assert!(!got.truncated);
        assert_eq!(got.msgs[1].thread_ts, Some(Ts("1792096026.762699".into())));
        assert_eq!(got.msgs[2].channel, "D0DANA");
    }

    #[test]
    fn search_marks_truncated_at_cap() {
        let body = r#"{"ok":true,"messages":{"paging":{"total":15000,"pages":150},"matches":[]}}"#;
        let fake = FakeSlack::new().on("search.messages", &[], Ok(body));
        let got = search(&fake, "q").unwrap();
        assert_eq!(fake.calls.borrow().len(), 100);
        assert!(got.truncated);
        assert_eq!(got.total, 15000);
    }

    #[test]
    fn search_exactly_at_cap_is_not_truncated() {
        let body = r#"{"ok":true,"messages":{"paging":{"total":1,"pages":100},"matches":[]}}"#;
        let fake = FakeSlack::new().on("search.messages", &[], Ok(body));
        assert!(!search(&fake, "q").unwrap().truncated);
    }

    #[test]
    fn watches_one_call_limit_1000() {
        let fake = FakeSlack::new().on("reactions.list", &[], Ok(REACTIONS));
        let (msgs, complete) = watches(&fake, "U0ME", &eyes()).unwrap();
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "reactions.list");
        assert!(calls[0].1.contains(&("limit".into(), "1000".into())));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].channel, "C0CORE");
        assert_eq!(msgs[0].ts, Ts("1792096026.762699".into()));
        assert!(complete);
    }

    #[test]
    fn watches_incomplete_when_limit_reached() {
        let item = r#"{"type":"message","channel":"C0CORE","message":{"ts":"1.1","text":"x"}}"#;
        let body = format!(r#"{{"ok":true,"items":[{}]}}"#, vec![item; 1000].join(","));
        let fake = FakeSlack::new().on("reactions.list", &[], Ok(&body));
        let (msgs, complete) = watches(&fake, "U0ME", &eyes()).unwrap();
        assert!(msgs.is_empty());
        assert!(!complete);
    }

    #[test]
    fn watches_accept_colon_wrapped_emoji() {
        let fake = FakeSlack::new().on("reactions.list", &[], Ok(REACTIONS));
        let (msgs, _) = watches(&fake, "U0ME", &[":eyes:".to_string()]).unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn reaction_present_checks_me_and_emoji() {
        let fake = FakeSlack::new().on("reactions.get", &[], Ok(REACTIONS_GET));
        let ts = Ts("1792096026.762699".into());
        assert!(reaction_present(&fake, "U0ME", &eyes(), "C0CORE", &ts).unwrap());
        assert!(!reaction_present(&fake, "U0SAM", &eyes(), "C0CORE", &ts).unwrap());
        assert!(!reaction_present(&fake, "U0ME", &["tada".to_string()], "C0CORE", &ts).unwrap());
        let calls = fake.calls.borrow();
        assert!(calls[0].1.contains(&("timestamp".into(), ts.0.clone())));
    }

    #[test]
    fn history_follows_cursor_and_drops_replies() {
        let fake = FakeSlack::new()
            .on("conversations.history", &[], Ok(HISTORY))
            .on(
                "conversations.history",
                &[("cursor", "CUR2")],
                Ok(HISTORY_P2),
            );
        let got = history(&fake, "C0CORE", &Ts("1792000000.000000".into())).unwrap();
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[0]
                .1
                .contains(&("oldest".into(), "1792000000.000000".into()))
        );
        assert!(calls[0].1.contains(&("limit".into(), "200".into())));
        assert!(calls[1].1.contains(&("cursor".into(), "CUR2".into())));
        let ts: Vec<&str> = got.iter().map(|m| m.ts.0.as_str()).collect();
        assert_eq!(
            ts,
            [
                "1792096300.000500",
                "1792096100.000100",
                "1792096000.000200"
            ]
        );
    }

    /// T4: a `next_cursor` that comes back again, or paging past 1000 pages, is an error,
    /// not an endless loop or a silent cap.
    #[test]
    fn cursor_loops_are_errors() {
        let stuck =
            r#"{"ok":true,"messages":[],"channels":[],"response_metadata":{"next_cursor":"c1"}}"#;
        let fake = FakeSlack::new()
            .on("conversations.history", &[], Ok(stuck))
            .on("users.conversations", &[], Ok(stuck));
        assert_eq!(
            history(&fake, "C0CORE", &Ts("1".into())),
            Err(SlackError::Api(
                "conversations.history: repeated cursor".into()
            ))
        );
        assert_eq!(
            mpims(&fake),
            Err(SlackError::Api(
                "users.conversations: repeated cursor".into()
            ))
        );
        assert_eq!(fake.calls.borrow().len(), 4);

        /// A new cursor on every page, forever.
        struct Endless(RefCell<u32>);
        impl Slack for Endless {
            fn call(&self, _: &str, _: &[(&str, String)]) -> Result<Value, SlackError> {
                *self.0.borrow_mut() += 1;
                let n = *self.0.borrow();
                Ok(
                    serde_json::json!({"ok": true, "messages": [], "channels": [],
                    "response_metadata": {"next_cursor": format!("c{n}")}}),
                )
            }
        }
        let endless = Endless(RefCell::new(0));
        assert_eq!(
            replies(&endless, "C0CORE", &Ts("1".into())),
            Err(SlackError::Api(
                "conversations.replies: over 1000 pages".into()
            ))
        );
        assert_eq!(*endless.0.borrow(), 1000);
        let endless = Endless(RefCell::new(0));
        assert_eq!(
            channels(&endless),
            Err(SlackError::Api(
                "conversations.list: over 1000 pages".into()
            ))
        );
    }

    #[test]
    fn replies_returns_root_first() {
        let fake = FakeSlack::new().on("conversations.replies", &[], Ok(REPLIES));
        let root = Ts("1792096026.762699".into());
        let got = replies(&fake, "C0CORE", &root).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].ts, root);
        assert_eq!(got[0].user, "U0ME");
        assert_eq!(got[2].user, "U0SAM");
        assert!(
            fake.calls.borrow()[0]
                .1
                .contains(&("ts".into(), root.0.clone()))
        );
    }

    #[test]
    fn mpims_lists_ids() {
        let fake = FakeSlack::new().on("users.conversations", &[("types", "mpim")], Ok(MPIMS));
        assert_eq!(mpims(&fake).unwrap(), ["G0MPDM1", "G0MPDM2"]);
        assert!(
            fake.calls.borrow()[0]
                .1
                .contains(&("exclude_archived".into(), "true".into()))
        );
    }

    #[test]
    fn user_name_prefers_display_then_real_then_handle() {
        let fake = FakeSlack::new().on("users.info", &[], Ok(USER));
        assert_eq!(user_name(&fake, "U0DANA").unwrap(), "Dana");
        let no_display = r#"{"ok":true,"user":{"name":"marcus","real_name":"Marcus Lee","profile":{"display_name":""}}}"#;
        let fake = FakeSlack::new().on("users.info", &[], Ok(no_display));
        assert_eq!(user_name(&fake, "U0M").unwrap(), "Marcus Lee");
        let handle = r#"{"ok":true,"user":{"name":"bot","profile":{}}}"#;
        let fake = FakeSlack::new().on("users.info", &[], Ok(handle));
        assert_eq!(user_name(&fake, "U0B").unwrap(), "bot");
    }

    #[test]
    fn channel_label_kinds() {
        let mut names = |id: &str| format!("name-{id}");
        let fake = FakeSlack::new()
            .on(
                "conversations.info",
                &[("channel", "D0DANA")],
                Ok(r#"{"ok":true,"channel":{"is_im":true,"user":"U0DANA"}}"#),
            )
            .on(
                "conversations.info",
                &[("channel", "G0MPDM1")],
                Ok(r#"{"ok":true,"channel":{"is_mpim":true}}"#),
            )
            .on(
                "conversations.members",
                &[],
                Ok(r#"{"ok":true,"members":["U0ME","U0DANA","U0SAM"]}"#),
            )
            .on(
                "conversations.info",
                &[("channel", "C0CORE")],
                Ok(r#"{"ok":true,"channel":{"is_channel":true,"name":"core"}}"#),
            );
        assert_eq!(
            channel_label(&fake, "U0ME", "D0DANA", &mut names).unwrap(),
            "@name-U0DANA"
        );
        assert_eq!(
            channel_label(&fake, "U0ME", "G0MPDM1", &mut names).unwrap(),
            "@name-U0DANA, name-U0SAM"
        );
        assert_eq!(
            channel_label(&fake, "U0ME", "C0CORE", &mut names).unwrap(),
            "#core"
        );
    }

    #[test]
    fn me_reads_auth_test() {
        let fake = FakeSlack::new().on(
            "auth.test",
            &[],
            Ok(r#"{"ok":true,"user_id":"U0ME","team_id":"T0HEARD"}"#),
        );
        let m = me(&fake).unwrap();
        assert_eq!(
            (m.user_id.as_str(), m.team_id.as_str()),
            ("U0ME", "T0HEARD")
        );
        let fake = FakeSlack::new().on("auth.test", &[], Ok(r#"{"ok":true}"#));
        assert!(me(&fake).is_err());
    }

    #[test]
    fn unmatched_call_is_api_error() {
        assert_eq!(
            me(&FakeSlack::new()),
            Err(SlackError::Api("auth.test: no fake answer".into()))
        );
    }

    /// Run with `cargo test live_ -- --ignored --nocapture`. Prints counts only.
    #[test]
    #[ignore]
    fn live_auth_and_search() {
        let token = crate::token::read().expect("token");
        let s = crate::slack::HttpSlack::new(token, true);
        let who = me(&s).expect("auth.test");
        let floor = Utc::now() - Duration::days(7);
        let got = search(&s, &format!("from:me {}", after(floor))).expect("search");
        println!(
            "live: auth ok, team id len {}, search matches {} (total {}, truncated {})",
            who.team_id.len(),
            got.msgs.len(),
            got.total,
            got.truncated
        );
    }
}
