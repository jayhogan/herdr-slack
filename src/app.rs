//! App state, key handling and the screen view model (spec §9 "The TUI", "Clearing").

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::ago;
use crate::collect::apply_clears;
use crate::config::Config;
use crate::model::{Kind, Row, RowKey, Ts};
use crate::refresh::describe;
use crate::slack::SlackError;
use crate::state::{Cache, Clear, Clears};
use crate::text;
use crate::token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    OldestWaiting,
    NewestActivity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    /// `clear` is `Some` for `O`; `unwatch` when that clear drops a watch (`open_clears_watch`).
    Open {
        row: Row,
        clear: Option<Clear>,
        unwatch: bool,
    },
    /// `x`; `unwatch` when the row has Watch (remove the reaction in Slack).
    Clear {
        row: Row,
        clear: Clear,
        unwatch: bool,
    },
    Undo(UndoEntry),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UndoEntry {
    pub key: RowKey,
    pub prev: Option<Clear>,
    /// (channel, ts) to re-add the watch reaction on.
    pub rewatch: Option<(String, Ts)>,
}

const STOPPED: &str = "refresh stopped — restart herdr-slack";

pub struct App {
    cache: Cache,
    clears: Clears,
    sel: usize,
    expanded: BTreeSet<RowKey>,
    sort: Sort,
    status: Option<String>,
    undo: Vec<UndoEntry>,
    cfg: Config,
    loading: bool,
    auth_error: Option<String>,
    /// The background refresh died; shown whenever no toast is.
    stopped: bool,
}

pub struct Screen {
    pub header: Header,
    pub rows: Vec<RowView>,
    pub selected: usize,
    pub status: Option<String>,
    /// The last refresh's non-auth failure (auth shows as `Body::AuthError`).
    pub error: Option<String>,
    /// How many sources failed or truncated in the last refresh.
    pub warnings: usize,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Loading,
    Empty,
    Rows,
    AuthError(String),
}

pub struct Header {
    pub count: usize,
    pub by_kind: Vec<(Kind, usize)>,
    pub sort: Sort,
    pub synced: Option<String>,
    pub warning: Option<String>,
}

pub struct RowView {
    pub text: String,
    /// The whole message, line breaks kept, for the expanded row.
    pub full: String,
    pub age: String,
    pub author: String,
    pub channel: String,
    pub kinds: Vec<Kind>,
    pub expanded: bool,
}

impl App {
    pub fn new(cfg: Config) -> App {
        App {
            cache: Cache::default(),
            clears: Clears::default(),
            sel: 0,
            expanded: BTreeSet::new(),
            sort: Sort::OldestWaiting,
            status: None,
            undo: Vec::new(),
            cfg,
            loading: false,
            auth_error: None,
            stopped: false,
        }
    }

    /// Run `f`, then keep the selection on the same row, or clamp it when the row is gone.
    fn keep_selection(&mut self, f: impl FnOnce(&mut App)) {
        let old = self.sel;
        let key = self.selected_row().map(|r| r.key);
        f(self);
        let rows = self.rows();
        self.sel = key
            .and_then(|k| rows.iter().position(|r| r.key == k))
            .unwrap_or(old.min(rows.len().saturating_sub(1)));
    }

    pub fn set_cache(&mut self, c: Cache) {
        self.keep_selection(|a| {
            a.cache = c;
            a.auth_error = None;
        });
    }

    pub fn set_clears(&mut self, c: Clears) {
        self.keep_selection(|a| a.clears = c);
    }

    pub fn set_loading(&mut self, on: bool) {
        self.loading = on;
    }

    pub fn set_status(&mut self, s: String) {
        self.status = Some(s);
    }

    /// The refresh worker is gone: say so until restart.
    pub fn set_stopped(&mut self) {
        self.stopped = true;
    }

