//! Background refresh (spec §3 "How it runs", §6 "Data fetching"): one worker thread per
//! instance, sharing one `cache.json` between all running copies through the fetch lock.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Utc};

use crate::config::Config;
use crate::refresh;
use crate::slack::{Slack, SlackError};
use crate::state::{CACHE, CLEARS, Cache, Clears, STATE, State};
use crate::store::StateDir;

#[derive(Debug, Clone, PartialEq)]
pub enum FetchRequest {
    Scheduled,
    Forced,
}

// Sent once per refresh, so the size gap with `Started` costs nothing worth a Box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum FetchEvent {
    Started,
    Done(Cache),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    UseCache,
    Fetch,
    WaitForOther,
}

/// A cache younger than `fresh_secs` is used as is by a scheduled request.
pub fn decide(
    req: &FetchRequest,
    cache_age: Option<Duration>,
    have_lock: bool,
    fresh_secs: i64,
) -> Plan {
    match req {
        FetchRequest::Forced => Plan::Fetch,
        FetchRequest::Scheduled => match cache_age {
            Some(age) if age < Duration::seconds(fresh_secs) => Plan::UseCache,
            _ if have_lock => Plan::Fetch,
            _ => Plan::WaitForOther,
        },
    }
}

/// Decides when the main loop sends requests: `Scheduled` at startup, then `every` after the
/// answer to the last outstanding request.
pub struct Poller {
    every: StdDuration,
    /// `None` while a scheduled answer is awaited: the next one is timed from its `Done`.
    next_scheduled: Option<Instant>,
    /// Requests sent and not yet answered by a `Done`.
    in_flight: usize,
}

impl Poller {
    pub fn new(now: Instant, every: StdDuration) -> Poller {
        Poller {
            every,
            next_scheduled: Some(now),
            in_flight: 0,
        }
    }

    /// Requests due at `now`; each returned request must be sent.
    pub fn due(&mut self, now: Instant) -> Vec<FetchRequest> {
        if self.next_scheduled.is_some_and(|at| now >= at) {
            self.next_scheduled = None;
            self.in_flight += 1;
            return vec![FetchRequest::Scheduled];
        }
        Vec::new()
    }

    /// The request for `r`.
    pub fn forced(&mut self) -> FetchRequest {
        self.in_flight += 1;
        FetchRequest::Forced
    }

    /// Called for every `Done`.
    pub fn on_done(&mut self, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if self.in_flight == 0 {
            self.next_scheduled = Some(now + self.every);
        }
    }
}

