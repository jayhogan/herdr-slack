//! herdr-slack CLI and pane entry: args, `verify`, the TUI (spec §3 "How it runs", §7, §9).

mod ago;
mod app;
mod collect;
mod config;
mod fetch;
mod fixtures;
mod model;
mod open;
mod refresh;
mod slack;
mod sources;
mod state;
mod store;
mod text;
mod token;
mod ui;
mod watch;

use std::env;
use std::io;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use serde_json::Value;

use crate::app::{Action, App, UndoEntry};
use crate::config::{Config, OpenMode};
use crate::fetch::{FetchEvent, FetchRequest, Poller};
use crate::model::Row;
use crate::refresh::describe;
use crate::slack::{HttpSlack, Slack, SlackError};
use crate::state::{CLEARS, Clear, Clears, Me};
use crate::store::StateDir;

const USAGE: &str = "usage: herdr-slack [verify] [--fixtures]";
const TICK: Duration = Duration::from_secs(1);
const CLEARS_EVERY: Duration = Duration::from_secs(2);
const NO_TOKEN: &str = "Slack token missing";

#[derive(Debug, PartialEq)]
enum Cmd {
    Tui,
    Verify,
}

#[derive(Debug, PartialEq)]
struct Args {
    cmd: Cmd,
    fixtures: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut cmd = None;
    let mut fixtures = false;
    for arg in args {
        match arg.as_str() {
            "--fixtures" => fixtures = true,
            "verify" => {
                if cmd.replace(Cmd::Verify).is_some() {
                    return Err("command given more than once".into());
                }
            }
            other => return Err(format!("unrecognized argument '{other}'")),
        }
    }
    let cmd = cmd.unwrap_or(Cmd::Tui);
    if cmd == Cmd::Verify && fixtures {
        return Err("verify always talks to Slack; it cannot use --fixtures".into());
    }
    Ok(Args { cmd, fixtures })
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args = match parse_args(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("herdr-slack: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match args.cmd {
        Cmd::Tui => run_tui(args.fixtures),
        Cmd::Verify => Err("verify is not implemented yet".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdr-slack: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Where actions go: nowhere (fixtures), nowhere yet (no token), or Slack.
enum Mode {
    /// Sample data; actions only say what they would do. Holds the sample identity.
    Fixtures(Option<Me>),
    /// No token at startup: the auth banner shows, and `r` tries again.
    NoToken,
    Live(Box<Live>),
}

fn run_tui(fixtures_mode: bool) -> Result<(), String> {
    let (cfg, cfg_err) = match config::load(&config::default_path()) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let (mut app, mut mode) = if fixtures_mode {
        fixtures(cfg.clone(), Utc::now())
    } else {
        let mut app = App::new(cfg.clone());
        let mode = connect(&mut app, &cfg);
        (app, mode)
    };
    if let Some(e) = cfg_err {
        app.set_status(e);
    }
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut mode, &cfg);
    ratatui::restore();
    result.map_err(|e| e.to_string())
}

/// `--fixtures`: the sample inbox, clears kept in memory only.
fn fixtures(cfg: Config, now: DateTime<Utc>) -> (App, Mode) {
    let cache = fixtures::sample_cache(now);
    let me = cache.me.clone();
    let mut app = App::new(cfg);
    app.set_cache(cache);
    (app, Mode::Fixtures(me))
}

/// Starts the worker when there is a token; otherwise shows the auth banner.
fn connect(app: &mut App, cfg: &Config) -> Mode {
    match token::read() {
        Ok(token) => Mode::Live(Box::new(Live::start(token, cfg))),
        Err(e) => {
            app.set_auth_error(e);
            Mode::NoToken
        }
    }
}

/// The background refresh, the UI's own Slack client for actions, and the clears reload.
struct Live {
    requests: Sender<FetchRequest>,
    events: Receiver<FetchEvent>,
    poller: Poller,
    state: StateDir,
    slack_ui: HttpSlack,
    next_clears_reload: Instant,
    /// From the last cache that had it; needed for `slack://` URLs.
    me: Option<Me>,
}

impl Live {
    fn start(token: String, cfg: &Config) -> Live {
        let worker_slack = Box::new(HttpSlack::new(token.clone(), true));
        let (requests, events) =
            fetch::spawn(worker_slack, cfg.clone(), StateDir::default_location());
        let now = Instant::now();
        Live {
            requests,
            events,
            poller: Poller::new(now, Duration::from_secs(cfg.refresh_secs)),
            state: StateDir::default_location(),
            slack_ui: HttpSlack::new(token, false),
            next_clears_reload: now,
            me: None,
        }
    }

    /// Reloads clears.json every 2 s, sends due requests and applies every arrived event.
    fn tick(&mut self, app: &mut App) {
        let now = Instant::now();
        if now >= self.next_clears_reload {
            // An unreadable file keeps the clears already shown.
            if let Ok(clears) = self.state.load::<Clears>(CLEARS) {
                app.set_clears(clears);
            }
            self.next_clears_reload = now + CLEARS_EVERY;
        }
        for req in self.poller.due(now) {
            let _ = self.requests.send(req); // a dead worker leaves the last data on screen
        }
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                FetchEvent::Started => app.set_loading(true),
                FetchEvent::Done(cache) => {
                    if cache.me.is_some() {
                        self.me = cache.me.clone();
                    }
                    app.set_cache(cache);
                    app.set_loading(false);
                    self.poller.on_done(Instant::now());
                }
            }
        }
    }

    fn refresh(&mut self) {
        let _ = self.requests.send(self.poller.forced());
    }
}

/// Carries out a key's action; true when the app should quit.
fn dispatch(app: &mut App, action: Action, mode: &mut Mode, cfg: &Config) -> bool {
    if action == Action::Refresh && matches!(mode, Mode::NoToken) {
        *mode = connect(app, cfg);
        if matches!(mode, Mode::NoToken) {
            app.set_status(NO_TOKEN.into());
        }
        return false;
    }
    match (action, mode) {
        (Action::Quit, _) => return true,
        (Action::None, _) => {}
        (Action::Refresh, Mode::Live(live)) => live.refresh(),
        (Action::Refresh, _) => app.set_status("fixtures: would refresh".into()),
        (action, Mode::Live(live)) => run_action(
            app,
            &action,
            &live.slack_ui,
            &live.state,
            cfg,
            live.me.as_ref(),
            &open::launch,
        ),
        (action, Mode::Fixtures(me)) => fixture_action(app, &action, me.as_ref()),
        (_, Mode::NoToken) => app.set_status(NO_TOKEN.into()),
    }
    false
}

/// Open, clear or undo against Slack and clears.json, then tell the app how it went.
fn run_action(
    app: &mut App,
    action: &Action,
    slack: &dyn Slack,
    state: &StateDir,
    cfg: &Config,
    me: Option<&Me>,
    launch: &dyn Fn(&str) -> Result<(), String>,
) {
    let result = match action {
        Action::Open {
            row,
            clear,
            unwatch,
        } => url_for(row, slack, cfg, me)
            .and_then(|url| launch(&url))
            .and_then(|()| match clear {
                Some(clear) => write_clear(row, clear, *unwatch, slack, state, cfg),
                None => Ok(()),
            }),
        Action::Clear {
            row,
            clear,
            unwatch,
        } => write_clear(row, clear, *unwatch, slack, state, cfg),
        Action::Undo(entry) => undo(entry, slack, state, cfg),
        Action::None | Action::Quit | Action::Refresh => return,
    };
    app.applied(action, result);
}

/// `slack://` for `open_mode: app` once the identity is known; else the https permalink.
fn url_for(row: &Row, slack: &dyn Slack, cfg: &Config, me: Option<&Me>) -> Result<String, String> {
    match (cfg.open_mode, me) {
        (OpenMode::App, Some(me)) => Ok(open::app_url(me, row)),
        _ => open::permalink(slack, &row.channel, &row.shown.ts).map_err(|e| describe(&e)),
    }
}

/// Removes the watch reactions (when `unwatch`), then records the clear in clears.json.
/// A failed removal stops before anything is cleared.
fn write_clear(
    row: &Row,
    clear: &Clear,
    unwatch: bool,
    slack: &dyn Slack,
    state: &StateDir,
    cfg: &Config,
) -> Result<(), String> {
    if unwatch && let Some(ts) = &row.watched {
        for name in &cfg.watch_emoji {
            let params = [
                ("name", name.clone()),
                ("channel", row.channel.clone()),
                ("timestamp", ts.0.clone()),
            ];
            tolerate(slack.call("reactions.remove", &params), "no_reaction")?;
        }
    }
    update_clears(state, |c| {
        c.entries.insert(row.key.clone(), clear.clone());
    })
}

/// Re-adds the watch reaction (when the clear removed one), then restores the earlier clear.
fn undo(
    entry: &UndoEntry,
    slack: &dyn Slack,
    state: &StateDir,
    cfg: &Config,
) -> Result<(), String> {
    if let Some((channel, ts)) = &entry.rewatch {
        let name = cfg.watch_emoji.first().ok_or("no watch_emoji configured")?;
        let params = [
            ("name", name.clone()),
            ("channel", channel.clone()),
            ("timestamp", ts.0.clone()),
        ];
        tolerate(slack.call("reactions.add", &params), "already_reacted")?;
    }
    update_clears(state, |c| {
        match &entry.prev {
            Some(prev) => c.entries.insert(entry.key.clone(), prev.clone()),
            None => c.entries.remove(&entry.key),
        };
    })
}

/// A Slack result where the error `code` means the work is already done.
fn tolerate(result: Result<Value, SlackError>, code: &str) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(SlackError::Api(m)) if m.rsplit(": ").next() == Some(code) => Ok(()),
        Err(e) => Err(describe(&e)),
    }
}

