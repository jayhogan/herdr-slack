//! One full refresh (spec §6 "Data fetching", §7 "Never miss a message"): every Slack read for
//! one refresh, fed to `collect`, giving the new cache.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use crate::collect::{Read, Reads, ThreadId, Windows, activity, collect};
use crate::config::Config;
use crate::model::{Kind, Msg, Names, Row, RowKey, Ts, msg_id};
use crate::slack::{Slack, SlackError};
use crate::sources::{self, Searched};
use crate::state::{Cache, Clears, State, WatchState};
use crate::{text, watch};

const VERSION: u32 = 1;
/// Threads with activity this recent are read every refresh; quieter ones every 5th.
const ACTIVE_DAYS: i64 = 2;
const QUIET_EVERY: u64 = 5;
/// Known threads stop being tracked after this long with no activity (§6 step 5).
const KNOWN_THREAD_DAYS: i64 = 14;
const MPIM_RELIST_HOURS: i64 = 6;
/// Every group DM is read at least this often (§6 step 7).
const MPIM_WINDOW_SECS: u64 = 600;

/// One full refresh (§6). Err only when the whole refresh is invalid (auth.test fails);
/// per-source failures become `Cache.warnings` and carried-forward rows.
pub fn run(
    s: &dyn Slack,
    cfg: &Config,
    state: &mut State,
    clears: &mut Clears,
    prev: &Cache,
    now: DateTime<Utc>,
) -> Result<Cache, SlackError> {
    let me = sources::me(s)?;
    let my = me.user_id.as_str();
    let mut warn = Vec::new();
    let mut names = prev.names.clone();
    let mention_floor = now - Duration::days(cfg.lookback_days.into());
    let win = Windows {
        mention_floor: unix(mention_floor),
        channel_floor: unix(now - Duration::hours(cfg.channel_lookback_hours.into())),
        retention_floor: unix(now - Duration::days(cfg.retention_days.into())),
    };
    // `after:` is day-granular, exclusive and in the user's timezone: one more day of overlap
    // keeps the whole window [floor, now] covered (§7 "Overlap").
    let after = sources::after(mention_floor - Duration::days(1));

    let watched = read_watches(s, my, cfg, &mut state.watch, &mut warn);

    let mut mention_hits = BTreeMap::new();
    let mut mention_errs = Vec::new();
    let queries = std::iter::once(format!("<@{my}>")).chain(cfg.extra_mention_queries.clone());
    for query in queries {
        match search(s, &query, &after, &mut warn) {
            Some(hits) => {
                mention_hits.extend(hits.into_iter().map(|m| (msg_id(&m.channel, &m.ts), m)))
            }
            None => mention_errs.push(query),
        }
    }
    let mention_hits: Vec<Msg> = mention_hits.into_values().collect();
    let to_me = search(s, "to:me", &after, &mut warn).unwrap_or_default();
    let from_me = search(s, "from:me", &after, &mut warn).unwrap_or_default();

    // DMs (before threads, so DM thread roots are read this refresh).
    if state
        .mpims_listed_at
        .is_none_or(|t| now - t > Duration::hours(MPIM_RELIST_HOURS))
    {
        match sources::mpims(s) {
            Ok(ids) => {
                state.mpims = ids;
                state.mpims_listed_at = Some(now);
            }
            Err(e) => warn.push(format!("group DMs: {}", describe(&e))),
        }
    }
    let dm_rows: BTreeSet<String> = prev
        .rows
        .iter()
        .filter(|r| r.kinds.contains(&Kind::Dm))
        .map(|r| r.channel.clone())
        .collect();
    let all_hits = mention_hits.iter().chain(&to_me).chain(&from_me);
    let mut dm_channels: BTreeSet<String> = state.mpims.iter().cloned().collect();
    dm_channels.extend(dm_rows.iter().cloned());
    dm_channels.extend(all_hits.map(|m| m.channel.clone()).filter(|c| is_im(c)));
    let (slice, cursor) = mpim_slice(&state.mpims, state.mpim_cursor, cfg.refresh_secs);
    state.mpim_cursor = cursor;
    let mut dm_reads: BTreeSet<String> = to_me
        .iter()
        .map(|m| m.channel.clone())
        .filter(|c| is_im(c))
        .collect();
    dm_reads.extend(dm_rows);
    dm_reads.extend(slice);
    let mut dms = BTreeMap::new();
    for ch in dm_reads {
        let read = sources::history(s, &ch, &win.mention_floor);
        let read = keep(read, &mut warn, &label(&names, &ch));
        dms.insert(ch, read);
    }

    // Threads: known ones on their schedule, new ones always.
    let mut hinted: BTreeMap<ThreadId, Ts> = BTreeMap::new();
    let mut hint = |channel: &str, root: &Ts, ts: &Ts| {
        let id = ThreadId {
            channel: channel.to_owned(),
            root: root.clone(),
        };
        let at = hinted.entry(id).or_insert_with(|| ts.clone());
        *at = at.clone().max(ts.clone());
    };
    for m in mention_hits.iter().chain(&from_me) {
        // A top-level hit outside a DM may be a root with replies (search can't say).
        if m.thread_ts.is_some() || !dm_channels.contains(&m.channel) {
            hint(&m.channel, &m.root(), &m.ts);
        }
    }
    for m in to_me.iter().filter(|m| m.thread_ts.is_some()) {
        hint(&m.channel, &m.root(), &m.ts);
    }
    for m in state.watch.watched.values() {
        hint(&m.channel, &m.root(), &m.ts);
    }
    for m in dms
        .values()
        .flatten()
        .flatten()
        .filter(|m| m.reply_count > 0)
    {
        hint(&m.channel, &m.ts, &m.ts);
    }
    let known = state.known_threads.iter().filter_map(|(key, ts)| {
        let (channel, root) = key.split_once(':')?;
        let id = ThreadId {
            channel: channel.to_owned(),
            root: Ts(root.to_owned()),
        };
        Some((id, ts.clone()))
    });
    let mut due: BTreeMap<ThreadId, bool> = BTreeMap::new();
    for (id, last) in known {
        let last = hinted
            .get(&id)
            .map_or(last.clone(), |h| h.clone().max(last));
        let key = msg_id(&id.channel, &id.root);
        due.insert(id, thread_due(&key, &last, state.refresh_no, now));
    }
    for id in hinted.into_keys() {
        due.entry(id).or_insert(true);
    }
    let mut threads = BTreeMap::new();
    for id in due.into_iter().filter(|(_, d)| *d).map(|(id, _)| id) {
        let read = sources::replies(s, &id.channel, &id.root);
        let source = format!("{} thread", label(&names, &id.channel));
        threads.insert(id, keep(read, &mut warn, &source));
    }

    let channels = read_channels(s, cfg, prev, &mut names, &win, &mut warn);

    let reads = Reads {
        me: my.to_owned(),
        ignore: cfg.ignore_users.iter().cloned().collect(),
        watched,
        mentions: if mention_errs.is_empty() {
            Ok(mention_hits)
        } else {
            Err(format!("failed: {}", mention_errs.join(", ")))
        },
        threads,
        channels,
        dms,
        dm_channels,
    };
    readd_watches(&reads, clears, now);
    let rows = collect(&reads, &prev.rows, clears, &win);

    for (id, ts) in activity(&reads) {
        let at = state
            .known_threads
            .entry(msg_id(&id.channel, &id.root))
            .or_insert_with(|| ts.clone());
        *at = at.clone().max(ts);
    }
    let expiry = unix(now - Duration::days(KNOWN_THREAD_DAYS));
    state.known_threads.retain(|_, ts| *ts >= expiry);

    let watched_rows = watch_rows(&state.watch);
    clears
        .entries
        .retain(|k, c| c.wm >= win.retention_floor || watched_rows.contains(k));

    let unresolved = resolve_names(s, my, &rows, &mut names);
    if unresolved > 0 {
        warn.push(format!("names: {unresolved} unresolved"));
    }
    state.version = VERSION;
    state.refresh_no += 1;
    Ok(Cache {
        version: VERSION,
        fetched_at: Some(now),
        error: None,
        error_at: None,
        warnings: warn,
        me: Some(me),
        rows,
        names,
    })
}

