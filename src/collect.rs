//! Turns one refresh's reads into rows (spec §5 "What goes in the list", §7 "Never miss a
//! message"; clears per §9 "Clearing").

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Kind, Msg, Row, RowKey, Ts, msg_id};
use crate::state::Clears;

pub type Read<T> = Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThreadId {
    pub channel: String,
    pub root: Ts,
}

pub struct Reads {
    pub me: String,
    pub ignore: BTreeSet<String>, // config.ignore_users
    pub watched: Read<Vec<Msg>>,  // WatchState.watched after watch::apply, or Err
    pub mentions: Read<Vec<Msg>>, // all mention queries merged
    pub threads: BTreeMap<ThreadId, Read<Vec<Msg>>>, // replies read this refresh (absent = not read)
    pub channels: BTreeMap<String, Read<Vec<Msg>>>,  // curated, top-level
    pub dms: BTreeMap<String, Read<Vec<Msg>>>, // IM / MPIM history, top-level (absent = not read)
    pub dm_channels: BTreeSet<String>,         // every channel id known to be an IM or MPIM
}

pub struct Windows {
    pub mention_floor: Ts,
    pub channel_floor: Ts,
    pub retention_floor: Ts,
}

/// Slack system messages that never create a row (§5).
const SKIP_SUBTYPES: &[&str] = &[
    "channel_join",
    "channel_leave",
    "channel_topic",
    "channel_purpose",
    "channel_name",
    "channel_archive",
    "channel_unarchive",
    "group_join",
    "group_leave",
    "group_topic",
    "group_purpose",
    "group_name",
    "pinned_item",
    "unpinned_item",
    "message_deleted",
    "message_changed",
    "reminder_add",
    "bot_add",
    "bot_remove",
    "joiner_notification_for_inviter",
];

/// One reason a message is waiting on Jay, on the row it belongs to.
struct Event<'a> {
    key: RowKey,
    channel: &'a str,
    kind: Kind,
    msg: &'a Msg,
}

pub fn collect(reads: &Reads, prev: &[Row], clears: &Clears, win: &Windows) -> Vec<Row> {
    let hints = hints(reads, prev);
    let mut events = Vec::new();
    watch_events(reads, &mut events);
    mention_events(reads, &hints, &mut events);
    thread_events(reads, &mut events);
    channel_events(reads, &mut events);
    dm_events(reads, &hints, &mut events);
    let mut rows = build(reads, clears, events);
    for p in apply_clears(prev.to_vec(), clears) {
        match rows.get_mut(&p.key) {
            Some(row) => merge(reads, row, p),
            None => {
                if let Some(row) = carry(reads, win, p) {
                    rows.insert(row.key.clone(), row);
                }
            }
        }
    }
    rows.into_values().collect()
}

/// What the UI shows between refreshes: hides rows a newer clear covers (§9).
pub fn apply_clears(rows: Vec<Row>, clears: &Clears) -> Vec<Row> {
    rows.into_iter()
        .filter_map(|mut row| match clears.entries.get(&row.key) {
            Some(c) if row.newest <= c.wm => (c.keep_watch && row.kinds.contains(&Kind::Watch))
                .then(|| {
                    row.kinds = vec![Kind::Watch];
                    row
                }),
            _ => Some(row),
        })
        .collect()
}

/// Threads Jay is in, from a refresh's reads: thread id → last activity.
pub fn activity(reads: &Reads) -> BTreeMap<ThreadId, Ts> {
    reads
        .threads
        .iter()
        .filter_map(|(id, read)| {
            let newest = read.as_ref().ok()?.iter().map(|m| &m.ts).max()?;
            Some((id.clone(), newest.clone()))
        })
        .collect()
}

fn is_top_level(m: &Msg) -> bool {
    m.thread_ts.as_ref().is_none_or(|t| *t == m.ts)
}

/// Rule 1: Jay's own, ignored senders and system subtypes never count.
fn counts(reads: &Reads, m: &Msg) -> bool {
    m.user != reads.me
        && !reads.ignore.contains(&m.user)
        && !m
            .subtype
            .as_deref()
            .is_some_and(|s| SKIP_SUBTYPES.contains(&s))
}

fn after(last: Option<&Ts>, ts: &Ts) -> bool {
    last.is_none_or(|l| ts > l)
}

fn my_last<'a>(reads: &Reads, msgs: impl Iterator<Item = &'a Msg>) -> Option<&'a Ts> {
    msgs.filter(|m| m.user == reads.me).map(|m| &m.ts).max()
}

fn is_ok<T>(read: Option<&Read<T>>) -> bool {
    matches!(read, Some(Ok(_)))
}

/// Message ids that must key their own thread row, whichever source reports them.
struct Hints {
    /// Roots known to have replies: a search hit always says `reply_count == 0`.
    threaded: BTreeSet<String>,
    /// Watched messages; the previous rows' watches when the watch read failed.
    watched: BTreeSet<String>,
}