    /// Main could not read the token: show the banner.
    pub fn set_auth_error(&mut self, msg: String) {
        self.auth_error = Some(msg);
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut rows = apply_clears(self.cache.rows.clone(), &self.clears);
        match self.sort {
            Sort::OldestWaiting => {
                rows.sort_by(|a, b| a.oldest.cmp(&b.oldest).then(a.key.cmp(&b.key)))
            }
            Sort::NewestActivity => {
                rows.sort_by(|a, b| b.newest.cmp(&a.newest).then(a.key.cmp(&b.key)))
            }
        }
        rows
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.sel)
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: DateTime<Utc>) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        self.status = None;
        let last = self.rows().len().saturating_sub(1);
        let code = match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Quit;
            }
            c => c,
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('g') => self.sel = 0,
            KeyCode::Char('G') => self.sel = last,
            KeyCode::Char(' ') => {
                if let Some(row) = self.selected_row()
                    && !self.expanded.remove(&row.key)
                {
                    self.expanded.insert(row.key);
                }
            }
            KeyCode::Char('s') => {
                self.sort = match self.sort {
                    Sort::OldestWaiting => Sort::NewestActivity,
                    Sort::NewestActivity => Sort::OldestWaiting,
                };
                self.sel = 0;
            }
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('o') => {
                if let Some(row) = self.selected_row() {
                    return Action::Open {
                        row,
                        clear: None,
                        unwatch: false,
                    };
                }
            }
            KeyCode::Char('O') => {
                if let Some(row) = self.selected_row() {
                    let has_watch = row.kinds.contains(&Kind::Watch);
                    let keep_watch = has_watch && !self.cfg.open_clears_watch;
                    let clear = Some(Clear {
                        wm: row.newest.clone(),
                        keep_watch,
                        at: now,
                    });
                    return Action::Open {
                        row,
                        clear,
                        unwatch: has_watch && !keep_watch,
                    };
                }
            }
            KeyCode::Char('x') => {
                if let Some(row) = self.selected_row() {
                    let clear = Clear {
                        wm: row.newest.clone(),
                        keep_watch: false,
                        at: now,
                    };
                    let unwatch = row.kinds.contains(&Kind::Watch);
                    return Action::Clear {
                        row,
                        clear,
                        unwatch,
                    };
                }
            }
            KeyCode::Char('u') => match self.undo.last() {
                Some(entry) => return Action::Undo(entry.clone()),
                None => self.status = Some("Nothing to undo".into()),
            },
            _ => {}
        }
        Action::None
    }

    /// After main carried out an action: record it locally (clears + undo) or report failure.
    pub fn applied(&mut self, action: &Action, result: Result<(), String>) {
        if let Err(e) = result {
            self.status = Some(e);
            return;
        }
        match action {
            Action::Clear {
                row,
                clear,
                unwatch,
            }
            | Action::Open {
                row,
                clear: Some(clear),
                unwatch,
            } => {
                let rewatch = unwatch
                    .then(|| row.watched.clone().map(|ts| (row.channel.clone(), ts)))
                    .flatten();
                let status = if *unwatch {
                    "Cleared · :eyes: removed · u to undo"
                } else if matches!(action, Action::Open { .. }) && clear.keep_watch {
                    "Opened · watch kept"
                } else {
                    "Cleared · u to undo"
                };
                self.record_clear(row, clear, rewatch, status);
            }
            Action::Undo(entry) => {
                self.keep_selection(|a| {
                    match &entry.prev {
                        Some(c) => a.clears.entries.insert(entry.key.clone(), c.clone()),
                        None => a.clears.entries.remove(&entry.key),
                    };
                    a.undo.pop();
                });
                self.status = Some(if entry.rewatch.is_some() {
                    "Restored · :eyes: re-added".into()
                } else {
                    "Restored".into()
                });
            }
            Action::Open { clear: None, .. } | Action::None | Action::Quit | Action::Refresh => {}
        }
    }

    fn record_clear(
        &mut self,
        row: &Row,
        clear: &Clear,
        rewatch: Option<(String, Ts)>,
        status: &str,
    ) {
        self.keep_selection(|a| {
            let prev = a.clears.entries.insert(row.key.clone(), clear.clone());
            a.undo.push(UndoEntry {
                key: row.key.clone(),
                prev,
                rewatch,
            });
        });
        self.status = Some(status.into());
    }

    pub fn screen(&self, now: DateTime<Utc>) -> Screen {
        let rows = self.rows();
        let names = &self.cache.names;
        let by_kind = [
            Kind::Watch,
            Kind::Mention,
            Kind::Reply,
            Kind::Thread,
            Kind::Channel,
            Kind::Dm,
        ]
        .into_iter()
        .map(|k| (k, rows.iter().filter(|r| r.kinds.contains(&k)).count()))
        .filter(|(_, n)| *n > 0)
        .collect();
        let views: Vec<RowView> = rows
            .iter()
            .map(|r| RowView {
                text: text::one_line(&text::display(&r.shown.body, names)),
                full: text::display(&r.shown.body, names),
                age: ago::long(r.newest.to_utc(), now),
                author: names
                    .users
                    .get(&r.shown.user)
                    .unwrap_or(&r.shown.user)
                    .clone(),
                channel: names.channels.get(&r.channel).unwrap_or(&r.channel).clone(),
                kinds: r.kinds.clone(),
                expanded: self.expanded.contains(&r.key),
            })
            .collect();
        let fetched = self.cache.fetched_at;
        let body = if let Some(msg) = &self.auth_error {
            Body::AuthError(msg.clone())
        } else if matches!(self.cache.error, Some(SlackError::Auth(_))) {
            Body::AuthError(token::MISSING_TOKEN.to_string())
        } else if views.is_empty() && fetched.is_none() {
            Body::Loading
        } else if views.is_empty() {
            Body::Empty
        } else {
            Body::Rows
        };
        let status = self
            .status
            .clone()
            .or_else(|| self.stopped.then(|| STOPPED.to_string()))
            .or_else(|| self.loading.then(|| "Refreshing…".to_string()));
        Screen {
            header: Header {
                count: views.len(),
                by_kind,
                sort: self.sort,
                synced: fetched.map(|t| ago::long(t, now)),
                warning: self.cache.warnings.first().cloned(),
            },
            rows: views,
            selected: self.sel,
            status,
            error: match &self.cache.error {
                Some(SlackError::Auth(_)) | None => None,
                Some(e) => Some(describe(e)),
            },
            warnings: self.cache.warnings.len(),
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::sample_cache;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_792_100_000, 0).unwrap()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn app_with(cfg: Config) -> App {
        let mut app = App::new(cfg);
        app.set_cache(sample_cache(now()));
        app
    }

    fn app() -> App {
        app_with(Config::default())
    }

    fn select(app: &mut App, kind: Kind, idx: usize) {
        app.sel = app
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kinds.contains(&kind))
            .nth(idx)
            .unwrap()
            .0;
    }

    fn do_key(app: &mut App, c: char) -> Action {
        let act = app.handle_key(key(c), now());
        app.applied(&act, Ok(()));
        act
    }

    #[test]
    fn screen_carries_full_text_and_non_auth_error() {
        let mut a = app();
        let mut c = sample_cache(now());
        c.error = Some(SlackError::Http("search.messages: timed out".into()));
        c.warnings = vec!["a".into(), "b".into()];
        a.set_cache(c);
        let s = a.screen(now());
        assert_eq!(s.error.as_deref(), Some("search.messages: timed out"));
        assert_eq!(s.warnings, 2);
        assert_eq!(s.body, Body::Rows);
        assert!(s.rows[1].full.starts_with("@Jay can you sanity check"));
        a.set_cache({
            let mut c = sample_cache(now());
            c.error = Some(SlackError::Auth("invalid_auth".into()));
            c
        });
        assert_eq!(a.screen(now()).error, None);
    }

    #[test]
    fn keys_move_and_clamp() {
        let mut a = app();
        let last = a.rows().len() - 1;
        a.handle_key(key('k'), now());
        assert_eq!(a.sel, 0);
        a.handle_key(key('j'), now());
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), now());
        assert_eq!(a.sel, 2);
        a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), now());
        assert_eq!(a.sel, 1);
        a.handle_key(key('G'), now());
        assert_eq!(a.sel, last);
        a.handle_key(key('j'), now());
        assert_eq!(a.sel, last);
        a.handle_key(key('g'), now());
        assert_eq!(a.sel, 0);
        assert_eq!(a.handle_key(key('q'), now()), Action::Quit);
        assert_eq!(
            a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), now()),
            Action::Quit
        );
        assert_eq!(a.handle_key(key('r'), now()), Action::Refresh);
    }

    #[test]
    fn space_expands_selected_only() {
        let mut a = app();
        a.handle_key(key('j'), now());
        a.handle_key(key(' '), now());
        let expanded: Vec<bool> = a.screen(now()).rows.iter().map(|r| r.expanded).collect();
        assert_eq!(expanded.iter().filter(|e| **e).count(), 1);
        assert!(expanded[1]);
        a.handle_key(key(' '), now());
        assert!(a.screen(now()).rows.iter().all(|r| !r.expanded));
    }

    #[test]
    fn sort_oldest_waiting_default() {
        let mut a = app();
        let rows = a.rows();
        assert_eq!(
            rows[0].oldest,
            rows.iter().map(|r| r.oldest.clone()).min().unwrap()
        );
        assert_eq!(rows[0].shown.user, "U0DANA");
        a.handle_key(key('j'), now());
        a.handle_key(key('s'), now());
        let rows = a.rows();
        assert_eq!(a.sel, 0);
        assert_eq!(a.screen(now()).header.sort, Sort::NewestActivity);
        assert_eq!(
            rows[0].newest,
            rows.iter().map(|r| r.newest.clone()).max().unwrap()
        );
    }

    #[test]
    fn x_makes_clear_action_with_unwatch_for_watch_rows() {
        let mut a = app();
        select(&mut a, Kind::Watch, 0);
        match a.handle_key(key('x'), now()) {
            Action::Clear {
                row,
                clear,
                unwatch,
            } => {
                assert!(unwatch);
                assert_eq!(clear.wm, row.newest);
                assert!(!clear.keep_watch);
            }
            other => panic!("{other:?}"),
        }
        select(&mut a, Kind::Dm, 0);
        match a.handle_key(key('x'), now()) {
            Action::Clear { unwatch, .. } => assert!(!unwatch),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn big_o_keeps_watch_by_default() {
        let mut a = app();
        select(&mut a, Kind::Watch, 0);
        match a.handle_key(key('O'), now()) {
            Action::Open { clear: Some(c), .. } => assert!(c.keep_watch),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            a.handle_key(key('o'), now()),
            Action::Open { clear: None, .. }
        ));
        let cfg = Config {
            open_clears_watch: true,
            ..Config::default()
        };
        let mut a = app_with(cfg);
        select(&mut a, Kind::Watch, 0);
        match a.handle_key(
            KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
            now(),
        ) {
            Action::Open { clear: Some(c), .. } => assert!(!c.keep_watch),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn applied_ok_hides_row_and_undo_restores() {
        let mut a = app();
        select(&mut a, Kind::Watch, 0);
        let before = a.rows().len();
        let x = a.handle_key(key('x'), now());
        a.applied(&x, Ok(()));
        assert_eq!(a.rows().len(), before - 1);
        assert_eq!(
            a.status.as_deref(),
            Some("Cleared · :eyes: removed · u to undo")
        );
        let Action::Undo(entry) = a.handle_key(key('u'), now()) else {
            panic!("expected undo")
        };
        assert!(entry.rewatch.is_some());
        a.applied(&Action::Undo(entry), Ok(()));
        assert_eq!(a.rows().len(), before);
        assert_eq!(a.status.as_deref(), Some("Restored · :eyes: re-added"));
        assert_eq!(a.handle_key(key('u'), now()), Action::None);
        assert_eq!(a.status.as_deref(), Some("Nothing to undo"));
    }

    #[test]
    fn big_o_applied_keeps_watch_row_visible() {
        let mut a = app();
        select(&mut a, Kind::Watch, 1); // b: watch, mention, thread
        let before = a.rows().len();
        let act = a.handle_key(key('O'), now());
        a.applied(&act, Ok(()));
        let rows = a.rows();
        assert_eq!(rows.len(), before);
        assert_eq!(rows[a.sel].kinds, vec![Kind::Watch]);
        assert_eq!(a.status.as_deref(), Some("Opened · watch kept"));
        assert_eq!(a.undo.last().unwrap().rewatch, None);
    }

    #[test]
    fn big_o_applied_hides_non_watch_row_and_undo_restores() {
        let mut a = app();
        select(&mut a, Kind::Dm, 0);
        let before = a.rows();
        let act = a.handle_key(key('O'), now());
        assert!(matches!(act, Action::Open { unwatch: false, .. }));
        a.applied(&act, Ok(()));
        assert_eq!(a.rows().len(), before.len() - 1);
        assert_eq!(a.status.as_deref(), Some("Cleared · u to undo"));
        do_key(&mut a, 'u');
        assert_eq!(a.rows(), before);
        assert_eq!(a.status.as_deref(), Some("Restored"));
    }

    #[test]
    fn big_o_with_open_clears_watch_unwatches() {
        let cfg = Config {
            open_clears_watch: true,
            ..Config::default()
        };
        let mut a = app_with(cfg);
        select(&mut a, Kind::Watch, 0);
        let before = a.rows().len();
        let act = a.handle_key(key('O'), now());
        let Action::Open { row, unwatch, .. } = &act else {
            panic!("{act:?}")
        };
        assert!(*unwatch);
        let expect = Some((row.channel.clone(), row.watched.clone().unwrap()));
        a.applied(&act, Ok(()));
        assert_eq!(a.rows().len(), before - 1);
        assert_eq!(a.undo.last().unwrap().rewatch, expect);
        assert_eq!(
            a.status.as_deref(),
            Some("Cleared · :eyes: removed · u to undo")
        );
        do_key(&mut a, 'u');
        assert_eq!(a.status.as_deref(), Some("Restored · :eyes: re-added"));
    }

    #[test]
    fn applied_err_changes_nothing() {
        let mut a = app();
        let before = a.rows();
        let x = a.handle_key(key('x'), now());
        a.applied(&x, Err("reactions.remove: missing_scope".into()));
        assert_eq!(a.rows(), before);
        assert!(a.undo.is_empty());
        assert_eq!(a.status.as_deref(), Some("reactions.remove: missing_scope"));
    }

    #[test]
    fn undo_is_multilevel() {
        let mut a = app();
        let before = a.rows();
        do_key(&mut a, 'x');
        do_key(&mut a, 'x');
        assert_eq!(a.rows().len(), before.len() - 2);
        do_key(&mut a, 'u');
        assert_eq!(a.rows().len(), before.len() - 1);
        do_key(&mut a, 'u');
        assert_eq!(a.rows(), before);
        assert!(a.undo.is_empty());
    }

    #[test]
    fn reload_clears_hides_row() {
        let mut a = app();
        let row = a.rows().remove(0);
        let mut clears = Clears::default();
        clears.entries.insert(
            row.key.clone(),
            Clear {
                wm: row.newest.clone(),
                keep_watch: false,
                at: now(),
            },
        );
        a.set_clears(clears);
        assert!(a.rows().iter().all(|r| r.key != row.key));
    }

    #[test]
    fn selection_follows_row_across_set_cache() {
        let mut a = app();
        a.sel = 3;
        let key3 = a.rows()[3].key.clone();
        let mut cache = sample_cache(now());
        let mut older = cache.rows[0].clone();
        older.key = RowKey("C0EPD:older".into());
        older.oldest = Ts::from_unix(1);
        cache.rows.push(older);
        a.set_cache(cache);
        assert_eq!(a.rows()[a.sel].key, key3);
        assert_eq!(a.screen(now()).selected, 4);
        assert_eq!(a.sel, 4);
    }

    #[test]
    fn header_counts_by_kind() {
        let s = app().screen(now());
        assert_eq!(s.header.count, 9);
        assert_eq!(
            s.header.by_kind,
            vec![
                (Kind::Watch, 2),
                (Kind::Mention, 3),
                (Kind::Reply, 2),
                (Kind::Thread, 2),
                (Kind::Channel, 2),
                (Kind::Dm, 1),
            ]
        );
        assert_eq!(s.body, Body::Rows);
        assert_eq!(s.header.synced.as_deref(), Some("2 min ago"));
        assert_eq!(s.rows[0].author, "Dana Okafor");
        assert_eq!(s.rows[0].channel, "#epd");
        assert_eq!(s.rows[0].age, "3 days ago");
        assert_eq!(s.rows[0].kinds, vec![Kind::Watch]);
        assert!(s.rows[0].text.starts_with("Review the Q4 audit-log"));
        assert!(!s.rows[0].text.contains('\n'));
        assert_eq!(s.header.warning, None);
    }

    #[test]
    fn status_loading_and_warning() {
        let mut a = app();
        a.set_loading(true);
        assert_eq!(a.screen(now()).status.as_deref(), Some("Refreshing…"));
        a.set_status("hello".into());
        assert_eq!(a.screen(now()).status.as_deref(), Some("hello"));
        let mut cache = sample_cache(now());
        cache.warnings = vec!["truncated #core-eng".into(), "second".into()];
        a.set_cache(cache);
        assert_eq!(
            a.screen(now()).header.warning.as_deref(),
            Some("truncated #core-eng")
        );
    }

    #[test]
    fn auth_error_body() {
        let mut a = App::new(Config::default());
        assert_eq!(a.screen(now()).body, Body::Loading);
        let cache = Cache {
            error: Some(SlackError::Auth("invalid_auth".into())),
            ..Cache::default()
        };
        a.set_cache(cache);
        assert_eq!(
            a.screen(now()).body,
            Body::AuthError(token::MISSING_TOKEN.to_string())
        );
        let mut b = App::new(Config::default());
        b.set_auth_error("no token".into());
        assert_eq!(b.screen(now()).body, Body::AuthError("no token".into()));
    }
}