/// Quiet threads are read every 5th refresh, spread by a stable hash of the id.
pub fn thread_due(id: &str, last_activity: &Ts, refresh_no: u64, now: DateTime<Utc>) -> bool {
    if now - last_activity.to_utc() <= Duration::days(ACTIVE_DAYS) {
        return true;
    }
    let hash: u64 = id.bytes().map(u64::from).sum();
    refresh_no.wrapping_add(hash).is_multiple_of(QUIET_EVERY)
}

/// The group DMs to read this refresh, so every one is read within ~10 minutes:
/// `ceil(len / k)` ids from `cursor` (wrapping), `k = max(1, 600 / refresh_secs)`.
pub fn mpim_slice(all: &[String], cursor: usize, refresh_secs: u64) -> (Vec<String>, usize) {
    if all.is_empty() {
        return (Vec::new(), 0);
    }
    let k = (MPIM_WINDOW_SECS / refresh_secs.max(1)).max(1) as usize;
    let n = all.len().div_ceil(k);
    let start = cursor % all.len();
    let ids = (0..n)
        .map(|i| all[(start + i) % all.len()].clone())
        .collect();
    (ids, (start + n) % all.len())
}

fn unix(t: DateTime<Utc>) -> Ts {
    Ts::from_unix(t.timestamp())
}