fn hints(reads: &Reads, prev: &[Row]) -> Hints {
    let read_roots = reads.threads.keys().map(|id| msg_id(&id.channel, &id.root));
    let history_roots = reads
        .dms
        .values()
        .flatten()
        .flatten()
        .filter(|m| m.reply_count > 0)
        .map(|m| msg_id(&m.channel, &m.ts));
    let prev_roots = prev
        .iter()
        .filter(|r| thread_id(r).is_some())
        .map(|r| r.key.0.clone());
    let watched = match &reads.watched {
        Ok(ws) => ws.iter().map(|m| msg_id(&m.channel, &m.ts)).collect(),
        Err(_) => prev
            .iter()
            .filter_map(|r| Some(msg_id(&r.channel, r.watched.as_ref()?)))
            .collect(),
    };
    Hints {
        threaded: read_roots.chain(history_roots).chain(prev_roots).collect(),
        watched,
    }
}

/// Rule 2: top-level DM messages without replies and not watched share the DM row.
fn key_for(reads: &Reads, hints: &Hints, m: &Msg) -> RowKey {
    let id = msg_id(&m.channel, &m.ts);
    let dm_row = reads.dm_channels.contains(&m.channel)
        && is_top_level(m)
        && m.reply_count == 0
        && !hints.threaded.contains(&id)
        && !hints.watched.contains(&id);
    if dm_row {
        RowKey::dm(&m.channel)
    } else {
        RowKey::thread(&m.channel, &m.root())
    }
}

/// Rule 6.
fn watch_events<'a>(reads: &'a Reads, out: &mut Vec<Event<'a>>) {
    for m in reads.watched.iter().flatten() {
        out.push(Event {
            key: RowKey::thread(&m.channel, &m.root()),
            channel: &m.channel,
            kind: Kind::Watch,
            msg: m,
        });
    }
}

/// Rule 4: Jay posted after the mention, seen in a successful read of where it lives.
fn answered(reads: &Reads, m: &Msg, key: &RowKey) -> bool {
    let mine_after = |msgs: &Vec<Msg>, top_only: bool| {
        msgs.iter()
            .any(|x| x.user == reads.me && x.ts > m.ts && (!top_only || is_top_level(x)))
    };
    if *key == RowKey::dm(&m.channel) {
        return matches!(reads.dms.get(&m.channel), Some(Ok(ms)) if mine_after(ms, true));
    }
    let id = ThreadId {
        channel: m.channel.clone(),
        root: m.root(),
    };
    matches!(reads.threads.get(&id), Some(Ok(ms)) if mine_after(ms, false))
}

fn mention_events<'a>(reads: &'a Reads, hints: &Hints, out: &mut Vec<Event<'a>>) {
    for m in reads.mentions.iter().flatten() {
        let key = key_for(reads, hints, m);
        if counts(reads, m) && !answered(reads, m, &key) {
            out.push(Event {
                key,
                channel: &m.channel,
                kind: Kind::Mention,
                msg: m,
            });
        }
    }
}

/// Rules 3 and 5: others' messages after Jay's last post in the thread. In a DM thread Jay
/// hasn't posted in, its replies still count as `dm` (the root comes from the DM history).
fn thread_events<'a>(reads: &'a Reads, out: &mut Vec<Event<'a>>) {
    for (id, read) in &reads.threads {
        let Ok(msgs) = read else { continue };
        let last = my_last(reads, msgs.iter());
        let kind = match last {
            Some(_) if msgs.iter().any(|m| m.ts == id.root && m.user == reads.me) => Kind::Reply,
            Some(_) => Kind::Thread,
            None if reads.dm_channels.contains(&id.channel) => Kind::Dm,
            None => continue,
        };
        let key = RowKey::thread(&id.channel, &id.root);
        for m in msgs {
            if counts(reads, m) && after(last, &m.ts) && (last.is_some() || m.ts != id.root) {
                out.push(Event {
                    key: key.clone(),
                    channel: &id.channel,
                    kind,
                    msg: m,
                });
            }
        }
    }
}

fn channel_events<'a>(reads: &'a Reads, out: &mut Vec<Event<'a>>) {
    for (ch, read) in &reads.channels {
        let Ok(msgs) = read else { continue };
        for m in msgs.iter().filter(|m| is_top_level(m) && counts(reads, m)) {
            out.push(Event {
                key: RowKey::thread(ch, &m.ts),
                channel: ch,
                kind: Kind::Channel,
                msg: m,
            });
        }
    }
}

