//! Data model shared by every module (spec §4 sources, §5 row model).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::text;

/// A Slack timestamp such as `"1792096026.762699"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ts(pub String);

impl Ts {
    fn parts(&self) -> Option<(u64, u32)> {
        let (s, m) = self.0.split_once('.').unwrap_or((&self.0, "0"));
        Some((s.parse().ok()?, m.parse().ok()?))
    }

    pub fn from_unix(secs: i64) -> Ts {
        Ts(format!("{secs}.000000"))
    }

    pub fn to_utc(&self) -> DateTime<Utc> {
        let (s, m) = self.parts().unwrap_or((0, 0));
        DateTime::from_timestamp(s as i64, m.saturating_mul(1000)).unwrap_or_default()
    }
}

impl Ord for Ts {
    fn cmp(&self, other: &Self) -> Ordering {
        // Unparsable sorts first (None < Some).
        self.parts()
            .cmp(&other.parts())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Ts {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Why a row is in the inbox. Declaration order is display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Kind {
    Watch,
    Mention,
    Reply,
    Thread,
    Channel,
    Dm,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Watch => "watch",
            Kind::Mention => "mention",
            Kind::Reply => "reply",
            Kind::Thread => "thread",
            Kind::Channel => "channel",
            Kind::Dm => "dm",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    pub channel: String,
    pub ts: Ts,
    /// The thread's root ts; `Some(ts)` on a root that has replies.
    pub thread_ts: Option<Ts>,
    pub reply_count: u32,
    /// `user`, else `bot_id`, else "".
    pub user: String,
    pub subtype: Option<String>,
    /// `text::flatten` output; still holds `<@U…>` tokens.
    pub body: String,
}

impl Msg {
    pub fn root(&self) -> Ts {
        self.thread_ts.clone().unwrap_or_else(|| self.ts.clone())
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_owned)
}

fn author(v: &Value) -> String {
    str_field(v, "user")
        .or_else(|| str_field(v, "bot_id"))
        .unwrap_or_default()
}

/// A `conversations.history` / `conversations.replies` element.
pub fn parse_message(channel: &str, v: &Value) -> Option<Msg> {
    Some(Msg {
        channel: channel.to_owned(),
        ts: Ts(str_field(v, "ts")?),
        thread_ts: str_field(v, "thread_ts").map(Ts),
        reply_count: v.get("reply_count").and_then(Value::as_u64).unwrap_or(0) as u32,
        user: author(v),
        subtype: str_field(v, "subtype"),
        body: text::flatten(v),
    })
}

/// A `search.messages` match. The thread comes from the permalink's `?thread_ts=`.
pub fn parse_search_match(v: &Value) -> Option<Msg> {
    let permalink = v.get("permalink").and_then(Value::as_str).unwrap_or("");
    let thread_ts = permalink
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("thread_ts=")))
        .map(|t| Ts(t.to_owned()));
    Some(Msg {
        channel: v.get("channel")?.get("id")?.as_str()?.to_owned(),
        ts: Ts(str_field(v, "ts")?),
        thread_ts,
        reply_count: 0,
        user: author(v),
        subtype: str_field(v, "subtype"),
        body: text::flatten(v),
    })
}

pub fn msg_id(channel: &str, ts: &Ts) -> String {
    format!("{channel}:{}", ts.0)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowKey(pub String);

impl RowKey {
    pub fn thread(channel: &str, root: &Ts) -> RowKey {
        RowKey(msg_id(channel, root))
    }

    pub fn dm(channel: &str) -> RowKey {
        RowKey(format!("{channel}:dm"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub key: RowKey,
    pub channel: String,
    /// Sorted, no duplicates.
    pub kinds: Vec<Kind>,
    /// Newest unhandled message; for a watch-only row, the watched message.
    pub shown: Msg,
    /// Oldest unhandled message (sort key "waiting").
    pub oldest: Ts,
    /// Newest message in the row (age label, clear watermark).
    pub newest: Ts,
    pub pending: usize,
    /// Ts of the message carrying the user's watch reaction.
    pub watched: Option<Ts>,
}

/// Id to display-name lookups. Channel values are display labels:
/// "#incidents", "@Dana Okafor", "@Ava Chen, Sam Patel".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Names {
    pub users: BTreeMap<String, String>,
    pub channels: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ts_orders_numerically() {
        assert!(Ts("1700000000.000100".into()) > Ts("999999999.999999".into()));
        assert!(Ts("1700000000.000100".into()) > Ts("1700000000.000099".into()));
    }

    #[test]
    fn ts_unparsable_sorts_first() {
        assert!(Ts("junk".into()) < Ts("1.000001".into()));
    }

    #[test]
    fn ts_unix_roundtrip_and_serde() {
        let t = Ts::from_unix(1_792_096_026);
        assert_eq!(t.to_utc().timestamp(), 1_792_096_026);
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"1792096026.000000\"");
    }

    #[test]
    fn search_match_thread_from_permalink() {
        let v = json!({
            "channel": {"id": "C0CORE"}, "ts": "1792096026.762699", "user": "U0DANA", "text": "hi",
            "permalink": "https://heard.slack.com/archives/C0CORE/p1792096026762699?thread_ts=1792000000.000100&cid=C0CORE"
        });
        let m = parse_search_match(&v).unwrap();
        assert_eq!(m.channel, "C0CORE");
        assert_eq!(m.thread_ts, Some(Ts("1792000000.000100".into())));
        assert_eq!(m.root(), Ts("1792000000.000100".into()));
    }

    #[test]
    fn search_match_top_level_has_no_thread() {
        let v = json!({
            "channel": {"id": "C0CORE"}, "ts": "1792096026.762699", "user": "U0DANA", "text": "hi",
            "permalink": "https://heard.slack.com/archives/C0CORE/p1792096026762699"
        });
        let m = parse_search_match(&v).unwrap();
        assert_eq!(m.thread_ts, None);
        assert_eq!(m.root(), m.ts);
    }

    #[test]
    fn parse_message_falls_back_to_bot_id() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/msg-attach.json")).unwrap();
        let m = parse_message("C0CORE", &v).unwrap();
        assert_eq!(m.user, "B0ALERT");
        assert_eq!(m.reply_count, 0);
        assert_eq!(msg_id(&m.channel, &m.ts), "C0CORE:1792096100.000200");
    }

    #[test]
    fn kind_order_and_labels() {
        let mut ks = [Kind::Dm, Kind::Watch, Kind::Thread, Kind::Mention];
        ks.sort();
        let labels: Vec<_> = ks.iter().map(|k| k.label()).collect();
        assert_eq!(labels, ["watch", "mention", "thread", "dm"]);
    }

    #[test]
    fn row_roundtrips_through_json() {
        let v =
            json!({"channel": {"id": "D0DANA"}, "ts": "1.000001", "user": "U0DANA", "text": "yo"});
        let shown = parse_search_match(&v).unwrap();
        let row = Row {
            key: RowKey::dm("D0DANA"),
            channel: "D0DANA".into(),
            kinds: vec![Kind::Dm],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        };
        let back: Row = serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn row_keys() {
        assert_eq!(RowKey::dm("D0DANA").0, "D0DANA:dm");
        assert_eq!(
            RowKey::thread("C0CORE", &Ts("1.000001".into())).0,
            "C0CORE:1.000001"
        );
    }
}