/// A 1:1 DM id.
fn is_im(channel: &str) -> bool {
    channel.starts_with('D')
}

fn describe(e: &SlackError) -> String {
    match e {
        SlackError::Auth(code) => format!("auth: {code}"),
        SlackError::RateLimited(method) => format!("{method}: rate limited"),
        SlackError::Api(s) | SlackError::Http(s) => s.clone(),
    }
}

fn label(names: &Names, channel: &str) -> String {
    names
        .channels
        .get(channel)
        .cloned()
        .unwrap_or_else(|| channel.to_owned())
}

/// A failed read becomes `Err` (so collect carries its rows) plus a warning.
fn keep(
    read: Result<Vec<Msg>, SlackError>,
    warn: &mut Vec<String>,
    source: &str,
) -> Read<Vec<Msg>> {
    read.map_err(|e| {
        let e = describe(&e);
        warn.push(format!("{source}: {e}"));
        e
    })
}

/// `query` over the lookback window; None (and a warning) when it fails.
fn search(s: &dyn Slack, query: &str, after: &str, warn: &mut Vec<String>) -> Option<Vec<Msg>> {
    match sources::search(s, &format!("{query} {after}")) {
        Ok(Searched {
            msgs, truncated, ..
        }) => {
            if truncated {
                warn.push(format!("{query}: truncated"));
            }
            Some(msgs)
        }
        Err(e) => {
            warn.push(format!("{query}: {}", describe(&e)));
            None
        }
    }
}

/// Source 1: one reactions.list, re-checks for watches it may have cut off, then the watch
/// state. The read is the watched set after `watch::apply`.
fn read_watches(
    s: &dyn Slack,
    me: &str,
    cfg: &Config,
    ws: &mut WatchState,
    warn: &mut Vec<String>,
) -> Read<Vec<Msg>> {
    let (read, complete) = match sources::watches(s, me, &cfg.watch_emoji) {
        Ok(r) => r,
        Err(e) => {
            let e = describe(&e);
            warn.push(format!("watches: {e}"));
            return Err(e);
        }
    };
    let mut present = BTreeMap::new();
    for (channel, ts) in watch::to_recheck(ws, &read, complete) {
        match sources::reaction_present(s, me, &cfg.watch_emoji, &channel, &ts) {
            Ok(p) => {
                present.insert(msg_id(&channel, &ts), p);
            }
            Err(e) => warn.push(format!("watches: {}", describe(&e))),
        }
    }
    watch::apply(ws, &read, complete, &present);
    Ok(ws.watched.values().cloned().collect())
}

