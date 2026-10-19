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
/// Group DMs are re-listed this often (one Tier-3 call), so a new one shows promptly.
const MPIM_RELIST_MINS: i64 = 10;
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
    let cursors = Cursors {
        last_ok: state.last_ok.clone(),
        now,
        retention_floor: unix(now - Duration::days(cfg.retention_days.into())),
    };
    let dm_back = Duration::days(cfg.lookback_days.into());
    let channel_back = Duration::hours(cfg.channel_lookback_hours.into());
    let mut ok = Vec::new(); // cursors to advance: their reads succeeded

    let watched = read_watches(s, my, cfg, &mut state.watch, &mut warn);

    let mut mention_hits = BTreeMap::new();
    let mut mentions_complete = true;
    // Rows older than the floor every mention query covered count as aged out (§7).
    let mut mention_floor = unix(now - dm_back);
    for query in mention_queries(my, cfg) {
        let key = format!("search:{query}");
        let floor = cursors.floor(&key, dm_back);
        mention_floor = mention_floor.max(floor.clone());
        // Hits always count; a failed or truncated query can't prove a mention resolved (§7).
        let (hits, complete) = search(s, &query, &floor, &mut warn).unwrap_or_default();
        mention_hits.extend(hits.into_iter().map(|m| (msg_id(&m.channel, &m.ts), m)));
        if complete {
            ok.push(key);
        } else {
            mentions_complete = false;
        }
    }
    let mention_hits: Vec<Msg> = mention_hits.into_values().collect();
    // These only pick what to read (absent reads carry rows), so partial hits are safe to use.
    let mut search_other = |query: &str| {
        let key = format!("search:{query}");
        let floor = cursors.floor(&key, dm_back);
        let (hits, complete) = search(s, query, &floor, &mut warn).unwrap_or_default();
        if complete {
            ok.push(key);
        }
        hits
    };
    let to_me = search_other("to:me");
    let from_me = search_other("from:me");
    let win = Windows {
        mention_floor,
        channel_floor: unix(now - channel_back),
        retention_floor: cursors.retention_floor.clone(),
    };

    // DMs (before threads, so DM thread roots are read this refresh).
    let mut new_mpims = Vec::new();
    if state
        .mpims_listed_at
        .is_none_or(|t| now - t >= Duration::minutes(MPIM_RELIST_MINS))
    {
        match sources::mpims(s) {
            Ok(ids) => {
                // Not in the previous list: read now rather than when the slice gets there.
                new_mpims = ids
                    .iter()
                    .filter(|id| !state.mpims.contains(id))
                    .cloned()
                    .collect();
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
    let elapsed = state.last_refresh_at.map(|t| (now - t).num_seconds());
    let (slice, cursor) = mpim_slice(&state.mpims, state.mpim_cursor, elapsed);
    state.mpim_cursor = cursor;
    let mut dm_reads: BTreeSet<String> = to_me
        .iter()
        .map(|m| m.channel.clone())
        .filter(|c| is_im(c))
        .collect();
    dm_reads.extend(dm_rows);
    dm_reads.extend(slice);
    dm_reads.extend(new_mpims);
    let mut dms = BTreeMap::new();
    for ch in dm_reads {
        let key = format!("ch:{ch}");
        let read = sources::history(s, &ch, &cursors.floor(&key, dm_back));
        if read.is_ok() {
            ok.push(key);
        }
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
    // Watched threads never expire while watched: they stay known, on the quiet schedule.
    let watched_threads: BTreeSet<String> =
        watch_rows(&state.watch).into_iter().map(|k| k.0).collect();
    // Within one quiet cycle of expiry, read every refresh: expiry needs a fresh read (§7).
    let cycle = Duration::seconds((QUIET_EVERY * cfg.refresh_secs) as i64);
    let near_expiry = unix(now - Duration::days(KNOWN_THREAD_DAYS) + cycle);
    let mut due: BTreeMap<ThreadId, bool> = BTreeMap::new();
    for (id, last) in known {
        let last = hinted
            .get(&id)
            .map_or(last.clone(), |h| h.clone().max(last));
        let key = msg_id(&id.channel, &id.root);
        let expiring = last <= near_expiry && !watched_threads.contains(&key);
        let d = expiring || thread_due(&key, &last, state.refresh_no, now);
        due.insert(id, d);
    }
    for id in hinted.keys() {
        due.entry(id.clone()).or_insert(true);
    }
    let mut threads = BTreeMap::new();
    let mut gone = BTreeSet::new();
    for id in due.into_iter().filter(|(_, d)| *d).map(|(id, _)| id) {
        let read = sources::replies(s, &id.channel, &id.root);
        if read.as_ref().is_err_and(is_gone) {
            gone.insert(id.clone());
        }
        let source = format!("{} thread", label(&names, &id.channel));
        threads.insert(id, keep(read, &mut warn, &source));
    }

    let channels = read_channels(s, cfg, prev, &mut names, &cursors, &mut ok, &mut warn);
    for key in ok {
        state.last_ok.insert(key, unix(now));
    }

    let reads = Reads {
        me: my.to_owned(),
        ignore: cfg.ignore_users.iter().cloned().collect(),
        watched,
        mentions: mention_hits,
        mentions_complete,
        threads,
        channels,
        dms,
        dm_channels,
        gone,
    };
    readd_watches(&reads, clears, now);
    let rows = collect(&reads, &prev.rows, clears, &win);

    // Every hinted thread is tracked, even when its read failed, so it outlives its search hit.
    for (id, ts) in hinted.into_iter().chain(activity(&reads)) {
        let at = state
            .known_threads
            .entry(msg_id(&id.channel, &id.root))
            .or_insert_with(|| ts.clone());
        *at = at.clone().max(ts);
    }
    // A gone thread is never read again (its warning shows this refresh only).
    for id in &reads.gone {
        state.known_threads.remove(&msg_id(&id.channel, &id.root));
    }
    // Expire only what a successful read this refresh shows quiet for 14 days.
    let expiry = unix(now - Duration::days(KNOWN_THREAD_DAYS));
    let read_ok: BTreeSet<String> = reads
        .threads
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(id, _)| msg_id(&id.channel, &id.root))
        .collect();
    state
        .known_threads
        .retain(|key, ts| *ts >= expiry || !read_ok.contains(key) || watched_threads.contains(key));

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
    state.last_refresh_at = Some(now);
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

/// The `after:` filter for a search window starting at `floor`. `after:` is day-granular,
/// exclusive and in the user's timezone: one more day of overlap keeps [floor, now] covered (§7).
fn after_filter(floor: &Ts) -> String {
    sources::after(floor.to_utc() - Duration::days(1))
}

/// The `after:` filter for the full lookback window (`verify`).
pub(crate) fn search_after(cfg: &Config, now: DateTime<Utc>) -> String {
    after_filter(&unix(now - Duration::days(cfg.lookback_days.into())))
}

/// Overlap kept before a source's last successful read (§7 "Overlap").
const CURSOR_OVERLAP_MINS: i64 = 10;

/// Per-source read cursors as they stood when this refresh started.
struct Cursors {
    last_ok: BTreeMap<String, Ts>,
    now: DateTime<Utc>,
    retention_floor: Ts,
}

impl Cursors {
    /// Where a read of `source` starts: the lookback, or 10 min before its last successful
    /// read when that is older, never past retention. So a gap (laptop closed, failed reads)
    /// is read on the next success.
    fn floor(&self, source: &str, lookback: Duration) -> Ts {
        let window = self.now - lookback;
        let from = self.last_ok.get(source).map_or(window, |t| {
            (t.to_utc() - Duration::minutes(CURSOR_OVERLAP_MINS)).min(window)
        });
        unix(from).max(self.retention_floor.clone())
    }
}

/// `<@me>` plus the configured extra mention queries.
pub(crate) fn mention_queries(my: &str, cfg: &Config) -> Vec<String> {
    std::iter::once(format!("<@{my}>"))
        .chain(cfg.extra_mention_queries.clone())
        .collect()
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
/// `ceil(len × elapsed / 600)` ids from `cursor` (wrapping), at least 1, at most all; all of
/// them when `elapsed` (seconds since the last refresh started) is unknown.
pub fn mpim_slice(all: &[String], cursor: usize, elapsed: Option<i64>) -> (Vec<String>, usize) {
    if all.is_empty() {
        return (Vec::new(), 0);
    }
    let len = all.len();
    let n = match elapsed {
        None => len,
        Some(secs) => {
            let secs = u64::try_from(secs).unwrap_or(0);
            let share = (len as u64).saturating_mul(secs).div_ceil(MPIM_WINDOW_SECS);
            usize::try_from(share).unwrap_or(len).clamp(1, len)
        }
    };
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

/// Error codes that say a thread is gone for good (deleted, or its channel is unreadable).
const GONE_CODES: [&str; 3] = ["thread_not_found", "channel_not_found", "not_in_channel"];

fn is_gone(e: &SlackError) -> bool {
    matches!(e, SlackError::Api(m) if m.rsplit(": ").next().is_some_and(|c| GONE_CODES.contains(&c)))
}

pub(crate) fn describe(e: &SlackError) -> String {
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

/// `query` from `floor`: its hits and whether they are complete (not truncated).
/// None (and a warning) when it fails.
fn search(
    s: &dyn Slack,
    query: &str,
    floor: &Ts,
    warn: &mut Vec<String>,
) -> Option<(Vec<Msg>, bool)> {
    match sources::search(s, &format!("{query} {}", after_filter(floor))) {
        Ok(Searched {
            msgs, truncated, ..
        }) => {
            if truncated {
                warn.push(format!("{query}: truncated"));
            }
            Some((msgs, !truncated))
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

/// Source 6: curated channels' top-level posts since each channel's cursor. `#name` entries
/// resolve through cached names, else one conversations.list for the refresh.
fn read_channels(
    s: &dyn Slack,
    cfg: &Config,
    prev: &Cache,
    names: &mut Names,
    cursors: &Cursors,
    ok: &mut Vec<String>,
    warn: &mut Vec<String>,
) -> BTreeMap<String, Read<Vec<Msg>>> {
    let lookback = Duration::hours(cfg.channel_lookback_hours.into());
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
        let key = format!("ch:{id}");
        let read = sources::history(s, &id, &cursors.floor(&key, lookback));
        if read.is_ok() {
            ok.push(key);
        }
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

    /// A previous-refresh row with one unhandled message.
    fn prev_row(key: &str, kind: Kind, ts: &str, root: Option<&str>, user: &str) -> Row {
        let channel = key.split_once(':').unwrap().0.to_owned();
        let shown = Msg {
            channel: channel.clone(),
            ts: Ts(ts.into()),
            thread_ts: root.map(|r| Ts(r.into())),
            reply_count: 0,
            user: user.into(),
            subtype: None,
            body: "waiting".into(),
        };
        Row {
            key: RowKey(key.into()),
            channel,
            kinds: vec![kind],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        }
    }

    fn prev_cache(row: &Row, channel_label: Option<&str>) -> Cache {
        let mut names = Names::default();
        names.users.insert(row.shown.user.clone(), "Dana".into());
        if let Some(l) = channel_label {
            names.channels.insert(row.channel.clone(), l.into());
        }
        Cache {
            rows: vec![row.clone()],
            names,
            ..Cache::default()
        }
    }

    fn run_with(fake: &FakeSlack, cfg: &Config, state: &mut State, prev: &Cache) -> Cache {
        run(fake, cfg, state, &mut Clears::default(), prev, now()).unwrap()
    }

    #[test]
    fn truncated_mention_search_keeps_rows() {
        let row = prev_row(
            "C0PLAT:1792097000.000100",
            Kind::Mention,
            "1792097000.000100",
            None,
            "U0MARCUS",
        );
        let prev = prev_cache(&row, Some("#platform"));
        let capped =
            r#"{"ok":true,"messages":{"paging":{"total":15000,"pages":150},"matches":[]}}"#;
        let fake = base().on("search.messages", &[("query", &q("<@U0ME>"))], Ok(capped));
        let cache = run_with(&fake, &Config::default(), &mut seeded(), &prev);
        assert_eq!(cache.warnings, ["<@U0ME>: truncated"]);
        assert_eq!(cache.rows, [row]);
    }

    #[test]
    fn failed_mention_queries_warn_and_keep_rows() {
        let row = prev_row(
            "C0PLAT:1792097000.000100",
            Kind::Mention,
            "1792097000.000100",
            None,
            "U0MARCUS",
        );
        let prev = prev_cache(&row, Some("#platform"));
        let cfg = Config {
            extra_mention_queries: vec!["@core-team".into(), "@oncall".into()],
            ..Config::default()
        };
        let down = || Err(SlackError::Http("search.messages: timed out".into()));
        let fake = base()
            .on("search.messages", &[("query", &q("@core-team"))], down())
            .on("search.messages", &[("query", &q("@oncall"))], down());
        let cache = run_with(&fake, &cfg, &mut seeded(), &prev);
        assert_eq!(
            cache.warnings,
            [
                "@core-team: search.messages: timed out",
                "@oncall: search.messages: timed out"
            ]
        );
        assert_eq!(cache.rows, [row]);
    }

    /// F4: one failing mention query still lets the others' new mentions through, and keeps
    /// the previous mention rows it might have held.
    #[test]
    fn one_failed_mention_query_keeps_the_others() {
        let old = prev_row(
            "C0PLAT:1792097000.000100",
            Kind::Mention,
            "1792097000.000100",
            None,
            "U0MARCUS",
        );
        let prev = prev_cache(&old, Some("#platform"));
        let cfg = Config {
            extra_mention_queries: vec!["@oncall".into()],
            ..Config::default()
        };
        let new = found(&[hit(
            "C0CORE",
            "1792099500.000100",
            "U0DANA",
            "<@U0ME> can you check?",
            None,
        )]);
        let fake = base()
            .on("search.messages", &[("query", &q("<@U0ME>"))], Ok(&new))
            .on(
                "search.messages",
                &[("query", &q("@oncall"))],
                Err(SlackError::Http("search.messages: timed out".into())),
            );
        let cache = run_with(&fake, &cfg, &mut seeded(), &prev);
        assert_eq!(cache.warnings, ["@oncall: search.messages: timed out"]);
        assert_eq!(
            keys(&cache),
            ["C0CORE:1792099500.000100", "C0PLAT:1792097000.000100"]
        );
        assert_eq!(cache.rows[0].kinds, [Kind::Mention]);
    }

    #[test]
    fn unresolved_channel_keeps_rows() {
        let row = prev_row(
            "C0INC:1792099000.000100",
            Kind::Channel,
            "1792099000.000100",
            None,
            "U0THEO",
        );
        // No cached "#incidents" label, so the name needs conversations.list.
        let prev = prev_cache(&row, None);
        let failed = base().on(
            "conversations.list",
            &[],
            Err(SlackError::Api("conversations.list: ratelimited".into())),
        );
        let cache = run_with(&failed, &incidents(), &mut seeded(), &prev);
        assert_eq!(
            cache.warnings,
            ["#incidents: conversations.list: ratelimited"]
        );
        assert_eq!(cache.rows, std::slice::from_ref(&row));
        let missing = base().on(
            "conversations.list",
            &[],
            Ok(r#"{"ok":true,"channels":[{"id":"C0CORE","name":"core"}]}"#),
        );
        let cache = run_with(&missing, &incidents(), &mut seeded(), &prev);
        assert_eq!(cache.warnings, ["#incidents: not found"]);
        assert_eq!(cache.rows, [row]);
    }

    #[test]
    fn failed_dm_history_keeps_row() {
        let row = prev_row("D0DANA:dm", Kind::Dm, "1792098000.000100", None, "U0DANA");
        let prev = prev_cache(&row, Some("@Dana"));
        let fake = base().on(
            "conversations.history",
            &[("channel", "D0DANA")],
            Err(SlackError::Http("conversations.history: timed out".into())),
        );
        let cache = run_with(&fake, &Config::default(), &mut seeded(), &prev);
        assert_eq!(cache.warnings, ["@Dana: conversations.history: timed out"]);
        assert_eq!(cache.rows, [row]);
    }

    fn param(params: &[(String, String)], k: &str) -> String {
        params.iter().find(|(key, _)| key == k).unwrap().1.clone()
    }

    fn history_oldest(fake: &FakeSlack, channel: &str) -> String {
        let read = calls(fake, "conversations.history");
        let p = read.iter().find(|p| has(p, "channel", channel)).unwrap();
        param(p, "oldest")
    }

    /// F1: a closed laptop (63 h > the 48 h channel window) leaves no gap: each read reaches
    /// back to 10 min before its last successful read, capped at retention.
    #[test]
    fn cursors_widen_reads_back_to_last_success() {
        let mut state = State {
            mpims: vec!["G0MPDM1".into()],
            mpims_listed_at: Some(now()),
            ..seeded()
        };
        let at = |secs: i64| Ts(ago(secs));
        state.last_ok = BTreeMap::from([
            ("ch:C0INC".into(), at(63 * 3600)),
            ("ch:G0MPDM1".into(), at(9 * DAY)),
            ("search:<@U0ME>".into(), at(10 * DAY)),
            ("search:to:me".into(), at(20 * DAY)), // past retention: capped
        ]);
        let friday = ago(63 * 3600 - 60);
        let fake = base().on(
            "conversations.history",
            &[("channel", "C0INC")],
            Ok(&messages(&[msg(&friday, "U0THEO", "db failover")])),
        );
        let cache = run_with(&fake, &incidents(), &mut state, &Cache::default());
        assert_eq!(history_oldest(&fake, "C0INC"), ago(63 * 3600 + 600));
        assert_eq!(history_oldest(&fake, "G0MPDM1"), ago(9 * DAY + 600));
        let searches = calls(&fake, "search.messages");
        let query = |base: &str| {
            searches
                .iter()
                .map(|p| param(p, "query"))
                .find(|q| q.starts_with(&format!("{base} ")))
                .unwrap()
        };
        let back = |secs: i64| sources::after(now() - Duration::seconds(secs) - Duration::days(1));
        assert_eq!(
            query("<@U0ME>"),
            format!("<@U0ME> {}", back(10 * DAY + 600))
        );
        assert_eq!(query("to:me"), format!("to:me {}", back(14 * DAY)));
        assert_eq!(query("from:me"), q("from:me")); // no cursor yet: the lookback
        assert_eq!(keys(&cache), [format!("C0INC:{friday}").as_str()]);
        // Every successful read moved its cursor to this refresh's start.
        for key in ["ch:C0INC", "ch:G0MPDM1", "search:<@U0ME>", "search:to:me"] {
            assert_eq!(state.last_ok[key], Ts(ago(0)), "{key}");
        }
        assert_eq!(state.last_ok["search:from:me"], Ts(ago(0)));
    }

    #[test]
    fn cursors_stay_put_when_a_read_fails() {
        let mut state = seeded();
        let old = Ts(ago(3 * DAY));
        state.last_ok = BTreeMap::from([
            ("ch:C0INC".into(), old.clone()),
            ("search:<@U0ME>".into(), old.clone()),
        ]);
        let capped =
            r#"{"ok":true,"messages":{"paging":{"total":15000,"pages":150},"matches":[]}}"#;
        let fake = base()
            .on(
                "conversations.history",
                &[("channel", "C0INC")],
                Err(SlackError::Http("conversations.history: timed out".into())),
            )
            .on("search.messages", &[], Ok(capped));
        run_with(&fake, &incidents(), &mut state, &Cache::default());
        assert_eq!(state.last_ok["ch:C0INC"], old);
        assert_eq!(state.last_ok["search:<@U0ME>"], old);
        assert!(!state.last_ok.contains_key("search:to:me"));
    }

    /// F7: group DMs are re-listed every 10 minutes, and a new one is read in that refresh.
    #[test]
    fn new_group_dm_read_when_listed() {
        let channels_of = |fake: &FakeSlack| -> Vec<String> {
            calls(fake, "conversations.history")
                .iter()
                .map(|p| param(p, "channel"))
                .collect()
        };
        let mpims: Vec<String> = (0..10).map(|i| format!("G0MP{i:03}")).collect();
        let mut listed = mpims.clone();
        listed.push("G0NEW".into());
        let body = format!(
            r#"{{"ok":true,"channels":[{}]}}"#,
            listed
                .iter()
                .map(|id| format!(r#"{{"id":"{id}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let state = State {
            mpims: mpims.clone(),
            last_refresh_at: Some(now() - Duration::seconds(50)), // slice of 1
            ..seeded()
        };
        // Listed 9 minutes ago: not yet.
        let mut fresh = State {
            mpims_listed_at: Some(now() - Duration::minutes(9)),
            ..state.clone()
        };
        let fake = base().on("users.conversations", &[], Ok(&body));
        run_with(&fake, &Config::default(), &mut fresh, &Cache::default());
        assert!(calls(&fake, "users.conversations").is_empty());
        assert_eq!(channels_of(&fake), ["G0MP000"]);
        // Listed 11 minutes ago: re-listed, and the new one read now.
        let mut stale = State {
            mpims_listed_at: Some(now() - Duration::minutes(11)),
            ..state
        };
        let fake = base().on("users.conversations", &[], Ok(&body));
        run_with(&fake, &Config::default(), &mut stale, &Cache::default());
        assert_eq!(calls(&fake, "users.conversations").len(), 1);
        assert_eq!(stale.mpims, listed);
        assert_eq!(channels_of(&fake), ["G0MP000", "G0NEW"]);
    }

    #[test]
    fn mpims_relisted_and_cursor_advances() {
        let mut state = State {
            mpims: vec!["G0STALE".into()],
            mpims_listed_at: Some(now() - Duration::hours(7)),
            last_refresh_at: Some(now() - Duration::seconds(300)), // half of the 10 min
            ..seeded()
        };
        let cfg = Config::default();
        let listed = r#"{"ok":true,"channels":[{"id":"G0MPDM1"},{"id":"G0MPDM2"}]}"#;
        let history_of = |fake: &FakeSlack| -> Vec<String> {
            calls(fake, "conversations.history")
                .iter()
                .flat_map(|p| {
                    p.iter()
                        .filter(|(k, _)| k == "channel")
                        .map(|(_, v)| v.clone())
                })
                .collect()
        };
        let first = base().on("users.conversations", &[], Ok(listed));
        run_with(&first, &cfg, &mut state, &Cache::default());
        assert_eq!(state.mpims, ["G0MPDM1", "G0MPDM2"]);
        assert_eq!(state.mpims_listed_at, Some(now()));
        assert_eq!(state.mpim_cursor, 1);
        // Both are new to the list, so both are read now.
        assert_eq!(history_of(&first), ["G0MPDM1", "G0MPDM2"]);
        let second = base();
        state.last_refresh_at = Some(now() - Duration::seconds(300));
        run_with(&second, &cfg, &mut state, &Cache::default());
        assert!(calls(&second, "users.conversations").is_empty());
        assert_eq!(state.mpim_cursor, 0);
        assert_eq!(history_of(&second), ["G0MPDM2"]);
    }

    #[test]
    fn near_expiry_thread_is_read_and_failed_read_never_expires() {
        let near_ts = ago(14 * DAY - 864); // 13.99 days
        let root_ts = ago(20 * DAY);
        let near = format!("C0NEAR:{root_ts}");
        let gone = format!("C0GONE:{}", ago(20 * DAY));
        // A refresh number where the hash schedule alone wouldn't read either.
        let n = (0..5)
            .find(|n| {
                !thread_due(&near, &Ts(near_ts.clone()), *n, now())
                    && !thread_due(&gone, &Ts(ago(15 * DAY)), *n, now())
            })
            .unwrap();
        let mut state = State {
            refresh_no: n,
            ..seeded()
        };
        state.known_threads = BTreeMap::from([
            (near.clone(), Ts(near_ts.clone())),
            (gone.clone(), Ts(ago(15 * DAY))),
        ]);
        let fake = base()
            .on(
                "conversations.replies",
                &[("channel", "C0NEAR")],
                Ok(&messages(&[
                    root(&root_ts, "U0ME", "rollout plan", 1),
                    reply(&near_ts, &root_ts, "U0DANA", "one more question"),
                ])),
            )
            .on(
                "conversations.replies",
                &[("channel", "C0GONE")],
                Err(SlackError::Http("conversations.replies: timed out".into())),
            );
        let cache = run_with(&fake, &Config::default(), &mut state, &Cache::default());
        assert_eq!(state.known_threads[&near], Ts(near_ts.clone()));
        assert_eq!(keys(&cache), [near.as_str()]);
        assert_eq!(cache.rows[0].kinds, [Kind::Reply]);
        assert_eq!(cache.rows[0].shown.user, "U0DANA");
        // Past 14 days but its read failed: still tracked.
        assert_eq!(state.known_threads[&gone], Ts(ago(15 * DAY)));
    }

    /// F6: a thread Slack says is gone stops being tracked, its rows go, and it warns only
    /// in the refresh that found out.
    #[test]
    fn gone_thread_is_dropped_and_warned_once() {
        let root_ts = ago(3 * DAY);
        let key = format!("C0CORE:{root_ts}");
        let row = prev_row(&key, Kind::Thread, &ago(2 * DAY), Some(&root_ts), "U0DANA");
        let prev = prev_cache(&row, Some("#core"));
        let mut state = seeded();
        state.known_threads.insert(key.clone(), Ts(ago(2 * DAY)));
        for code in ["thread_not_found", "channel_not_found", "not_in_channel"] {
            let mut state = state.clone();
            let gone = || {
                base().on(
                    "conversations.replies",
                    &[],
                    Err(SlackError::Api(format!("conversations.replies: {code}"))),
                )
            };
            let first = gone();
            let cache = run_with(&first, &Config::default(), &mut state, &prev);
            assert_eq!(
                cache.warnings,
                [format!("#core thread: conversations.replies: {code}")]
            );
            assert!(cache.rows.is_empty(), "{code}: {:?}", cache.rows);
            assert!(state.known_threads.is_empty(), "{code}");
            let second = gone();
            let again = run_with(&second, &Config::default(), &mut state, &cache);
            assert!(calls(&second, "conversations.replies").is_empty());
            assert!(again.warnings.is_empty(), "{:?}", again.warnings);
        }
    }

    /// A curated channel we can't read is a config problem: it warns every refresh, with one
    /// call per refresh.
    #[test]
    fn unreadable_channel_warns_each_refresh_with_one_call() {
        let mut state = seeded();
        let mut prev = Cache::default();
        for _ in 0..2 {
            let fake = base().on(
                "conversations.history",
                &[("channel", "C0INC")],
                Err(SlackError::Api(
                    "conversations.history: not_in_channel".into(),
                )),
            );
            prev = run_with(&fake, &incidents(), &mut state, &prev);
            assert_eq!(
                prev.warnings,
                ["#incidents: conversations.history: not_in_channel"]
            );
            assert_eq!(calls(&fake, "conversations.history").len(), 1);
        }
    }

    #[test]
    fn failed_new_thread_read_is_tracked() {
        let new_root = ago(DAY);
        let hit_ts = ago(DAY - 60);
        let from_me = found(&[hit("C0NEW", &hit_ts, "U0ME", "on it", Some(&new_root))]);
        let fake = base()
            .on("search.messages", &[("query", &q("from:me"))], Ok(&from_me))
            .on(
                "conversations.replies",
                &[("channel", "C0NEW")],
                Err(SlackError::Http("conversations.replies: timed out".into())),
            );
        let mut state = seeded();
        run_with(&fake, &Config::default(), &mut state, &Cache::default());
        assert_eq!(
            state.known_threads,
            BTreeMap::from([(format!("C0NEW:{new_root}"), Ts(hit_ts))])
        );
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

    /// F3: the slice is sized by the real time since the last refresh, so a slow refresh
    /// cycle still reads every group DM within ~10 minutes.
    #[test]
    fn mpim_slice_covers_all_in_window() {
        let all: Vec<String> = (0..160).map(|i| format!("G0MP{i:03}")).collect();
        let mut cursor = 0;
        let mut seen = BTreeSet::new();
        for _ in 0..3 {
            let (ids, next) = mpim_slice(&all, cursor, Some(200));
            assert_eq!(ids.len(), 54);
            seen.extend(ids);
            cursor = next;
        }
        assert_eq!(seen.len(), 160);
        // A 6.5-minute cycle reads ceil(160 * 390 / 600) = 104.
        assert_eq!(mpim_slice(&all, 0, Some(390)).0.len(), 104);
        // First run, or a long gap: all of them. At least one, always.
        assert_eq!(mpim_slice(&all, 7, None).0.len(), 160);
        let (ids, next) = mpim_slice(&all, 7, Some(5000));
        assert_eq!((ids.len(), next), (160, 7));
        assert_eq!(mpim_slice(&all, 0, Some(0)).0.len(), 1);
        assert_eq!(mpim_slice(&[], 4, Some(180)), (vec![], 0));
    }

    /// F3: a watched thread quiet past 14 days stays known and is read on the quiet schedule,
    /// not every refresh as if it were new.
    #[test]
    fn quiet_watched_thread_follows_schedule() {
        let root_ts = ago(20 * DAY);
        let key = format!("C0CORE:{root_ts}");
        let watched = Msg {
            channel: "C0CORE".into(),
            ts: Ts(root_ts.clone()),
            thread_ts: None,
            reply_count: 0,
            user: "U0DANA".into(),
            subtype: None,
            body: "keep an eye on this".into(),
        };
        let mut state = seeded();
        state.watch.watched.insert(key.clone(), watched.clone());
        state.known_threads.insert(key.clone(), Ts(root_ts.clone()));
        let reactions = format!(
            r#"{{"ok":true,"items":[{{"type":"message","channel":"C0CORE","message":{{"type":"message","ts":"{root_ts}","user":"U0DANA","text":"keep an eye on this","reactions":[{{"name":"eyes","users":["U0ME"]}}]}}}}]}}"#
        );
        let thread = messages(&[msg(&root_ts, "U0DANA", "keep an eye on this")]);
        let mut read_on = Vec::new();
        for n in 0..QUIET_EVERY {
            state.refresh_no = n;
            let fake = base().on("reactions.list", &[], Ok(&reactions)).on(
                "conversations.replies",
                &[],
                Ok(&thread),
            );
            let cache = run_with(&fake, &Config::default(), &mut state, &Cache::default());
            if !calls(&fake, "conversations.replies").is_empty() {
                read_on.push(n);
            }
            assert_eq!(keys(&cache), [key.as_str()]);
            assert!(state.known_threads.contains_key(&key), "refresh {n}");
        }
        assert_eq!(read_on.len(), 1, "read on {read_on:?}");
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
        assert!(calls(&fake, "users.conversations").is_empty()); // listed < 10 min ago
        assert_eq!(keys(&cache), ["G0MPDM1:1792097000.000100"]);
        let row = &cache.rows[0];
        assert_eq!(row.kinds, [Kind::Dm]);
        assert_eq!(row.shown.user, "U0SAM");
        assert_eq!(row.pending, 2);
    }
}
