//! Watch bookkeeping: baseline, watched set, re-checks (spec §5 "Only `:eyes:` added from now on
//! count", §6 source 1 "Watches").

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Msg, Ts, msg_id};
use crate::state::WatchState;

/// Applies one reactions read. `read` = currently watched messages (Jay's reaction, any watch emoji);
/// `complete` = the read held every reaction; `present` = reactions.get answers for ids asked via `to_recheck`.
pub fn apply(
    state: &mut WatchState,
    read: &[Msg],
    complete: bool,
    present: &BTreeMap<String, bool>,
) {
    let read: BTreeMap<String, &Msg> = read
        .iter()
        .map(|m| (msg_id(&m.channel, &m.ts), m))
        .collect();
    let Some(baseline) = &mut state.baseline else {
        state.baseline = Some(read.into_keys().collect());
        return;
    };
    baseline.retain(|id| read.contains_key(id));
    let mut watched: BTreeMap<String, Msg> = read
        .iter()
        .filter(|(id, _)| !baseline.contains(*id))
        .map(|(id, m)| (id.clone(), (*m).clone()))
        .collect();
    if !complete {
        // Missing from a partial read: unwatched only when reactions.get says so.
        for (id, m) in &state.watched {
            if !read.contains_key(id) && present.get(id) != Some(&false) {
                watched.insert(id.clone(), m.clone());
            }
        }
    }
    state.watched = watched;
}

/// Previously watched ids missing from an incomplete read: re-check them with reactions.get.
pub fn to_recheck(state: &WatchState, read: &[Msg], complete: bool) -> Vec<(String, Ts)> {
    if complete {
        return Vec::new();
    }
    let read: BTreeSet<String> = read.iter().map(|m| msg_id(&m.channel, &m.ts)).collect();
    state
        .watched
        .iter()
        .filter(|(id, _)| !read.contains(*id))
        .map(|(_, m)| (m.channel.clone(), m.ts.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(n: u64) -> Msg {
        Msg {
            channel: "C0CORE".into(),
            ts: Ts(format!("{}.000000", 1_792_000_000 + n)),
            thread_ts: None,
            reply_count: 0,
            user: "U0DANA".into(),
            subtype: None,
            body: format!("message {n}"),
        }
    }

    fn id(n: u64) -> String {
        let m = m(n);
        msg_id(&m.channel, &m.ts)
    }

    fn ids<'a>(it: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        it.into_iter().cloned().collect()
    }

    fn state(baseline: &[u64], watched: &[u64]) -> WatchState {
        WatchState {
            baseline: Some(baseline.iter().map(|&n| id(n)).collect()),
            watched: watched.iter().map(|&n| (id(n), m(n))).collect(),
        }
    }

    #[test]
    fn first_read_sets_baseline_and_watches_nothing() {
        let mut s = WatchState::default();
        apply(&mut s, &[m(1), m(2)], true, &BTreeMap::new());
        assert_eq!(s.baseline, Some(BTreeSet::from([id(1), id(2)])));
        assert!(s.watched.is_empty());
    }

    #[test]
    fn new_eyes_after_baseline_count() {
        let mut s = state(&[1], &[]);
        apply(&mut s, &[m(1), m(2)], true, &BTreeMap::new());
        assert_eq!(ids(s.watched.keys()), [id(2)]);
        assert_eq!(s.watched[&id(2)], m(2));
        assert_eq!(s.baseline, Some(BTreeSet::from([id(1)])));
    }

    #[test]
    fn baseline_entry_dropped_when_reaction_gone_then_readd_counts() {
        let mut s = state(&[1], &[]);
        apply(&mut s, &[], true, &BTreeMap::new());
        assert_eq!(s.baseline, Some(BTreeSet::new()));
        apply(&mut s, &[m(1)], true, &BTreeMap::new());
        assert_eq!(ids(s.watched.keys()), [id(1)]);
    }

    #[test]
    fn incomplete_read_rechecks_missing_watch() {
        let mut s = state(&[], &[1, 2, 3, 4]);
        let read = [m(4)];
        let ts = |n| m(n).ts;
        assert_eq!(
            to_recheck(&s, &read, false),
            [
                ("C0CORE".to_string(), ts(1)),
                ("C0CORE".to_string(), ts(2)),
                ("C0CORE".to_string(), ts(3)),
            ]
        );
        let present = BTreeMap::from([(id(1), false), (id(2), true)]);
        apply(&mut s, &read, false, &present);
        // 1 answered absent: unwatched. 2 present, 3 unknown: kept. 4 still in the read.
        assert_eq!(ids(s.watched.keys()), [id(2), id(3), id(4)]);
    }

    #[test]
    fn complete_read_drops_missing_watch_without_recheck() {
        let mut s = state(&[], &[1]);
        assert!(to_recheck(&s, &[], true).is_empty());
        apply(&mut s, &[], true, &BTreeMap::new());
        assert!(s.watched.is_empty());
    }
}