/// Rule 5: others' top-level messages after Jay's last top-level message there. A root that
/// moved to its own thread row is answered by Jay's last message in that thread, once read.
fn dm_events<'a>(reads: &'a Reads, hints: &Hints, out: &mut Vec<Event<'a>>) {
    for (ch, read) in &reads.dms {
        let Ok(msgs) = read else { continue };
        let tops = || msgs.iter().filter(|m| is_top_level(m));
        let last = my_last(reads, tops());
        for m in tops().filter(|m| counts(reads, m)) {
            let key = key_for(reads, hints, m);
            let id = ThreadId {
                channel: ch.clone(),
                root: m.ts.clone(),
            };
            let cutoff = match reads.threads.get(&id) {
                Some(Ok(thread)) if key != RowKey::dm(ch) => my_last(reads, thread.iter()),
                _ => last,
            };
            if after(cutoff, &m.ts) {
                out.push(Event {
                    key,
                    channel: ch,
                    kind: Kind::Dm,
                    msg: m,
                });
            }
        }
    }
}

/// Rule 7 (controller ruling): the DM row's watermark carries only to a root that moved from
/// the DM row to its own thread row, never to the thread's replies.
fn dm_watermark<'c>(reads: &Reads, clears: &'c Clears, key: &RowKey, ch: &str) -> Option<&'c Ts> {
    let dm = RowKey::dm(ch);
    if *key == dm || !reads.dm_channels.contains(ch) {
        return None;
    }
    clears.entries.get(&dm).map(|c| &c.wm)
}

/// Rules 7 and 9: one row per key from its events.
fn build(reads: &Reads, clears: &Clears, events: Vec<Event>) -> BTreeMap<RowKey, Row> {
    let mut by_key: BTreeMap<RowKey, Vec<Event>> = BTreeMap::new();
    for e in events {
        by_key.entry(e.key.clone()).or_default().push(e);
    }
    by_key
        .into_iter()
        .filter_map(|(key, evs)| {
            let row = build_row(reads, clears, &key, &evs)?;
            Some((key, row))
        })
        .collect()
}

fn build_row(reads: &Reads, clears: &Clears, key: &RowKey, evs: &[Event]) -> Option<Row> {
    let channel = evs[0].channel;
    let own = clears.entries.get(key).map(|c| &c.wm);
    let dm_wm = dm_watermark(reads, clears, key, channel);
    let fresh = |e: &&Event| {
        let moved_root = dm_wm.is_some() && *key == RowKey::thread(channel, &e.msg.ts);
        e.kind == Kind::Watch || (after(own, &e.msg.ts) && (!moved_root || after(dm_wm, &e.msg.ts)))
    };
    let live: Vec<&Event> = evs.iter().filter(fresh).collect();
    let watch = live
        .iter()
        .filter(|e| e.kind == Kind::Watch)
        .map(|e| e.msg)
        .max_by(|a, b| a.ts.cmp(&b.ts));
    let unhandled: BTreeMap<&Ts, &Msg> = live
        .iter()
        .filter(|e| e.kind != Kind::Watch)
        .map(|e| (&e.msg.ts, e.msg))
        .collect();
    let mut kinds: BTreeSet<Kind> = live.iter().map(|e| e.kind).collect();
    if !unhandled.is_empty() && reads.dm_channels.contains(channel) {
        kinds.insert(Kind::Dm);
    }
    let (oldest, shown) = match (unhandled.first_key_value(), unhandled.last_key_value()) {
        (Some((o, _)), Some((_, s))) => ((*o).clone(), (*s).clone()),
        _ => watch.map(|w| (w.ts.clone(), w.clone()))?,
    };
    Some(Row {
        key: key.clone(),
        channel: channel.to_owned(),
        kinds: kinds.into_iter().collect(),
        newest: shown.ts.clone(),
        shown,
        oldest,
        pending: unhandled.len(),
        watched: watch.map(|w| w.ts.clone()),
    })
}

fn thread_id(row: &Row) -> Option<ThreadId> {
    let root = row.key.0.strip_prefix(&format!("{}:", row.channel))?;
    (row.key != RowKey::dm(&row.channel)).then(|| ThreadId {
        channel: row.channel.clone(),
        root: Ts(root.to_owned()),
    })
}

/// The read that would resolve `kind` on `row` failed or didn't run this refresh (§7).
fn unread(reads: &Reads, row: &Row, kind: Kind) -> bool {
    let thread_unread = || thread_id(row).is_none_or(|id| !is_ok(reads.threads.get(&id)));
    match kind {
        Kind::Watch => reads.watched.is_err(),
        Kind::Mention => reads.mentions.is_err(),
        Kind::Reply | Kind::Thread => thread_unread(),
        Kind::Channel => matches!(reads.channels.get(&row.channel), Some(Err(_))),
        Kind::Dm => {
            !is_ok(reads.dms.get(&row.channel)) || (thread_id(row).is_some() && thread_unread())
        }
    }
}