fn update_clears(state: &StateDir, change: impl FnOnce(&mut Clears)) -> Result<(), String> {
    state
        .update(CLEARS, |c: &mut Clears| {
            change(c);
            true
        })
        .map(drop)
        .map_err(|e| format!("could not write {CLEARS}: {e}"))
}

/// Fixtures mode: record clears in memory and say what a live run would do.
fn fixture_action(app: &mut App, action: &Action, me: Option<&Me>) {
    let what = match action {
        Action::Open { row, clear, .. } => {
            let url = me.map_or_else(|| row.key.0.clone(), |me| open::app_url(me, row));
            let and = if clear.is_some() { " and clear it" } else { "" };
            format!("open {url}{and}")
        }
        Action::Clear { row, unwatch, .. } => {
            let and = if *unwatch {
                " and remove the watch reaction"
            } else {
                ""
            };
            format!("clear {}{and}", row.key.0)
        }
        Action::Undo(entry) => {
            let and = if entry.rewatch.is_some() {
                " and re-add the watch reaction"
            } else {
                ""
            };
            format!("undo the clear of {}{and}", entry.key.0)
        }
        Action::None | Action::Quit | Action::Refresh => return,
    };
    app.applied(action, Ok(()));
    app.set_status(format!("fixtures: would {what}"));
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mode: &mut Mode,
    cfg: &Config,
) -> io::Result<()> {
    let mut scroll = 0;
    loop {
        if let Mode::Live(live) = mode {
            live.tick(app);
        }
        let screen = app.screen(Utc::now());
        terminal.draw(|frame| ui::list::render(frame, &screen, &mut scroll))?;

        // A key or resize wakes the loop early; otherwise redraw each tick.
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let action = app.handle_key(key, Utc::now());
            if dispatch(app, action, mode, cfg) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Body;
    use crate::fixtures::sample_cache;
    use crate::model::Kind;
    use crate::slack::fake::FakeSlack;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::RefCell;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                cmd: Cmd::Tui,
                fixtures: false
            })
        );
        assert_eq!(
            parse(&["--fixtures"]),
            Ok(Args {
                cmd: Cmd::Tui,
                fixtures: true
            })
        );
        assert_eq!(parse(&["verify"]).unwrap().cmd, Cmd::Verify);
        assert!(parse(&["bogus"]).is_err());
        assert!(parse(&["verify", "verify"]).is_err());
        assert!(parse(&["verify", "--fixtures"]).is_err()); // verify always hits Slack
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_792_100_000, 0).unwrap()
    }

    fn me() -> Me {
        Me {
            user_id: "U0ME".into(),
            team_id: "T0HEARD".into(),
        }
    }

    fn app(cfg: &Config) -> App {
        let mut app = App::new(cfg.clone());
        app.set_cache(sample_cache(now()));
        app
    }

    fn watch_row() -> Row {
        sample_cache(now())
            .rows
            .into_iter()
            .find(|r| r.kinds.contains(&Kind::Watch))
            .unwrap()
    }

    fn clear_of(row: &Row, unwatch: bool) -> Action {
        Action::Clear {
            row: row.clone(),
            clear: Clear {
                wm: row.newest.clone(),
                keep_watch: false,
                at: now(),
            },
            unwatch,
        }
    }

    fn status(app: &App) -> Option<String> {
        app.screen(now()).status
    }

    fn no_launch(url: &str) -> Result<(), String> {
        panic!("unexpected launch of {url}")
    }

    fn calls(fake: &FakeSlack, method: &str) -> Vec<Vec<(String, String)>> {
        fake.calls
            .borrow()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn reaction_params(name: &str, row: &Row) -> Vec<(String, String)> {
        vec![
            ("name".into(), name.into()),
            ("channel".into(), row.channel.clone()),
            ("timestamp".into(), row.watched.clone().unwrap().0),
        ]
    }

    #[test]
    fn watch_clear_fails_without_clearing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config::default();
        let mut a = app(&cfg);
        let before = a.rows();
        let fake = FakeSlack::new().on(
            "reactions.remove",
            &[],
            Err(SlackError::Api("reactions.remove: missing_scope".into())),
        );
        let row = watch_row();
        run_action(
            &mut a,
            &clear_of(&row, true),
            &fake,
            &state,
            &cfg,
            Some(&me()),
            &no_launch,
        );
        assert_eq!(state.load::<Clears>(CLEARS).unwrap(), Clears::default());
        assert_eq!(a.rows(), before);
        assert_eq!(
            status(&a).as_deref(),
            Some("reactions.remove: missing_scope")
        );
    }

    #[test]
    fn open_unwatch_fails_without_clearing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config {
            open_clears_watch: true,
            ..Config::default()
        };
        let mut a = app(&cfg);
        let before = a.rows();
        let fake = FakeSlack::new().on(
            "reactions.remove",
            &[],
            Err(SlackError::Http("reactions.remove: timed out".into())),
        );
        let row = watch_row();
        let opened = RefCell::new(Vec::new());
        let launch = |url: &str| {
            opened.borrow_mut().push(url.to_owned());
            Ok(())
        };
        let action = Action::Open {
            row: row.clone(),
            clear: Some(Clear {
                wm: row.newest.clone(),
                keep_watch: false,
                at: now(),
            }),
            unwatch: true,
        };
        run_action(&mut a, &action, &fake, &state, &cfg, Some(&me()), &launch);
        assert_eq!(*opened.borrow(), vec![open::app_url(&me(), &row)]);
        assert_eq!(state.load::<Clears>(CLEARS).unwrap(), Clears::default());
        assert_eq!(a.rows(), before);
        assert_eq!(status(&a).as_deref(), Some("reactions.remove: timed out"));
    }

    #[test]
    fn clear_writes_clears_json() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config {
            watch_emoji: vec!["eyes".into(), "white_check_mark".into()],
            ..Config::default()
        };
        let mut a = app(&cfg);
        let before = a.rows().len();
        let fake = FakeSlack::new()
            .on(
                "reactions.remove",
                &[("name", "eyes")],
                Ok(r#"{"ok":true}"#),
            )
            .on(
                "reactions.remove",
                &[("name", "white_check_mark")],
                Err(SlackError::Api("reactions.remove: no_reaction".into())),
            );
        let row = watch_row();
        let action = clear_of(&row, true);
        run_action(
            &mut a,
            &action,
            &fake,
            &state,
            &cfg,
            Some(&me()),
            &no_launch,
        );
        let Action::Clear { clear, .. } = &action else {
            unreachable!()
        };
        let on_disk = state.load::<Clears>(CLEARS).unwrap();
        assert_eq!(on_disk.entries.get(&row.key), Some(clear));
        assert_eq!(on_disk.entries.len(), 1);
        assert_eq!(
            calls(&fake, "reactions.remove"),
            vec![
                reaction_params("eyes", &row),
                reaction_params("white_check_mark", &row)
            ]
        );
        assert_eq!(a.rows().len(), before - 1);
        assert_eq!(
            status(&a).as_deref(),
            Some("Cleared · :eyes: removed · u to undo")
        );
    }

    #[test]
    fn undo_readds_reaction_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config::default();
        let mut a = app(&cfg);
        let before = a.rows();
        let fake = FakeSlack::new()
            .on("reactions.remove", &[], Ok(r#"{"ok":true}"#))
            .on(
                "reactions.add",
                &[],
                Err(SlackError::Api("reactions.add: already_reacted".into())),
            );
        let row = watch_row();
        run_action(
            &mut a,
            &clear_of(&row, true),
            &fake,
            &state,
            &cfg,
            Some(&me()),
            &no_launch,
        );
        assert_eq!(state.load::<Clears>(CLEARS).unwrap().entries.len(), 1);
        let undo = a.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), now());
        assert!(matches!(undo, Action::Undo(UndoEntry { prev: None, .. })));
        run_action(&mut a, &undo, &fake, &state, &cfg, Some(&me()), &no_launch);
        assert_eq!(
            calls(&fake, "reactions.add"),
            vec![reaction_params("eyes", &row)]
        );
        assert_eq!(state.load::<Clears>(CLEARS).unwrap(), Clears::default());
        assert_eq!(a.rows(), before);
        assert_eq!(status(&a).as_deref(), Some("Restored · :eyes: re-added"));
    }

    #[test]
    fn undo_failure_keeps_the_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config::default();
        let mut a = app(&cfg);
        let fake = FakeSlack::new()
            .on("reactions.remove", &[], Ok(r#"{"ok":true}"#))
            .on(
                "reactions.add",
                &[],
                Err(SlackError::Api("reactions.add: missing_scope".into())),
            );
        let row = watch_row();
        let action = clear_of(&row, true);
        run_action(
            &mut a,
            &action,
            &fake,
            &state,
            &cfg,
            Some(&me()),
            &no_launch,
        );
        let cleared = state.load::<Clears>(CLEARS).unwrap();
        let rows = a.rows();
        let undo = a.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), now());
        run_action(&mut a, &undo, &fake, &state, &cfg, Some(&me()), &no_launch);
        assert_eq!(state.load::<Clears>(CLEARS).unwrap(), cleared);
        assert_eq!(a.rows(), rows);
        assert_eq!(status(&a).as_deref(), Some("reactions.add: missing_scope"));
    }

    #[test]
    fn open_web_mode_uses_permalink() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().to_path_buf());
        let cfg = Config {
            open_mode: OpenMode::Web,
            ..Config::default()
        };
        let mut a = app(&cfg);
        let row = watch_row();
        let fake = FakeSlack::new().on(
            "chat.getPermalink",
            &[],
            Ok(r#"{"ok":true,"permalink":"https://heard.slack.com/archives/x"}"#),
        );
        let opened = RefCell::new(Vec::new());
        let launch = |url: &str| {
            opened.borrow_mut().push(url.to_owned());
            Ok(())
        };
        let action = Action::Open {
            row,
            clear: None,
            unwatch: false,
        };
        run_action(&mut a, &action, &fake, &state, &cfg, Some(&me()), &launch);
        assert_eq!(
            *opened.borrow(),
            vec!["https://heard.slack.com/archives/x".to_string()]
        );
        assert_eq!(state.load::<Clears>(CLEARS).unwrap(), Clears::default());
    }

    #[test]
    fn fixtures_mode_loads_sample() {
        let cfg = Config::default();
        let (mut a, mut mode) = fixtures(cfg.clone(), now());
        let s = a.screen(now());
        assert_eq!(s.body, Body::Rows);
        assert_eq!(s.rows.len(), 9);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        let x = a.handle_key(key('x'), now());
        assert!(!dispatch(&mut a, x, &mut mode, &cfg));
        assert_eq!(a.rows().len(), 8, "the clear is kept in memory");
        assert!(status(&a).unwrap().starts_with("fixtures: would clear "));

        let u = a.handle_key(key('u'), now());
        assert!(!dispatch(&mut a, u, &mut mode, &cfg));
        assert_eq!(a.rows().len(), 9);
        assert!(status(&a).unwrap().starts_with("fixtures: would undo "));

        let o = a.handle_key(key('o'), now());
        assert!(!dispatch(&mut a, o, &mut mode, &cfg));
        assert!(
            status(&a)
                .unwrap()
                .starts_with("fixtures: would open slack://channel?team=T0HEARD&id=")
        );

        let r = a.handle_key(key('r'), now());
        assert!(!dispatch(&mut a, r, &mut mode, &cfg));
        assert_eq!(status(&a).as_deref(), Some("fixtures: would refresh"));

        let q = a.handle_key(key('q'), now());
        assert!(dispatch(&mut a, q, &mut mode, &cfg));
    }

    #[test]
    fn no_token_actions_say_so() {
        let cfg = Config::default();
        let mut a = app(&cfg);
        let mut mode = Mode::NoToken;
        let x = a.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), now());
        let before = a.rows();
        assert!(!dispatch(&mut a, x, &mut mode, &cfg));
        assert_eq!(a.rows(), before);
        assert_eq!(status(&a).as_deref(), Some(NO_TOKEN));
        assert!(dispatch(&mut a, Action::Quit, &mut mode, &cfg));
    }
}