/// Starts the worker. It exits when the request sender is dropped or the event
/// receiver is gone.
pub fn spawn(
    slack: Box<dyn Slack>,
    cfg: Config,
    state: StateDir,
) -> (Sender<FetchRequest>, Receiver<FetchEvent>) {
    let (req_tx, req_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    thread::spawn(move || {
        let worker = Worker {
            slack,
            cfg,
            state,
            events: ev_tx,
        };
        for req in req_rx {
            if worker.handle(&req).is_err() {
                return;
            }
        }
    });
    (req_tx, ev_rx)
}

/// The UI stopped listening.
struct Gone;

struct Worker {
    slack: Box<dyn Slack>,
    cfg: Config,
    state: StateDir,
    events: Sender<FetchEvent>,
}

impl Worker {
    fn send(&self, ev: FetchEvent) -> Result<(), Gone> {
        self.events.send(ev).map_err(|_| Gone)
    }

    /// The cache on disk; an unreadable one counts as empty.
    fn load(&self) -> Cache {
        self.state.load(CACHE).unwrap_or_default()
    }

    fn handle(&self, req: &FetchRequest) -> Result<(), Gone> {
        // The lock is held until this function returns; a forced request waits for it.
        // A lock that cannot be tried (I/O error) must not stop refreshing: treat it as ours.
        let (mut lock, have_lock) = match self.state.try_fetch_lock() {
            Ok(Some(file)) => (Some(file), true),
            Ok(None) => (None, false),
            Err(_) => (None, true),
        };
        let cache = self.load();
        // The last attempt, failed or not: after an error each copy still waits refresh_secs.
        let age = cache.fetched_at.max(cache.error_at).map(|t| Utc::now() - t);
        let fresh = i64::try_from(self.cfg.refresh_secs).unwrap_or(i64::MAX);
        match decide(req, age, have_lock, fresh) {
            Plan::UseCache => self.send(FetchEvent::Done(cache)),
            Plan::WaitForOther => {
                // The other copy releases the lock once its refresh is written; no Slack calls.
                drop(self.state.fetch_lock());
                self.send(FetchEvent::Done(self.load()))
            }
            Plan::Fetch => {
                self.send(FetchEvent::Started)?;
                if lock.is_none() {
                    // If locking fails, refresh without the lock rather than not at all.
                    lock = self.state.fetch_lock().ok();
                }
                let cache = self.refresh();
                drop(lock);
                self.send(FetchEvent::Done(cache))
            }
        }
    }

    /// Runs one refresh against the files on disk and writes its outcome (under the fetch
    /// lock). Returns the cache as written; a cache write failure is reported in it.
    fn refresh(&self) -> Cache {
        let mut state: State = self.state.load(STATE).unwrap_or_default();
        let seen: Clears = self.state.load(CLEARS).unwrap_or_default();
        let mut clears = seen.clone();
        let prev = self.load();
        let result = refresh::run(
            self.slack.as_ref(),
            &self.cfg,
            &mut state,
            &mut clears,
            &prev,
            Utc::now(),
        );
        let now = Utc::now();
        let apply = |c: &mut Cache| match &result {
            Ok(new) => *c = new.clone(),
            // Keep the previous rows; the error is shown over them.
            Err(e) => {
                c.error = Some(e.clone());
                c.error_at = Some(now);
            }
        };
        // cache.json first: state.json only advances once its rows are published.
        let mut cache = match self.state.update(CACHE, |c: &mut Cache| {
            apply(c);
            true
        }) {
            Ok(cache) => cache,
            Err(io) => {
                let mut c = self.load();
                apply(&mut c);
                c.error = Some(SlackError::Api(format!("could not write {CACHE}: {io}")));
                c.error_at = Some(now);
                return c;
            }
        };
        if result.is_err() {
            return cache;
        }
        // The worker is state.json's only writer. clears.json is the UI's: apply only this
        // refresh's changes to the latest copy.
        let mut failed = Vec::new();
        if let Err(io) = self.state.update(STATE, |s: &mut State| {
            *s = state;
            true
        }) {
            failed.push(format!("could not write {STATE}: {io}"));
        }
        if let Err(io) = self.state.update(CLEARS, |latest: &mut Clears| {
            apply_clear_deltas(&seen, &clears, latest)
        }) {
            failed.push(format!("could not write {CLEARS}: {io}"));
        }
        if !failed.is_empty() {
            cache.warnings.extend(failed.iter().cloned());
            // If this fails too, the returned cache still shows the warnings.
            let _ = self.state.update(CACHE, |c: &mut Cache| {
                c.warnings.extend(failed);
                true
            });
        }
        cache
    }
}

/// Applies the worker's changes to `clears.json` (`seen` -> `after`) onto `latest`, the copy
/// on disk now: keep_watch flips and pruned entries, each only where the UI has not touched
/// the entry meanwhile. Returns whether `latest` changed.
fn apply_clear_deltas(seen: &Clears, after: &Clears, latest: &mut Clears) -> bool {
    let mut changed = false;
    for (key, old) in &seen.entries {
        match (after.entries.get(key), latest.entries.get_mut(key)) {
            (None, Some(now)) if now == old => {
                latest.entries.remove(key);
                changed = true;
            }
            (Some(new), Some(now))
                if new.keep_watch && !old.keep_watch && now.wm == old.wm && now.at == old.at =>
            {
                changed |= !now.keep_watch;
                now.keep_watch = true;
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Msg, Row, RowKey, Ts};
    use crate::slack::fake::FakeSlack;
    use crate::state::Clear;

    const AUTH_OK: &str = r#"{"ok":true,"user_id":"U0ME","team_id":"T0HEARD"}"#;

    #[test]
    fn decide_table() {
        let s = |secs| Some(Duration::seconds(secs));
        use FetchRequest::*;
        assert_eq!(decide(&Forced, s(5), false, 180), Plan::Fetch);
        assert_eq!(decide(&Scheduled, s(30), true, 180), Plan::UseCache);
        assert_eq!(decide(&Scheduled, s(30), false, 180), Plan::UseCache);
        assert_eq!(decide(&Scheduled, s(179), true, 180), Plan::UseCache);
        assert_eq!(decide(&Scheduled, s(180), true, 180), Plan::Fetch);
        assert_eq!(decide(&Scheduled, None, true, 180), Plan::Fetch);
        assert_eq!(decide(&Scheduled, s(200), false, 180), Plan::WaitForOther);
        assert_eq!(decide(&Scheduled, None, false, 180), Plan::WaitForOther);
        assert_eq!(decide(&Scheduled, s(30), true, 20), Plan::Fetch);
    }

    #[test]
    fn poller_times_from_last_done() {
        use FetchRequest::*;
        let s = StdDuration::from_secs;
        let t0 = Instant::now();
        let mut p = Poller::new(t0, s(180));
        assert_eq!(p.due(t0), vec![Scheduled]);
        assert!(p.due(t0 + s(500)).is_empty(), "awaiting its Done");
        assert_eq!(p.forced(), Forced);
        // Two requests out: the first Done does not schedule yet.
        p.on_done(t0 + s(2));
        assert!(p.due(t0 + s(500)).is_empty());
        p.on_done(t0 + s(5));
        assert!(p.due(t0 + s(184)).is_empty());
        assert_eq!(p.due(t0 + s(185)), vec![Scheduled]);
        p.on_done(t0 + s(186));
        // A forced refresh pushes the schedule out.
        assert_eq!(p.forced(), Forced);
        p.on_done(t0 + s(300));
        assert!(p.due(t0 + s(479)).is_empty());
        assert_eq!(p.due(t0 + s(480)), vec![Scheduled]);
    }

    fn recv(rx: &Receiver<FetchEvent>) -> FetchEvent {
        rx.recv_timeout(StdDuration::from_secs(10))
            .expect("worker event")
    }

    /// Events up to and including the next `Done`.
    fn until_done(rx: &Receiver<FetchEvent>) -> Vec<FetchEvent> {
        let mut out = Vec::new();
        loop {
            let ev = recv(rx);
            let done = matches!(ev, FetchEvent::Done(_));
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    fn done(evs: &[FetchEvent]) -> &Cache {
        let Some(FetchEvent::Done(cache)) = evs.last() else {
            unreachable!("{evs:?}")
        };
        cache
    }

    fn row(key: &str) -> Row {
        let shown = Msg {
            channel: "C0CORE".into(),
            ts: Ts("1792096026.762699".into()),
            thread_ts: None,
            reply_count: 0,
            user: "U0DANA".into(),
            subtype: None,
            body: "can you look at the deploy?".into(),
        };
        Row {
            key: RowKey(key.into()),
            channel: "C0CORE".into(),
            kinds: vec![Kind::Mention],
            oldest: shown.ts.clone(),
            newest: shown.ts.clone(),
            shown,
            pending: 1,
            watched: None,
        }
    }

    fn clear(wm: &str) -> Clear {
        Clear {
            wm: Ts(wm.into()),
            keep_watch: false,
            at: Utc::now() - Duration::hours(1),
        }
    }

    #[test]
    fn worker_keeps_rows_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let good: Cache = state
            .update(CACHE, |c: &mut Cache| {
                c.rows = vec![row("C0CORE:1792096026.762699")];
                true
            })
            .unwrap();
        let before_state: State = state
            .update(STATE, |s: &mut State| {
                s.refresh_no = 7;
                true
            })
            .unwrap();
        let fake = FakeSlack::new().on(
            "auth.test",
            &[],
            Err(SlackError::Auth("invalid_auth".into())),
        );
        let (tx, rx) = spawn(Box::new(fake), Config::default(), state);
        tx.send(FetchRequest::Forced).unwrap();
        let evs = until_done(&rx);
        assert_eq!(evs[0], FetchEvent::Started);
        let cache = done(&evs);
        assert_eq!(cache.rows, good.rows);
        assert_eq!(cache.error, Some(SlackError::Auth("invalid_auth".into())));
        assert!(cache.error_at.is_some());
        let disk = StateDir::new(tmp.path().to_path_buf());
        assert_eq!(&disk.load::<Cache>(CACHE).unwrap(), cache);
        assert_eq!(disk.load::<State>(STATE).unwrap(), before_state);
        assert!(!tmp.path().join(CLEARS).exists());
    }

    #[test]
    fn worker_saves_successful_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeSlack::new().on("auth.test", &[], Ok(AUTH_OK));
        let (tx, rx) = spawn(
            Box::new(fake),
            Config::default(),
            StateDir::new(tmp.path().to_path_buf()),
        );
        tx.send(FetchRequest::Scheduled).unwrap();
        let evs = until_done(&rx);
        assert_eq!(evs[0], FetchEvent::Started);
        let cache = done(&evs).clone();
        assert_eq!(cache.error, None);
        assert_eq!(cache.me.as_ref().unwrap().user_id, "U0ME");
        let disk = StateDir::new(tmp.path().to_path_buf());
        assert_eq!(disk.load::<Cache>(CACHE).unwrap(), cache);
        assert_eq!(disk.load::<State>(STATE).unwrap().refresh_no, 1);
        // A second Scheduled request within refresh_secs is served from the cache.
        tx.send(FetchRequest::Scheduled).unwrap();
        assert_eq!(until_done(&rx), vec![FetchEvent::Done(cache)]);
    }

    #[test]
    fn second_worker_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let other = StateDir::new(path.clone());
        let lock = other.try_fetch_lock().unwrap().expect("free lock");
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            held_rx.recv().unwrap();
            thread::sleep(StdDuration::from_millis(200));
            other
                .update(CACHE, |c: &mut Cache| {
                    c.warnings = vec!["other-copy".into()];
                    c.fetched_at = Some(Utc::now());
                    true
                })
                .unwrap();
            drop(lock);
        });
        // No answers: any Slack call would put an error into the cache.
        let (tx, rx) = spawn(
            Box::new(FakeSlack::new()),
            Config::default(),
            StateDir::new(path),
        );
        let start = Instant::now();
        tx.send(FetchRequest::Scheduled).unwrap();
        held_tx.send(()).unwrap();
        let evs = until_done(&rx);
        holder.join().unwrap();
        // Answered when the other copy lets go (~200 ms), not after a fixed sleep.
        assert!(start.elapsed() < StdDuration::from_millis(1500));
        assert_eq!(evs.len(), 1, "no Started: {evs:?}");
        let cache = done(&evs);
        assert_eq!(cache.warnings, ["other-copy"]);
        assert_eq!(cache.error, None);
    }

    /// A failed refresh counts as an attempt: right after it, a scheduled request uses the
    /// cache instead of calling Slack again.
    #[test]
    fn scheduled_after_error_uses_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let before: Cache = state
            .update(CACHE, |c: &mut Cache| {
                c.fetched_at = Some(Utc::now() - Duration::seconds(600));
                c.error = Some(SlackError::Http("boom".into()));
                c.error_at = Some(Utc::now());
                true
            })
            .unwrap();
        // No answers: a Slack call would replace the error with "no fake answer".
        let (tx, rx) = spawn(Box::new(FakeSlack::new()), Config::default(), state);
        tx.send(FetchRequest::Scheduled).unwrap();
        assert_eq!(until_done(&rx), vec![FetchEvent::Done(before)]);
    }

    /// Writes a clear to `clears.json` the moment the refresh starts, as the UI would.
    struct UiClears(FakeSlack, StateDir);
    impl Slack for UiClears {
        fn call(
            &self,
            method: &str,
            params: &[(&str, String)],
        ) -> Result<serde_json::Value, SlackError> {
            if method == "auth.test" {
                self.1
                    .update(CLEARS, |c: &mut Clears| {
                        c.entries
                            .insert(RowKey("D0DANA".into()), clear("1792096100.000100"));
                        true
                    })
                    .unwrap();
            }
            self.0.call(method, params)
        }
    }

    #[test]
    fn worker_preserves_ui_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        // Older than retention and unwatched: the refresh prunes it.
        let stale = RowKey("C0CORE:1000000000.000100".into());
        state
            .update(CLEARS, |c: &mut Clears| {
                c.entries.insert(stale.clone(), clear("1000000000.000100"));
                true
            })
            .unwrap();
        let slack = UiClears(
            FakeSlack::new().on("auth.test", &[], Ok(AUTH_OK)),
            StateDir::new(tmp.path().to_path_buf()),
        );
        let (tx, rx) = spawn(Box::new(slack), Config::default(), state);
        tx.send(FetchRequest::Forced).unwrap();
        assert_eq!(done(&until_done(&rx)).error, None);
        let clears: Clears = StateDir::new(tmp.path().to_path_buf())
            .load(CLEARS)
            .unwrap();
        assert!(clears.entries.contains_key(&RowKey("D0DANA".into())));
        assert!(!clears.entries.contains_key(&stale));
    }

    #[test]
    fn clear_deltas_apply_only_to_unchanged_entries() {
        let key = |k: &str| RowKey(k.into());
        let mut seen = Clears::default();
        for k in ["flip", "flip-moved", "pruned", "pruned-moved", "kept"] {
            seen.entries.insert(key(k), clear("1792096000.000100"));
        }
        let mut after = seen.clone();
        for k in ["flip", "flip-moved"] {
            after.entries.get_mut(&key(k)).unwrap().keep_watch = true;
        }
        for k in ["pruned", "pruned-moved"] {
            after.entries.remove(&key(k));
        }
        let mut latest = seen.clone();
        // The UI re-cleared these two meanwhile and added a new one.
        let moved = clear("1792096500.000100");
        latest.entries.insert(key("flip-moved"), moved.clone());
        latest.entries.insert(key("pruned-moved"), moved.clone());
        latest.entries.insert(key("new"), moved.clone());
        assert!(apply_clear_deltas(&seen, &after, &mut latest));
        assert!(latest.entries[&key("flip")].keep_watch);
        assert_eq!(latest.entries[&key("flip-moved")], moved);
        assert!(!latest.entries.contains_key(&key("pruned")));
        assert_eq!(latest.entries[&key("pruned-moved")], moved);
        assert_eq!(latest.entries[&key("new")], moved);
        assert_eq!(latest.entries[&key("kept")], seen.entries[&key("kept")]);
        assert!(!apply_clear_deltas(&seen, &seen, &mut latest.clone()));
    }

    #[test]
    fn cache_write_failure_leaves_state_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let before: State = state
            .update(STATE, |s: &mut State| {
                s.refresh_no = 7;
                true
            })
            .unwrap();
        std::fs::create_dir(tmp.path().join(CACHE)).unwrap();
        let fake = FakeSlack::new().on("auth.test", &[], Ok(AUTH_OK));
        let (tx, rx) = spawn(Box::new(fake), Config::default(), state);
        tx.send(FetchRequest::Forced).unwrap();
        let cache = done(&until_done(&rx)).clone();
        let Some(SlackError::Api(msg)) = &cache.error else {
            panic!("{:?}", cache.error)
        };
        assert!(msg.starts_with("could not write cache.json: "), "{msg}");
        let disk = StateDir::new(tmp.path().to_path_buf());
        assert_eq!(disk.load::<State>(STATE).unwrap(), before);
        assert!(!tmp.path().join(CLEARS).exists());
    }

    #[test]
    fn state_write_failure_warns_in_cache() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(STATE)).unwrap();
        let fake = FakeSlack::new().on("auth.test", &[], Ok(AUTH_OK));
        let (tx, rx) = spawn(
            Box::new(fake),
            Config::default(),
            StateDir::new(tmp.path().to_path_buf()),
        );
        tx.send(FetchRequest::Forced).unwrap();
        let cache = done(&until_done(&rx)).clone();
        assert_eq!(cache.error, None);
        let warned = |c: &Cache| {
            c.warnings
                .iter()
                .any(|w| w.starts_with("could not write state.json: "))
        };
        assert!(warned(&cache), "{:?}", cache.warnings);
        let disk: Cache = StateDir::new(tmp.path().to_path_buf()).load(CACHE).unwrap();
        assert_eq!(disk, cache);
    }

    #[test]
    fn worker_exits_when_receiver_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeSlack::new().on("auth.test", &[], Ok(AUTH_OK));
        let (tx, rx) = spawn(
            Box::new(fake),
            Config::default(),
            StateDir::new(tmp.path().to_path_buf()),
        );
        drop(rx);
        // The worker notices on its first send and stops; later sends then fail.
        let _ = tx.send(FetchRequest::Forced);
        let gone = (0..100).any(|_| {
            thread::sleep(StdDuration::from_millis(50));
            tx.send(FetchRequest::Scheduled).is_err()
        });
        assert!(gone);
    }
}