fn floor(win: &Windows, kind: Kind) -> &Ts {
    match kind {
        Kind::Channel => &win.channel_floor,
        _ => &win.mention_floor,
    }
}

/// Whether aging out can hide `kind`'s resolution: true unless a successful read that isn't
/// window-limited (conversations.replies) would have rebuilt or answered it.
fn window_limited(reads: &Reads, row: &Row, kind: Kind) -> bool {
    let thread = thread_id(row).and_then(|id| reads.threads.get(&id)?.as_ref().ok());
    match (kind, thread) {
        (Kind::Watch | Kind::Reply | Kind::Thread, _) => false,
        (Kind::Channel, _) | (_, None) => true,
        (Kind::Mention | Kind::Dm, Some(msgs)) => {
            my_last(reads, msgs.iter()).is_none_or(|l| *l < row.oldest)
        }
    }
}

/// Every non-watch kind's messages have left its lookback window, and one of them can only be
/// resolved by a window-limited read (§7 "Aging out isn't resolving").
fn aged_out(reads: &Reads, win: &Windows, row: &Row) -> bool {
    let kinds = || row.kinds.iter().filter(|k| **k != Kind::Watch);
    kinds().all(|k| row.oldest < *floor(win, *k)) && kinds().any(|k| window_limited(reads, row, *k))
}

/// A successful watch read that no longer holds the row's watch removes it.
fn drop_unwatched(reads: &Reads, row: &mut Row) {
    let Ok(ws) = &reads.watched else { return };
    let still = row
        .watched
        .as_ref()
        .is_some_and(|t| ws.iter().any(|w| w.channel == row.channel && w.ts == *t));
    if !still {
        row.kinds.retain(|k| *k != Kind::Watch);
        row.watched = None;
    }
}

/// Rule 8: keep a row this refresh didn't rebuild when nothing proved it resolved.
fn carry(reads: &Reads, win: &Windows, mut row: Row) -> Option<Row> {
    drop_unwatched(reads, &mut row);
    let has_watch = row.kinds.contains(&Kind::Watch);
    if row.kinds.is_empty() || (row.newest < win.retention_floor && !has_watch) {
        return None;
    }
    let keep = row.kinds.iter().any(|k| unread(reads, &row, *k)) || aged_out(reads, win, &row);
    keep.then_some(row)
}