/// Source 6: curated channels' top-level posts in the channel window. `#name` entries resolve
/// through cached names, else one conversations.list for the refresh.
fn read_channels(
    s: &dyn Slack,
    cfg: &Config,
    prev: &Cache,
    names: &mut Names,
    win: &Windows,
    warn: &mut Vec<String>,
) -> BTreeMap<String, Read<Vec<Msg>>> {
    let mut listed: Option<Result<BTreeMap<String, String>, String>> = None;
    let mut unresolved = false;
    let mut out = BTreeMap::new();
    for entry in &cfg.channels {
        let id = match entry.strip_prefix('#') {
            None => entry.clone(),
            Some(name) => {
                let cached = names.channels.iter().find(|(_, l)| *l == entry);
                let found = match cached {
                    Some((id, _)) => Ok(Some(id.clone())),
                    None => listed
                        .get_or_insert_with(|| {
                            let list = sources::channels(s).map_err(|e| describe(&e))?;
                            Ok(list.into_iter().map(|(id, n)| (n, id)).collect())
                        })
                        .as_ref()
                        .map(|by_name| by_name.get(name).cloned()),
                };
                match found {
                    Ok(Some(id)) => {
                        names.channels.insert(id.clone(), entry.clone());
                        id
                    }
                    Ok(None) => {
                        warn.push(format!("{entry}: not found"));
                        unresolved = true;
                        continue;
                    }
                    Err(e) => {
                        warn.push(format!("{entry}: {e}"));
                        unresolved = true;
                        continue;
                    }
                }
            }
        };
        let read = sources::history(s, &id, &win.channel_floor);
        let read = keep(read, warn, &label(names, &id));
        out.insert(id, read);
    }
    if unresolved {
        // An entry we couldn't map to an id can't prove its rows resolved: keep them.
        for r in prev
            .rows
            .iter()
            .filter(|r| r.kinds.contains(&Kind::Channel))
        {
            out.entry(r.channel.clone())
                .or_insert_with(|| Err("channel not resolved".into()));
        }
    }
    out
}

fn watch_rows(ws: &WatchState) -> BTreeSet<RowKey> {
    ws.watched
        .values()
        .map(|m| RowKey::thread(&m.channel, &m.root()))
        .collect()
}

/// §9 note: a clear made before this refresh whose row is watched again now had its watch
/// re-added after `x`, so the clear must keep the watch.
fn readd_watches(reads: &Reads, clears: &mut Clears, now: DateTime<Utc>) {
    let Ok(ws) = &reads.watched else { return };
    let rows: BTreeSet<RowKey> = ws
        .iter()
        .map(|m| RowKey::thread(&m.channel, &m.root()))
        .collect();
    for (key, c) in &mut clears.entries {
        if !c.keep_watch && c.at < now && rows.contains(key) {
            c.keep_watch = true;
        }
    }
}

/// Source 8: names for every user and channel the rows show, skipping cached ones. Bot ids
/// aren't users.info ids; they stay raw. Returns how many ids stayed unresolved.
fn resolve_names(s: &dyn Slack, me: &str, rows: &[Row], names: &mut Names) -> usize {
    let mut users = BTreeSet::new();
    let mut chans = BTreeSet::new();
    for row in rows {
        let (u, c) = text::ids(&row.shown.body);
        users.insert(row.shown.user.clone());
        users.extend(u);
        chans.insert(row.channel.clone());
        chans.extend(c);
    }
    let mut unresolved = 0;
    for id in users {
        if id.is_empty() || id.starts_with('B') || names.users.contains_key(&id) {
            continue;
        }
        match sources::user_name(s, &id) {
            Ok(n) => {
                names.users.insert(id, n);
            }
            Err(_) => unresolved += 1,
        }
    }
    for id in chans {
        if names.channels.contains_key(&id) {
            continue;
        }
        let mut failed = false;
        let users = &mut names.users;
        let mut user = |u: &str| {
            if let Some(n) = users.get(u) {
                return n.clone();
            }
            match sources::user_name(s, u) {
                Ok(n) => {
                    users.insert(u.to_owned(), n.clone());
                    n
                }
                Err(_) => {
                    failed = true;
                    u.to_owned()
                }
            }
        };
        match sources::channel_label(s, me, &id, &mut user) {
            Ok(l) if !failed => {
                names.channels.insert(id, l);
            }
            _ => unresolved += 1,
        }
    }
    unresolved
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::Duration;

    use super::*;
    use crate::model::{Kind, Msg, Names, Row, RowKey};
    use crate::slack::fake::FakeSlack;
    use crate::sources;
    use crate::state::{Clear, WatchState};

    const NOW: i64 = 1_792_100_000;
    const USER: &str = include_str!("../tests/fixtures/user.json");
    const REACTIONS: &str = include_str!("../tests/fixtures/reactions.json");
    /// The watched message in `REACTIONS`.
    const WATCHED: &str = "C0CORE:1792096026.762699";
    const EMPTY: &str = r#"{"ok":true,"messages":[]}"#;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW, 0).unwrap()
    }

    fn ago(secs: i64) -> String {
        format!("{}.000000", NOW - secs)
    }

    const DAY: i64 = 86_400;

    /// The search query sent for `base` with the default 7-day lookback.
    fn q(base: &str) -> String {
        format!("{base} {}", sources::after(now() - Duration::days(8)))
    }

    fn msg(ts: &str, user: &str, text: &str) -> String {
        format!(r#"{{"type":"message","ts":"{ts}","user":"{user}","text":"{text}"}}"#)
    }

    fn root(ts: &str, user: &str, text: &str, replies: u32) -> String {
        format!(
            r#"{{"type":"message","ts":"{ts}","thread_ts":"{ts}","reply_count":{replies},"user":"{user}","text":"{text}"}}"#
        )
    }

    fn reply(ts: &str, root: &str, user: &str, text: &str) -> String {
        format!(
            r#"{{"type":"message","ts":"{ts}","thread_ts":"{root}","user":"{user}","text":"{text}"}}"#
        )
    }

    fn messages(ms: &[String]) -> String {
        format!(r#"{{"ok":true,"messages":[{}]}}"#, ms.join(","))
    }

    fn hit(ch: &str, ts: &str, user: &str, text: &str, thread: Option<&str>) -> String {
        let p = ts.replace('.', "");
        let q = thread.map_or(String::new(), |t| format!("?thread_ts={t}&cid={ch}"));
        format!(
            r#"{{"ts":"{ts}","user":"{user}","text":"{text}","channel":{{"id":"{ch}"}},"permalink":"https://heard.slack.com/archives/{ch}/p{p}{q}"}}"#
        )
    }

    fn found(hits: &[String]) -> String {
        format!(
            r#"{{"ok":true,"messages":{{"paging":{{"total":{},"pages":1}},"matches":[{}]}}}}"#,
            hits.len(),
            hits.join(",")
        )
    }

    /// Every source answers empty; tests add rules after it (later rules win).
    fn base() -> FakeSlack {
        FakeSlack::new()
            .on(
                "auth.test",
                &[],
                Ok(r#"{"ok":true,"user_id":"U0ME","team_id":"T0HEARD"}"#),
            )
            .on("reactions.list", &[], Ok(r#"{"ok":true,"items":[]}"#))
            .on("search.messages", &[], Ok(&found(&[])))
            .on("conversations.replies", &[], Ok(EMPTY))
            .on("conversations.history", &[], Ok(EMPTY))
            .on(
                "users.conversations",
                &[],
                Ok(r#"{"ok":true,"channels":[]}"#),
            )
            .on(
                "conversations.list",
                &[],
                Ok(r#"{"ok":true,"channels":[{"id":"C0INC","name":"incidents"}]}"#),
            )
            .on("users.info", &[], Ok(USER))
            .on(
                "conversations.info",
                &[],
                Ok(r#"{"ok":true,"channel":{"name":"core"}}"#),
            )
    }

    /// Baseline already recorded, so new watches count.
    fn seeded() -> State {
        State {
            watch: WatchState {
                baseline: Some(BTreeSet::new()),
                watched: BTreeMap::new(),
            },
            ..State::default()
        }
    }

    fn incidents() -> Config {
        Config {
            channels: vec!["#incidents".into()],
            ..Config::default()
        }
    }

    fn calls(fake: &FakeSlack, method: &str) -> Vec<Vec<(String, String)>> {
        fake.calls
            .borrow()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn has(params: &[(String, String)], k: &str, v: &str) -> bool {
        params.contains(&(k.to_owned(), v.to_owned()))
    }

    fn keys(cache: &Cache) -> Vec<&str> {
        cache.rows.iter().map(|r| r.key.0.as_str()).collect()
    }

    /// One watch, one mention, one DM and one curated-channel post.
    fn full_fake() -> FakeSlack {
        let mention = found(&[hit(
            "C0PLAT",
            "1792097000.000100",
            "U0MARCUS",
            "<@U0ME> review?",
            None,
        )]);
        let to_me = found(&[hit("D0DANA", "1792098000.000100", "U0DANA", "lunch?", None)]);
        base()
            .on("reactions.list", &[], Ok(REACTIONS))
            .on("search.messages", &[("query", &q("<@U0ME>"))], Ok(&mention))
            .on("search.messages", &[("query", &q("to:me"))], Ok(&to_me))
            .on(
                "conversations.history",
                &[("channel", "D0DANA")],
                Ok(&messages(&[msg("1792098000.000100", "U0DANA", "lunch?")])),
            )
            .on(
                "conversations.history",
                &[("channel", "C0INC")],
                Ok(&messages(&[msg(
                    "1792099000.000100",
                    "U0THEO",
                    "deploy failed",
                )])),
            )
    }

    #[test]
    fn full_refresh_builds_rows() {
        let fake = full_fake();
        let mut state = seeded();
        let mut clears = Clears::default();
        let cache = run(
            &fake,
            &incidents(),
            &mut state,
            &mut clears,
            &Cache::default(),
            now(),
        )
        .unwrap();
        assert_eq!(cache.warnings, Vec::<String>::new());
        assert_eq!(
            keys(&cache),
            [
                WATCHED,
                "C0INC:1792099000.000100",
                "C0PLAT:1792097000.000100",
                "D0DANA:dm"
            ]
        );
        let kinds: Vec<&[Kind]> = cache.rows.iter().map(|r| r.kinds.as_slice()).collect();
        assert_eq!(
            kinds,
            [
                &[Kind::Watch][..],
                &[Kind::Channel],
                &[Kind::Mention],
                &[Kind::Dm]
            ]
        );
        assert_eq!(state.refresh_no, 1);
        assert_eq!(cache.me.as_ref().unwrap().user_id, "U0ME");
        assert_eq!(cache.fetched_at, Some(now()));
        // Search date is two days before the 7-day floor's date.
        let date = (now() - Duration::days(9)).format("%Y-%m-%d");
        let searches = calls(&fake, "search.messages");
        assert!(has(&searches[0], "query", &format!("<@U0ME> after:{date}")));
        // `#incidents` resolved to its id and kept for next time.
        assert_eq!(cache.names.channels["C0INC"], "#incidents");
        assert_eq!(cache.names.users["U0MARCUS"], "Dana");
    }

    #[test]
    fn source_failure_becomes_warning() {
        let shown = Msg {
            channel: "C0INC".into(),
            ts: Ts("1792099000.000100".into()),
            thread_ts: None,
            reply_count: 0,
            user: "U0THEO".into(),
            subtype: None,
            body: "deploy failed".into(),
        };
        let row = Row {
            key: RowKey::thread("C0INC", &shown.ts),
            channel: "C0INC".into(),
            kinds: vec![Kind::Channel],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        };
        let prev = Cache {
            rows: vec![row.clone()],
            names: Names {
                users: BTreeMap::from([("U0THEO".into(), "Theo Marsh".into())]),
                channels: BTreeMap::from([("C0INC".into(), "#incidents".into())]),
            },
            ..Cache::default()
        };
        let fake = base().on(
            "conversations.history",
            &[("channel", "C0INC")],
            Err(SlackError::Api(
                "conversations.history: channel_not_found".into(),
            )),
        );
        let mut state = seeded();
        let cache = run(
            &fake,
            &incidents(),
            &mut state,
            &mut Clears::default(),
            &prev,
            now(),
        )
        .unwrap();
        assert_eq!(
            cache.warnings,
            ["#incidents: conversations.history: channel_not_found"]
        );
        assert_eq!(cache.rows, [row]);
        // The name came from the cached names, not a second conversations.list.
        assert!(calls(&fake, "conversations.list").is_empty());
    }

    #[test]
    fn failed_thread_read_warns_and_keeps_row() {
        let root_ts = Ts(ago(3600));
        let shown = Msg {
            channel: "C0CORE".into(),
            ts: Ts(ago(600)),
            thread_ts: Some(root_ts.clone()),
            reply_count: 0,
            user: "U0DANA".into(),
            subtype: None,
            body: "any update?".into(),
        };
        let key = RowKey::thread("C0CORE", &root_ts);
        let row = Row {
            key: key.clone(),
            channel: "C0CORE".into(),
            kinds: vec![Kind::Thread],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        };
        let prev = Cache {
            rows: vec![row.clone()],
            names: Names {
                users: BTreeMap::from([("U0DANA".into(), "Dana".into())]),
                channels: BTreeMap::from([("C0CORE".into(), "#core".into())]),
            },
            ..Cache::default()
        };
        let mut state = seeded();
        state.known_threads.insert(key.0.clone(), Ts(ago(600)));
        let fake = base().on(
            "conversations.replies",
            &[],
            Err(SlackError::RateLimited("conversations.replies".into())),
        );
        let cache = run(
            &fake,
            &Config::default(),
            &mut state,
            &mut Clears::default(),
            &prev,
            now(),
        )
        .unwrap();
        assert_eq!(
            cache.warnings,
            ["#core thread: conversations.replies: rate limited"]
        );
        assert_eq!(cache.rows, [row]);
        assert_eq!(state.known_threads[&key.0], Ts(ago(600)));
    }

    #[test]
    fn auth_failure_is_err() {
        let fake = base().on(
            "auth.test",
            &[],
            Err(SlackError::Auth("invalid_auth".into())),
        );
        let mut state = State {
            refresh_no: 3,
            ..seeded()
        };
        let before = state.clone();
        let mut clears = Clears::default();
        clears.entries.insert(
            RowKey(WATCHED.into()),
            Clear {
                wm: Ts(ago(30 * DAY)),
                keep_watch: false,
                at: now() - Duration::hours(1),
            },
        );
        let clears_before = clears.clone();
        let got = run(
            &fake,
            &Config::default(),
            &mut state,
            &mut clears,
            &Cache::default(),
            now(),
        );
        assert_eq!(got, Err(SlackError::Auth("invalid_auth".into())));
        assert_eq!(state, before);
        assert_eq!(clears, clears_before);
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn thread_due_schedule() {
        let recent = Ts(ago(3600));
        assert!((0..10).all(|n| thread_due("C0CORE:1.000001", &recent, n, now())));
        let quiet = Ts(ago(5 * DAY));
        for start in [0, 5, 7] {
            let due = (start..start + 5)
                .filter(|n| thread_due("C0CORE:1.000001", &quiet, *n, now()))
                .count();
            assert_eq!(due, 1, "window starting at {start}");
        }
    }

    #[test]
    fn mpim_slice_covers_all_in_window() {
        let all: Vec<String> = (0..160).map(|i| format!("G0MP{i:03}")).collect();
        let mut cursor = 0;
        let mut seen = BTreeSet::new();
        for _ in 0..3 {
            let (ids, next) = mpim_slice(&all, cursor, 180);
            assert_eq!(ids.len(), 54);
            seen.extend(ids);
            cursor = next;
        }
        assert_eq!(seen.len(), 160);
        assert_eq!(mpim_slice(&[], 4, 180), (vec![], 0));
        let one = vec!["G0MPDM1".to_string()];
        assert_eq!(mpim_slice(&one, 0, 900), (one.clone(), 0));
    }

    #[test]
    fn known_threads_expire_after_14_days_quiet() {
        let old = format!("C0OLD:{}", ago(15 * DAY));
        let mid = format!("C0CORE:{}", ago(3 * DAY));
        let mut state = seeded();
        state.known_threads = BTreeMap::from([
            (old.clone(), Ts(ago(15 * DAY))),
            (mid.clone(), Ts(ago(3 * DAY))),
        ]);
        // A new thread Jay posted in, whose read shows newer activity than the hit.
        let new_root = ago(DAY);
        let from_me = found(&[hit(
            "C0NEW",
            &ago(DAY - 60),
            "U0ME",
            "on it",
            Some(&new_root),
        )]);
        let fake = base()
            .on("search.messages", &[("query", &q("from:me"))], Ok(&from_me))
            .on(
                "conversations.replies",
                &[("channel", "C0NEW")],
                Ok(&messages(&[
                    root(&new_root, "U0DANA", "question", 2),
                    reply(&ago(DAY - 60), &new_root, "U0ME", "on it"),
                    reply(&ago(3600), &new_root, "U0DANA", "thanks"),
                ])),
            );
        run(
            &fake,
            &Config::default(),
            &mut state,
            &mut Clears::default(),
            &Cache::default(),
            now(),
        )
        .unwrap();
        let new = format!("C0NEW:{new_root}");
        assert_eq!(
            state.known_threads,
            BTreeMap::from([(mid, Ts(ago(3 * DAY))), (new, Ts(ago(3600)))])
        );
    }

    #[test]
    fn new_threads_read_immediately() {
        let quiet = format!("C0QUIET:{}", ago(5 * DAY));
        let new_root = ago(6 * DAY);
        let new_id = format!("C0CORE:{new_root}");
        let hit_ts = Ts(ago(5 * DAY - 100));
        // A refresh number where neither thread is due by schedule alone.
        let n = (0..5)
            .find(|n| {
                !thread_due(&quiet, &Ts(ago(5 * DAY)), *n, now())
                    && !thread_due(&new_id, &hit_ts, *n, now())
            })
            .unwrap();
        let mut state = State {
            refresh_no: n,
            ..seeded()
        };
        state.known_threads.insert(quiet.clone(), Ts(ago(5 * DAY)));
        let from_me = found(&[hit("C0CORE", &hit_ts.0, "U0ME", "noted", Some(&new_root))]);
        let fake = base().on("search.messages", &[("query", &q("from:me"))], Ok(&from_me));
        run(
            &fake,
            &Config::default(),
            &mut state,
            &mut Clears::default(),
            &Cache::default(),
            now(),
        )
        .unwrap();
        let read = calls(&fake, "conversations.replies");
        assert_eq!(read.len(), 1);
        assert!(has(&read[0], "channel", "C0CORE"));
        assert!(has(&read[0], "ts", &new_root));
    }

    #[test]
    fn readded_watch_flips_keep_watch() {
        let clear = |wm: String| Clear {
            wm: Ts(wm),
            keep_watch: false,
            at: now() - Duration::hours(1),
        };
        let mut clears = Clears::default();
        // Cleared with `x` long ago, watch since re-added: kept (watched) and flipped.
        clears
            .entries
            .insert(RowKey(WATCHED.into()), clear(ago(20 * DAY)));
        // Old and unwatched: pruned past retention.
        let stale = RowKey(format!("C0CORE:{}", ago(21 * DAY)));
        clears.entries.insert(stale.clone(), clear(ago(20 * DAY)));
        // Recent and unwatched: kept as is.
        let recent = RowKey("C0PLAT:1792090000.000100".into());
        clears.entries.insert(recent.clone(), clear(ago(DAY)));
        let fake = base().on("reactions.list", &[], Ok(REACTIONS));
        let mut state = seeded();
        run(
            &fake,
            &Config::default(),
            &mut state,
            &mut clears,
            &Cache::default(),
            now(),
        )
        .unwrap();
        assert!(clears.entries[&RowKey(WATCHED.into())].keep_watch);
        assert!(!clears.entries.contains_key(&stale));
        assert!(!clears.entries[&recent].keep_watch);
    }

    #[test]
    fn names_fetched_once() {
        let mut state = seeded();
        let first = full_fake();
        let cache = run(
            &first,
            &incidents(),
            &mut state,
            &mut Clears::default(),
            &Cache::default(),
            now(),
        )
        .unwrap();
        assert!(!calls(&first, "users.info").is_empty());
        let second = full_fake();
        let again = run(
            &second,
            &incidents(),
            &mut state,
            &mut Clears::default(),
            &cache,
            now(),
        )
        .unwrap();
        assert_eq!(keys(&again), keys(&cache));
        assert!(calls(&second, "users.info").is_empty());
        assert!(calls(&second, "conversations.info").is_empty());
        assert!(calls(&second, "conversations.list").is_empty());
        assert_eq!(state.refresh_no, 2);
    }

    #[test]
    fn dm_thread_roots_are_read() {
        let mut state = State {
            mpims: vec!["G0MPDM1".into()],
            mpims_listed_at: Some(now()),
            ..seeded()
        };
        let fake = base()
            .on(
                "conversations.history",
                &[("channel", "G0MPDM1")],
                Ok(&messages(&[root(
                    "1792097000.000100",
                    "U0AVA",
                    "who owns this?",
                    1,
                )])),
            )
            .on(
                "conversations.replies",
                &[("channel", "G0MPDM1")],
                Ok(&messages(&[
                    root("1792097000.000100", "U0AVA", "who owns this?", 1),
                    reply("1792097500.000200", "1792097000.000100", "U0SAM", "not me"),
                ])),
            );
        let cache = run(
            &fake,
            &Config::default(),
            &mut state,
            &mut Clears::default(),
            &Cache::default(),
            now(),
        )
        .unwrap();
        let read = calls(&fake, "conversations.replies");
        assert_eq!(read.len(), 1);
        assert!(has(&read[0], "ts", "1792097000.000100"));
        assert!(calls(&fake, "users.conversations").is_empty()); // listed < 6 h ago
        assert_eq!(keys(&cache), ["G0MPDM1:1792097000.000100"]);
        let row = &cache.rows[0];
        assert_eq!(row.kinds, [Kind::Dm]);
        assert_eq!(row.shown.user, "U0SAM");
        assert_eq!(row.pending, 2);
    }
}