/// A rebuilt row keeps what an earlier refresh saw from a source that wasn't read this time.
fn merge(reads: &Reads, row: &mut Row, mut prev: Row) {
    // A failed watch read can't rebuild the watch: keep the one the previous row had.
    if reads.watched.is_err() && prev.kinds.contains(&Kind::Watch) {
        if !row.kinds.contains(&Kind::Watch) {
            row.kinds.insert(0, Kind::Watch);
        }
        row.watched = row.watched.take().or(prev.watched.clone());
    }
    if !prev
        .kinds
        .iter()
        .any(|k| *k != Kind::Watch && unread(reads, &prev, *k))
    {
        return;
    }
    drop_unwatched(reads, &mut prev);
    let kinds: BTreeSet<Kind> = row.kinds.iter().chain(&prev.kinds).copied().collect();
    row.kinds = kinds.into_iter().collect();
    row.oldest = row.oldest.clone().min(prev.oldest);
    row.newest = row.newest.clone().max(prev.newest);
    row.pending = row.pending.max(prev.pending);
    if prev.shown.ts > row.shown.ts {
        row.shown = prev.shown;
    }
    row.watched = row.watched.take().or(prev.watched);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Clear;
    use chrono::Utc;
    use std::slice;

    const ME: &str = "U0ME";
    const DANA: &str = "U0DANA";
    const MARCUS: &str = "U0MARCUS";
    const C: &str = "C0CORE";
    const D: &str = "D0DANA";

    fn ts(n: u64) -> Ts {
        Ts(format!("{}.000000", 1_792_000_000 + n))
    }

    /// A top-level message with no replies.
    fn msg(ch: &str, t: u64, user: &str, body: &str) -> Msg {
        Msg {
            channel: ch.into(),
            ts: ts(t),
            thread_ts: None,
            reply_count: 0,
            user: user.into(),
            subtype: None,
            body: body.into(),
        }
    }

    /// A reply at `t` in the thread rooted at `root`.
    fn reply(ch: &str, t: u64, root: u64, user: &str) -> Msg {
        Msg {
            thread_ts: Some(ts(root)),
            ..msg(ch, t, user, &format!("reply {t}"))
        }
    }

    /// A thread root with `n` replies.
    fn root(ch: &str, t: u64, user: &str, n: u32) -> Msg {
        Msg {
            thread_ts: Some(ts(t)),
            reply_count: n,
            ..msg(ch, t, user, &format!("root {t}"))
        }
    }

    fn tid(ch: &str, root: u64) -> ThreadId {
        ThreadId {
            channel: ch.into(),
            root: ts(root),
        }
    }

    fn reads() -> Reads {
        Reads {
            me: ME.into(),
            ignore: BTreeSet::from(["B0NOISE".to_string()]),
            watched: Ok(vec![]),
            mentions: Ok(vec![]),
            threads: BTreeMap::new(),
            channels: BTreeMap::new(),
            dms: BTreeMap::new(),
            dm_channels: BTreeSet::from([D.to_string(), "G0MPDM1".to_string()]),
        }
    }

    fn win() -> Windows {
        Windows {
            mention_floor: ts(0),
            channel_floor: ts(0),
            retention_floor: ts(0),
        }
    }

    fn clears(entries: &[(RowKey, u64, bool)]) -> Clears {
        Clears {
            entries: entries
                .iter()
                .map(|(k, wm, keep_watch)| {
                    let c = Clear {
                        wm: ts(*wm),
                        keep_watch: *keep_watch,
                        at: Utc::now(),
                    };
                    (k.clone(), c)
                })
                .collect(),
        }
    }

    fn run(r: &Reads) -> Vec<Row> {
        collect(r, &[], &Clears::default(), &win())
    }

    fn row(key: RowKey, ch: &str, kinds: &[Kind], shown: Msg, oldest: u64, pending: usize) -> Row {
        Row {
            key,
            channel: ch.into(),
            kinds: kinds.to_vec(),
            oldest: ts(oldest),
            newest: shown.ts.clone(),
            shown,
            pending,
            watched: None,
        }
    }

    fn keys(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.key.0.clone()).collect()
    }

    #[test]
    fn one_row_per_thread_with_all_kinds() {
        let mut r = reads();
        let top = root(C, 1, ME, 1);
        let ask = Msg {
            body: "<@U0ME> can you look?".into(),
            ..reply(C, 2, 1, DANA)
        };
        r.threads
            .insert(tid(C, 1), Ok(vec![top.clone(), ask.clone()]));
        r.mentions = Ok(vec![ask.clone()]);
        r.watched = Ok(vec![top]);
        let rows = run(&r);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.key, RowKey::thread(C, &ts(1)));
        assert_eq!(row.kinds, [Kind::Watch, Kind::Mention, Kind::Reply]);
        assert_eq!(row.shown, ask);
        assert_eq!(row.pending, 1);
        assert_eq!(row.watched, Some(ts(1)));
    }

    #[test]
    fn thread_vs_reply_by_root_author() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 2),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, MARCUS),
            ]),
        );
        r.threads.insert(
            tid(C, 10),
            Ok(vec![root(C, 10, ME, 1), reply(C, 11, 10, DANA)]),
        );
        let rows = run(&r);
        assert_eq!(
            keys(&rows),
            [RowKey::thread(C, &ts(1)).0, RowKey::thread(C, &ts(10)).0]
        );
        assert_eq!(rows[0].kinds, [Kind::Thread]);
        assert_eq!(rows[1].kinds, [Kind::Reply]);
    }

    #[test]
    fn replies_only_after_my_last_post() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 2),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, MARCUS),
            ]),
        );
        let rows = run(&r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pending, 1);
        assert_eq!(rows[0].shown.ts, ts(3));
        assert_eq!(rows[0].oldest, ts(3));
        assert_eq!(rows[0].newest, ts(3));
    }

    #[test]
    fn no_post_by_me_means_no_reply_or_thread() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![root(C, 1, DANA, 1), reply(C, 2, 1, MARCUS)]),
        );
        assert!(run(&r).is_empty());
    }

    #[test]
    fn mention_drops_after_i_answer() {
        let mut r = reads();
        let ask = reply(C, 2, 1, DANA);
        r.mentions = Ok(vec![ask.clone()]);
        // Thread not read this refresh: the mention stands.
        assert_eq!(run(&r)[0].kinds, [Kind::Mention]);
        r.threads.insert(
            tid(C, 1),
            Ok(vec![root(C, 1, MARCUS, 2), ask, reply(C, 3, 1, ME)]),
        );
        assert!(run(&r).is_empty());
    }

    #[test]
    fn my_own_and_ignored_messages_never_count() {
        let mut r = reads();
        r.channels.insert(
            C.into(),
            Ok(vec![
                msg(C, 1, ME, "mine"),
                msg(C, 2, "B0NOISE", "deploy ok"),
            ]),
        );
        r.mentions = Ok(vec![msg(C, 3, "B0NOISE", "<@U0ME> alert")]);
        r.dms.insert(D.into(), Ok(vec![msg(D, 4, ME, "hey Dana")]));
        assert!(run(&r).is_empty());
    }

    #[test]
    fn system_subtypes_skipped() {
        let mut r = reads();
        let join = Msg {
            subtype: Some("channel_join".into()),
            ..msg(C, 1, DANA, "joined")
        };
        let topic = Msg {
            subtype: Some("channel_topic".into()),
            ..msg(C, 2, MARCUS, "set the topic")
        };
        let bot = Msg {
            subtype: Some("bot_message".into()),
            ..msg(C, 3, "B0ALERT", "disk full")
        };
        r.channels.insert(C.into(), Ok(vec![join, topic, bot]));
        assert_eq!(keys(&run(&r)), [RowKey::thread(C, &ts(3)).0]);
    }

    #[test]
    fn dm_toplevel_groups_into_one_row() {
        let mut r = reads();
        r.dms.insert(
            D.into(),
            Ok(vec![
                msg(D, 1, DANA, "old"),
                msg(D, 2, ME, "answered"),
                msg(D, 3, DANA, "a"),
                msg(D, 4, DANA, "b"),
                msg(D, 5, DANA, "c"),
            ]),
        );
        let rows = run(&r);
        assert_eq!(keys(&rows), ["D0DANA:dm"]);
        let row = &rows[0];
        assert_eq!(row.channel, D);
        assert_eq!(row.kinds, [Kind::Dm]);
        assert_eq!(row.pending, 3);
        assert_eq!(row.oldest, ts(3));
        assert_eq!(row.shown.body, "c");
    }

    #[test]
    fn dm_message_with_replies_moves_to_thread_row() {
        let mut r = reads();
        r.dms.insert(
            D.into(),
            Ok(vec![
                msg(D, 1, DANA, "a"),
                root(D, 2, DANA, 2),
                msg(D, 3, DANA, "c"),
            ]),
        );
        let rows = run(&r);
        assert_eq!(keys(&rows), [RowKey::thread(D, &ts(2)).0, RowKey::dm(D).0]);
        assert!(rows[0].kinds.contains(&Kind::Dm));
        assert_eq!(rows[0].channel, D);
        assert_eq!(rows[1].pending, 2);
        assert_eq!(
            (rows[1].oldest.clone(), rows[1].newest.clone()),
            (ts(1), ts(3))
        );
    }

    #[test]
    fn dm_watermark_carries_to_thread_row() {
        let mut r = reads();
        let c = clears(&[(RowKey::dm(D), 5, false)]);
        r.dms.insert(
            D.into(),
            Ok(vec![msg(D, 4, DANA, "a"), root(D, 5, DANA, 2)]),
        );
        r.threads.insert(
            tid(D, 5),
            Ok(vec![
                root(D, 5, DANA, 2),
                reply(D, 6, 5, DANA),
                reply(D, 7, 5, DANA),
            ]),
        );
        let rows = collect(&r, &[], &c, &win());
        assert_eq!(keys(&rows), [RowKey::thread(D, &ts(5)).0]);
        assert_eq!(rows[0].kinds, [Kind::Dm]);
        assert_eq!(rows[0].pending, 2);
        assert_eq!(rows[0].oldest, ts(6));
        assert_eq!(rows[0].shown.ts, ts(7));
    }

    #[test]
    fn channel_posts_top_level_only() {
        let mut r = reads();
        r.channels.insert(
            C.into(),
            Ok(vec![
                msg(C, 1, DANA, "a"),
                reply(C, 2, 1, MARCUS),
                root(C, 3, MARCUS, 1),
            ]),
        );
        let rows = run(&r);
        assert_eq!(
            keys(&rows),
            [RowKey::thread(C, &ts(1)).0, RowKey::thread(C, &ts(3)).0]
        );
        assert!(
            rows.iter()
                .all(|r| r.kinds == [Kind::Channel] && r.pending == 1)
        );
        assert_eq!(rows[0].shown.ts, ts(1));
    }

    #[test]
    fn watermark_hides_old_shows_new() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 3),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, DANA),
                reply(C, 4, 1, DANA),
            ]),
        );
        let key = RowKey::thread(C, &ts(1));
        let rows = collect(&r, &[], &clears(&[(key.clone(), 3, false)]), &win());
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].pending, rows[0].oldest.clone()), (1, ts(4)));
        assert!(collect(&r, &[], &clears(&[(key, 4, false)]), &win()).is_empty());
    }

    #[test]
    fn failed_source_keeps_rows() {
        let mut r = reads();
        let prev = row(
            RowKey::thread(C, &ts(1)),
            C,
            &[Kind::Channel],
            msg(C, 1, DANA, "a"),
            1,
            1,
        );
        r.channels.insert(C.into(), Err("channel_not_found".into()));
        assert_eq!(
            collect(&r, slice::from_ref(&prev), &Clears::default(), &win()),
            [prev]
        );
    }

    fn prev_thread_row() -> Row {
        row(
            RowKey::thread(C, &ts(1)),
            C,
            &[Kind::Thread],
            reply(C, 3, 1, DANA),
            3,
            1,
        )
    }

    #[test]
    fn unread_thread_keeps_row() {
        let prev = prev_thread_row();
        let rows = collect(&reads(), slice::from_ref(&prev), &Clears::default(), &win());
        assert_eq!(rows, slice::from_ref(&prev));
        let mut r = reads();
        r.threads.insert(tid(C, 1), Err("ratelimited".into()));
        assert_eq!(
            collect(&r, slice::from_ref(&prev), &Clears::default(), &win()),
            [prev]
        );
    }

    #[test]
    fn successful_read_resolves_row() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 3),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, DANA),
                reply(C, 4, 1, ME),
            ]),
        );
        assert!(collect(&r, &[prev_thread_row()], &Clears::default(), &win()).is_empty());
    }

    #[test]
    fn aged_out_row_kept_until_retention() {
        let ask = reply(C, 2, 1, DANA);
        let prev = row(RowKey::thread(C, &ts(1)), C, &[Kind::Mention], ask, 2, 1);
        let r = reads(); // mentions Ok, without the old mention
        // Inside the window, a successful read without it resolves it.
        assert!(collect(&r, slice::from_ref(&prev), &Clears::default(), &win()).is_empty());
        let aged = Windows {
            mention_floor: ts(10),
            ..win()
        };
        assert_eq!(
            collect(&r, slice::from_ref(&prev), &Clears::default(), &aged),
            slice::from_ref(&prev)
        );
        let retired = Windows {
            retention_floor: ts(5),
            ..aged
        };
        assert!(collect(&r, &[prev], &Clears::default(), &retired).is_empty());
    }

    #[test]
    fn watch_never_ages_out() {
        let old = Windows {
            mention_floor: ts(10),
            channel_floor: ts(10),
            retention_floor: ts(10),
        };
        let eyes = msg(C, 1, DANA, "look at this later");
        let mut r = reads();
        r.watched = Ok(vec![eyes.clone()]);
        let rows = collect(&r, &[], &Clears::default(), &old);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kinds, [Kind::Watch]);
        assert_eq!(rows[0].shown, eyes);
        assert_eq!(rows[0].watched, Some(ts(1)));
        // Watch read failed: the carried watch row still isn't retired.
        r.watched = Err("ratelimited".into());
        assert_eq!(collect(&r, &rows, &Clears::default(), &old), rows);
    }

    #[test]
    fn carried_row_loses_watch_when_unwatched() {
        let mut prev = prev_thread_row();
        prev.kinds = vec![Kind::Watch, Kind::Thread];
        prev.watched = Some(ts(1));
        let rows = collect(&reads(), slice::from_ref(&prev), &Clears::default(), &win());
        assert_eq!(rows, [prev_thread_row()]);
    }

    #[test]
    fn partial_rebuild_keeps_unread_kinds() {
        let ask = reply(C, 3, 1, MARCUS);
        let prev = row(RowKey::thread(C, &ts(1)), C, &[Kind::Mention], ask, 3, 1);
        let mut r = reads();
        r.mentions = Err("ratelimited".into());
        let last = reply(C, 4, 1, DANA);
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 3),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, MARCUS),
                last.clone(),
            ]),
        );
        let rows = collect(&r, &[prev], &Clears::default(), &win());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kinds, [Kind::Mention, Kind::Thread]);
        assert_eq!(
            (rows[0].oldest.clone(), rows[0].newest.clone()),
            (ts(3), ts(4))
        );
        assert_eq!(rows[0].pending, 2);
        assert_eq!(rows[0].shown, last);
    }

    #[test]
    fn carried_row_respects_newer_clear() {
        let prev = prev_thread_row();
        let c = clears(&[(prev.key.clone(), 3, false)]);
        assert!(collect(&reads(), &[prev], &c, &win()).is_empty());
    }

    #[test]
    fn apply_clears_keep_watch() {
        let mut r = prev_thread_row();
        r.kinds = vec![Kind::Watch, Kind::Mention];
        r.watched = Some(ts(1));
        let rows = apply_clears(vec![r.clone()], &clears(&[(r.key.clone(), 3, true)]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kinds, [Kind::Watch]);
        assert!(apply_clears(vec![r.clone()], &clears(&[(r.key, 3, false)])).is_empty());
    }

    #[test]
    fn apply_clears_hides_until_newer() {
        let r = prev_thread_row(); // newest ts(3)
        let other = row(RowKey::dm(D), D, &[Kind::Dm], msg(D, 9, DANA, "hi"), 9, 1);
        let c = clears(&[(r.key.clone(), 3, false)]);
        assert_eq!(
            apply_clears(vec![r.clone(), other.clone()], &c),
            slice::from_ref(&other)
        );
        let c = clears(&[(r.key.clone(), 2, false)]);
        assert_eq!(apply_clears(vec![r.clone(), other.clone()], &c), [r, other]);
    }

    #[test]
    fn activity_is_newest_message_per_read_thread() {
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 2),
                reply(C, 5, 1, ME),
                reply(C, 3, 1, DANA),
            ]),
        );
        r.threads.insert(tid(C, 7), Err("ratelimited".into()));
        r.dms.insert(D.into(), Ok(vec![msg(D, 9, DANA, "hi")]));
        assert_eq!(activity(&r), BTreeMap::from([(tid(C, 1), ts(5))]));
    }

    #[test]
    fn dm_clear_keeps_unseen_thread_replies() {
        // Root t5 got Dana's reply t6; Jay then clears the DM row at t10.
        let mut r = reads();
        let c = clears(&[(RowKey::dm(D), 10, false)]);
        r.dms.insert(
            D.into(),
            Ok(vec![root(D, 5, DANA, 1), msg(D, 10, DANA, "later")]),
        );
        r.threads.insert(
            tid(D, 5),
            Ok(vec![root(D, 5, DANA, 1), reply(D, 6, 5, DANA)]),
        );
        let rows = collect(&r, &[], &c, &win());
        assert_eq!(keys(&rows), [RowKey::thread(D, &ts(5)).0]);
        assert_eq!(rows[0].pending, 1);
        assert_eq!(rows[0].shown.ts, ts(6));
    }

    #[test]
    fn dm_thread_row_resolves_when_i_reply_in_thread() {
        let mut r = reads();
        r.dms.insert(D.into(), Ok(vec![root(D, 5, DANA, 1)]));
        r.threads
            .insert(tid(D, 5), Ok(vec![root(D, 5, DANA, 1), reply(D, 6, 5, ME)]));
        assert!(run(&r).is_empty());
    }

    #[test]
    fn search_mention_on_dm_root_joins_its_thread_row() {
        // Search hits say reply_count 0 and carry no thread_ts for a root.
        let hit = msg(D, 5, DANA, "<@U0ME> thoughts?");
        let mut r = reads();
        r.mentions = Ok(vec![hit.clone()]);
        r.dms.insert(D.into(), Ok(vec![root(D, 5, DANA, 2)]));
        let rows = run(&r);
        assert_eq!(keys(&rows), [RowKey::thread(D, &ts(5)).0]);
        assert_eq!(rows[0].kinds, [Kind::Mention, Kind::Dm]);
        // Known threaded only from a thread read.
        let mut r = reads();
        r.mentions = Ok(vec![hit]);
        r.threads.insert(
            tid(D, 5),
            Ok(vec![root(D, 5, DANA, 1), reply(D, 6, 5, DANA)]),
        );
        assert_eq!(keys(&run(&r)), [RowKey::thread(D, &ts(5)).0]);
    }

    #[test]
    fn watch_kept_on_rebuilt_row_when_watch_read_fails() {
        let mut prev = prev_thread_row();
        prev.kinds = vec![Kind::Watch, Kind::Thread];
        prev.watched = Some(ts(1));
        let mut r = reads();
        r.watched = Err("ratelimited".into());
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 3),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, DANA),
                reply(C, 4, 1, DANA),
            ]),
        );
        let rows = collect(&r, &[prev], &Clears::default(), &win());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kinds, [Kind::Watch, Kind::Thread]);
        assert_eq!(rows[0].watched, Some(ts(1)));
    }

    #[test]
    fn watched_dm_message_stays_one_row_when_watch_read_fails() {
        let eyes = msg(D, 3, DANA, "remind me");
        let mut prev = row(
            RowKey::thread(D, &ts(3)),
            D,
            &[Kind::Watch],
            eyes.clone(),
            3,
            0,
        );
        prev.watched = Some(ts(3));
        let mut r = reads();
        r.watched = Err("ratelimited".into());
        r.dms.insert(D.into(), Ok(vec![eyes]));
        let rows = collect(&r, &[prev], &Clears::default(), &win());
        assert_eq!(keys(&rows), [RowKey::thread(D, &ts(3)).0]);
        assert_eq!(rows[0].kinds, [Kind::Watch, Kind::Dm]);
        assert_eq!(rows[0].watched, Some(ts(3)));
    }

    #[test]
    fn aged_thread_row_resolved_by_successful_read() {
        let aged = Windows {
            mention_floor: ts(10),
            channel_floor: ts(10),
            ..win()
        };
        let mut r = reads();
        r.threads.insert(
            tid(C, 1),
            Ok(vec![
                root(C, 1, DANA, 3),
                reply(C, 2, 1, ME),
                reply(C, 3, 1, DANA),
                reply(C, 4, 1, ME),
            ]),
        );
        assert!(collect(&r, &[prev_thread_row()], &Clears::default(), &aged).is_empty());
    }
}
