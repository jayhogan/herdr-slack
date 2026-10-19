# herdr-slack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust TUI, run in a herdr pane as `herdr-slack`, that lists the Slack threads waiting on Jay (watch, mention, reply, thread, channel, dm), one row per thread, and lets him open, clear and undo from the keyboard.

**Architecture:** Same shape as herdr-pr-pulse. A background worker thread refreshes Slack into a shared `cache.json` (one fetch across all running copies, guarded by an advisory lock). Pure functions (`collect`, `watch`, `apply_clears`) turn raw Slack reads into rows, so the "never miss a message" rules are unit-tested without a network. The UI thread only reads the cache and writes Jay's clears.

**Tech Stack:** Rust 2024 (MSRV 1.89), ratatui 0.30, crossterm 0.29, serde/serde_json, chrono, ureq 3 (rustls), unicode-width; dev: tempfile, insta.

**Spec:** `docs/superpowers/specs/2026-10-18-herdr-slack-design.md` (read it first; § numbers below refer to it). Template repo: https://github.com/jayhogan/herdr-pr-pulse (read with `gh api repos/jayhogan/herdr-pr-pulse/contents/<path> --jq .content | base64 -d`).

## Global Constraints

- `Cargo.toml`: `edition = "2024"`, `rust-version = "1.89"`; deps exactly: `chrono = { version = "0.4", default-features = false, features = ["std", "clock", "serde"] }`, `crossterm = "0.29"`, `ratatui = "0.30"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `ureq = "3"`, `unicode-width = "0.2"`; dev-deps `tempfile = "3"`, `insta = "1"`. No async runtime. No other crates.
- Flat `src/*.rs` modules plus `src/ui/`, each with a `//!` doc naming the spec section it implements. Errors are `Result<T, String>` (or the typed `SlackError`) with readable messages.
- Binary and plugin id `herdr-slack`. State dir `${XDG_STATE_HOME:-~/.local/state}/herdr-slack/` (mode `0700`, every file in it `0600`). Config `${XDG_CONFIG_HOME:-~/.config}/herdr-slack/config.json`.
- Token: `security find-generic-password -s herdr-slack -w`, else env `HERDR_SLACK_TOKEN`. Never written to disk, logged, or passed as a process argument.
- **Test fixtures are invented data.** Never paste a live Slack response into `tests/fixtures/`: Heard's Slack can contain customer data. Use the names and messages from the design canvas sample (Dana Okafor, Marcus Lee, …) and fake ids (`U0ME`, `C0CORE`, `D0DANA`, `G0MPDM1`, team `T0HEARD`).
- Colors: ANSI 16 only, no Blue or Magenta (pr-pulse's rule). The selected row is `▌` + BOLD with **no background** (pr-pulse's recorded decision; overrides the canvas highlight).
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all clean at the end of every task.
- Commit at the end of each task with a `feat:`/`test:`/`chore:` message ending in the attribution line `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A source fails or isn't read this refresh** (429 after retries, a channel the token can't read, a thread on the slow schedule). Expected: previous rows from it stay, and the failure shows in the status line. Pinned in Task 5 (`failed_source_keeps_rows`, `unread_thread_keeps_row`).
2. **Two copies running at once** (panes in several workspaces). Expected: one fetch at a time; a clear in one pane hides the row in the other within ~2 s. Pinned in Task 7 (`second_worker_waits`) and Task 8 (`reload_clears_hides_row`).
3. **Messages with empty `text`** (workflow/alert posts that only have blocks or attachments). Expected: line 1 shows real words. Pinned in Task 2 (`flatten_uses_blocks_when_text_empty`, `flatten_uses_attachments_last`).
4. **`x` on a watch row while Slack is unreachable.** Expected: nothing is cleared and the error shows. Pinned in Task 10 (`watch_clear_fails_without_clearing`).
5. **A narrow pane** (40 columns) with long names and three or more types. Expected: types stay visible; text, then channel, then author shrink. Pinned in Task 9 (`narrow_40` snapshot, `line2_keeps_types`).

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml`, `herdr-plugin.toml`, `scripts/install.sh`, `.github/workflows/ci.yml`, `.gitignore`, `README.md` | Packaging, as in pr-pulse |
| `src/main.rs` | Args, terminal setup, event loop, `Live` (worker + side effects), fixtures mode |
| `src/ago.rs` | "2 min ago" labels |
| `src/model.rs` | `Ts`, `Kind`, `Msg`, `RowKey`, `Row`, `Names`, JSON parsing of Slack messages |
| `src/text.rs` | Slack markup → readable text; cell-width truncate/wrap |
| `src/config.rs` | `Config` with defaults; load |
| `src/token.rs` | Read the token |
| `src/store.rs` | `StateDir` (copied from pr-pulse, plus permissions) |
| `src/state.rs` | On-disk types: `Cache`, `State`, `Clears` |
| `src/slack.rs` | `Slack` trait, `HttpSlack` (pacing, 429), `SlackError`, `fake::FakeSlack` |
| `src/sources.rs` | One function per Slack read, paging to the end |
| `src/watch.rs` | Watch baseline / recheck rules (§5, §6.1) |
| `src/collect.rs` | Reads + previous rows + clears → rows (§5, §7); `apply_clears` |
| `src/refresh.rs` | One full refresh: schedules, reads, collect, names |
| `src/fetch.rs` | Worker thread, `Poller`, `decide` (as pr-pulse) |
| `src/app.rs` | UI state, keys → `Action`, `Screen` model, undo |
| `src/open.rs` | Slack URL for a row; launching it |
| `src/fixtures.rs` | Sample `Cache` for `--fixtures` and UI tests |
| `src/verify.rs` | `herdr-slack verify` |
| `src/ui/mod.rs`, `src/ui/list.rs`, `src/ui/theme.rs` | Drawing |
| `tests/fixtures/*.json` | Invented Slack API responses |

---

### Task 1: Scaffold, args, ages

**Files:**
- Create: `Cargo.toml`, `herdr-plugin.toml`, `scripts/install.sh`, `.github/workflows/ci.yml`, `.gitignore`, `src/main.rs`, `src/ago.rs`
- Modify: `README.md`

**Interfaces:**
- Produces: `ago::long(ts: DateTime<Utc>, now: DateTime<Utc>) -> String`; `main::parse_args(&[String]) -> Result<Args, String>` with `struct Args { cmd: Cmd, fixtures: bool }`, `enum Cmd { Tui, Verify }`.

- [ ] **Step 1: Packaging files.** Copy pr-pulse's `herdr-plugin.toml` (id `herdr-slack`, name `Slack inbox`, description `What needs you in Slack.`, `min_herdr_version = "0.9.0"`, platforms linux+macos, `[[build]] cargo build --release`), `scripts/install.sh` (binary `herdr-slack`), `ci.yml` and `.gitignore` verbatim, renaming. `Cargo.toml` per Global Constraints.

- [ ] **Step 2: Failing tests** in `src/ago.rs` and `src/main.rs`:

```rust
#[test] fn long_boundaries() {   // now = 2026-10-18T12:00:00Z, s(n) = long(now - n secs, now)
    assert_eq!(s(0), "just now");   assert_eq!(s(59), "just now");
    assert_eq!(s(60), "1 min ago"); assert_eq!(s(59*60+59), "59 min ago");
    assert_eq!(s(3600), "1 hour ago"); assert_eq!(s(2*3600), "2 hours ago");
    assert_eq!(s(24*3600), "1 day ago"); assert_eq!(s(3*24*3600), "3 days ago");
    assert_eq!(s(-300), "just now");
}
#[test] fn args() {
    assert_eq!(parse(&[]), Ok(Args { cmd: Cmd::Tui, fixtures: false }));
    assert_eq!(parse(&["--fixtures"]), Ok(Args { cmd: Cmd::Tui, fixtures: true }));
    assert_eq!(parse(&["verify"]).unwrap().cmd, Cmd::Verify);
    assert!(parse(&["bogus"]).is_err());
    assert!(parse(&["verify", "verify"]).is_err());
    assert!(parse(&["verify", "--fixtures"]).is_err()); // verify always hits Slack
}
```

- [ ] **Step 3: Run** `cargo test` → FAIL (functions missing).
- [ ] **Step 4: Implement** `ago::long` and `parse_args`; `main` prints usage `usage: herdr-slack [verify] [--fixtures]` and exits 2 on a bad arg, otherwise exits 0 for now.
- [ ] **Step 5: Run** `cargo test && cargo clippy --all-targets -- -D warnings` → PASS.
- [ ] **Step 6: Commit** `chore: scaffold herdr-slack crate`.

---

### Task 2: Model and message text

**Files:**
- Create: `src/model.rs`, `src/text.rs`, `tests/fixtures/msg-rich.json`, `tests/fixtures/msg-attach.json`

**Interfaces:**
- Produces (`model.rs`):

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ts(pub String);              // Slack "1792096026.762699"
impl Ord for Ts {}                      // by (secs u64, micros u32); unparsable sorts first
impl Ts { pub fn from_unix(secs: i64) -> Ts; pub fn to_utc(&self) -> DateTime<Utc>; }

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Kind { Watch, Mention, Reply, Thread, Channel, Dm }   // declaration order = display order
impl Kind { pub fn label(self) -> &'static str }                // "watch", "mention", …

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    pub channel: String, pub ts: Ts,
    pub thread_ts: Option<Ts>,   // the thread's root ts; Some(ts) on a root that has replies
    pub reply_count: u32,
    pub user: String,            // `user`, else `bot_id`, else ""
    pub subtype: Option<String>,
    pub body: String,            // text::flatten output; still holds <@U…> tokens
}
impl Msg { pub fn root(&self) -> Ts /* thread_ts or ts */ }
pub fn parse_message(channel: &str, v: &Value) -> Option<Msg>;   // history/replies element
pub fn parse_search_match(v: &Value) -> Option<Msg>;             // channel = v.channel.id; thread_ts from permalink `?thread_ts=`
pub fn msg_id(channel: &str, ts: &Ts) -> String;                  // "C0CORE:1792096026.762699"

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RowKey(pub String);
impl RowKey { pub fn thread(channel: &str, root: &Ts) -> RowKey; pub fn dm(channel: &str) -> RowKey /* "D0DANA:dm" */ }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub key: RowKey, pub channel: String,
    pub kinds: Vec<Kind>,        // sorted, no duplicates
    pub shown: Msg,              // newest unhandled message; for a watch-only row, the watched message
    pub oldest: Ts,              // oldest unhandled message (sort key "waiting")
    pub newest: Ts,              // newest message in the row (age label, clear watermark)
    pub pending: usize,
    pub watched: Option<Ts>,     // ts of the message carrying Jay's watch reaction
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Names { pub users: BTreeMap<String, String>, pub channels: BTreeMap<String, String> }
// channels values are display labels: "#incidents", "@Dana Okafor", "@Ava Chen, Sam Patel"
```

- Produces (`text.rs`): `flatten(v: &Value) -> String`; `display(body: &str, names: &Names) -> String`; `one_line(s: &str) -> String`; `ids(body: &str) -> (Vec<String>, Vec<String>)` (user ids, channel ids referenced); `width(s: &str) -> usize`; `truncate(s: &str, w: usize) -> String`; `wrap(s: &str, w: usize, max_lines: usize) -> Vec<String>`.

- [ ] **Step 1: Failing tests.** Write `tests/fixtures/msg-rich.json` (a message with `"text": ""` and `blocks`: a `header`, a `section` whose `text.text` contains `<@U0ME>`, a `context` element) and `msg-attach.json` (empty text, no blocks, `attachments[0]` with `title` and `text`). Tests:

```rust
#[test] fn ts_orders_numerically() { assert!(Ts("1700000000.000100".into()) > Ts("999999999.999999".into()));
    assert!(Ts("1700000000.000100".into()) > Ts("1700000000.000099".into())); }
#[test] fn search_match_thread_from_permalink() // permalink ".../p1792…?thread_ts=1792000000.000100&cid=C0CORE" → thread_ts Some, root() == that ts
#[test] fn search_match_top_level_has_no_thread()
#[test] fn flatten_prefers_text()
#[test] fn flatten_uses_blocks_when_text_empty()   // msg-rich.json → contains header text and "<@U0ME>"
#[test] fn flatten_uses_attachments_last()         // msg-attach.json → contains title and text
#[test] fn display_resolves_tokens() {
    // names: U0MARCUS→"Marcus Lee", C0CORE→"#core-eng"
    assert_eq!(display("hey <@U0MARCUS> see <#C0CORE> and <#C0X|ops> <!subteam^S1|@core> <!here> <https://x.io|doc> <https://y.io> a &amp; b &lt;c&gt;", &names),
               "hey @Marcus Lee see #core-eng and #ops @core @here doc https://y.io a & b <c>");
    assert_eq!(display("<@U0NOBODY>", &names), "@U0NOBODY");
}
#[test] fn one_line_collapses() { assert_eq!(one_line("a\n\n b\t c"), "a b c"); }
#[test] fn truncate_counts_cells() { assert_eq!(truncate("日本語テキスト", 7), "日本語…"); assert_eq!(truncate("abc", 3), "abc"); assert_eq!(truncate("abcd", 3), "ab…"); }
#[test] fn wrap_ellipsis_on_last_line() { assert_eq!(wrap("aa bb cc dd ee", 5, 2), vec!["aa bb", "cc d…"]); }
```

- [ ] **Step 2: Run** `cargo test model text` → FAIL.
- [ ] **Step 3: Implement.** `flatten`: non-empty `text` → it; else walk `blocks` (`rich_text` elements: `text`→text, `user`→`<@id>`, `channel`→`<#id>`, `usergroup`→`<!subteam^id>`, `broadcast`→`<!range>`, `link`→its text or url, `emoji`→`:name:`; `section.text.text`, `section.fields[].text`, `header.text.text`, `context.elements[].text`), joining blocks with `\n`; else `attachments[]` `pretext`, `title`, `text`, `fallback` (first non-empty of each attachment, then joined). Width via `unicode-width`.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: slack message model and text rendering`.

---

### Task 3: Store, on-disk state, config, token

**Files:**
- Create: `src/store.rs`, `src/state.rs`, `src/config.rs`, `src/token.rs`

**Interfaces:**
- Consumes: `model::{Row, Names, Ts, RowKey}`.
- Produces:
  - `store::StateDir` copied from pr-pulse `src/store.rs` (methods `new`, `default_location` → `herdr-slack`, `load`, `update`, `try_fetch_lock`, `fetch_lock`; its tests), plus: the dir is created `0700` and every written file (including `.tmp` before rename and lock files) `0600`.
  - `state.rs`:

```rust
pub const CACHE: &str = "cache.json";   // written by the fetching worker
pub const STATE: &str = "state.json";   // worker-owned bookkeeping
pub const CLEARS: &str = "clears.json"; // UI-owned (worker only flips keep_watch, Task 6)

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32, pub fetched_at: Option<DateTime<Utc>>,
    pub error: Option<SlackError>, pub error_at: Option<DateTime<Utc>>,   // whole-refresh failure (auth)
    pub warnings: Vec<String>,     // per-source failures and truncations from the last refresh
    pub me: Option<Me>, pub rows: Vec<Row>, pub names: Names,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)] pub struct Me { pub user_id: String, pub team_id: String }

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32, pub refresh_no: u64,
    pub watch: WatchState,
    pub known_threads: BTreeMap<String, Ts>,   // msg_id(channel, root) → last activity ts
    pub mpims: Vec<String>, pub mpims_listed_at: Option<DateTime<Utc>>, pub mpim_cursor: usize,
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WatchState {
    pub baseline: Option<BTreeSet<String>>,    // msg ids; None until the first successful read
    pub watched: BTreeMap<String, Msg>,        // msg id → the watched message
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Clears { pub entries: BTreeMap<RowKey, Clear> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clear { pub wm: Ts, pub keep_watch: bool, pub at: DateTime<Utc> }
```

(`SlackError` is defined in Task 4; in this task define it in `slack.rs` as just the enum so `state.rs` compiles. Its variants are listed in Task 4.)

  - `config.rs`: `Config` with serde field names exactly as §10 (`channels`, `watch_emoji`, `extra_mention_queries`, `lookback_days`, `channel_lookback_hours`, `refresh_secs`, `retention_days`, `open_mode`, `open_clears_watch`) plus `ignore_users: Vec<String>`; defaults `[]`, `["eyes"]`, `[]`, `7`, `48`, `180`, `14`, `OpenMode::App`, `false`, `[]`; `#[serde(default)]` so partial files work; `enum OpenMode { App, Web }` serialized lowercase. `pub fn load(path: &Path) -> Result<Config, String>` (missing file → defaults; bad JSON → `Err("config.json: <serde error>")`); `pub fn default_path() -> PathBuf`.
  - `token.rs`: `pub fn read() -> Result<String, String>`: run `security find-generic-password -s herdr-slack -w` (macOS), trim; on failure or non-macOS use `HERDR_SLACK_TOKEN`; neither → `Err` with the text of §9's auth banner. `fn pick(keychain: Option<String>, env: Option<String>) -> Result<String, String>` is the tested core.

- [ ] **Step 1: Failing tests:** pr-pulse's store tests renamed to the new `Cache`; `files_are_private` (after `update`, the file mode `& 0o777 == 0o600`, dir `0o700`; `#[cfg(unix)]`); `config_partial_file_keeps_defaults` (`{"channels":["#incidents"]}` → `refresh_secs == 180`, `watch_emoji == ["eyes"]`); `config_missing_is_default`; `config_bad_json_errors`; `token_prefers_keychain`, `token_falls_back_to_env`, `token_missing_errors` (via `pick`, empty strings count as missing).
- [ ] **Step 2: Run** `cargo test` → FAIL.
- [ ] **Step 3: Implement.** Set permissions with `std::os::unix::fs::{PermissionsExt, OpenOptionsExt}` (`.mode(0o600)` on open; `set_permissions` on the dir after `create_dir_all`).
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: state dir, config and token`.

---

### Task 4: Slack client and paged sources

**Files:**
- Create: `src/slack.rs`, `src/sources.rs`, `tests/fixtures/search-p1.json`, `search-p2.json`, `reactions.json`, `replies.json`, `history.json`, `history-p2.json`, `mpims.json`, `user.json`
- Test: in-module tests

**Interfaces:**
- Produces (`slack.rs`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SlackError {
    Auth(String),        // not_authed, invalid_auth, token_revoked, account_inactive, missing token
    RateLimited(String), // method name, after retries
    Api(String),         // "<method>: <error code>"
    Http(String),
}
pub trait Slack: Send {
    /// GET https://slack.com/api/<method> with Bearer auth; the parsed body when "ok": true.
    fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value, SlackError>;
}
pub struct HttpSlack { /* token, ureq agent, pacing: bool, last call per tier */ }
impl HttpSlack { pub fn new(token: String, paced: bool) -> HttpSlack }
#[cfg(test)]
pub mod fake {
    pub struct FakeSlack { pub calls: RefCell<Vec<(String, Vec<(String, String)>)>> }
    impl FakeSlack { pub fn new() -> Self;
        /// Answers `method` when every (k, v) in `when` is among the call's params; later rules win.
        pub fn on(self, method: &str, when: &[(&str, &str)], answer: Result<&str, SlackError>) -> Self; }
}
```

  `HttpSlack` rules: tier 2 methods (`search.messages`, `reactions.list`) at least **4 s** apart; all other methods at least **1.5 s** apart, when `paced`. HTTP 429 → sleep `Retry-After` seconds (default 30), retry, at most 3 retries, then `RateLimited`. Auth error codes above → `Auth`. Other `"ok": false` → `Api`. Token only in the `Authorization` header.

- Produces (`sources.rs`), each paging to the end:

```rust
pub struct Searched { pub msgs: Vec<Msg>, pub total: u64, pub truncated: bool }
pub fn search(s: &dyn Slack, query: &str) -> Result<Searched, SlackError>;          // count=100, page=1.. until paging.pages; Slack's page cap (100) → truncated
pub fn watches(s: &dyn Slack, me: &str, emoji: &[String]) -> Result<(Vec<Msg>, bool), SlackError>; // ONE call, limit=1000 (cursor paging is broken, §4); bool = complete (items < 1000)
pub fn reaction_present(s: &dyn Slack, me: &str, emoji: &[String], channel: &str, ts: &Ts) -> Result<bool, SlackError>; // reactions.get
pub fn replies(s: &dyn Slack, channel: &str, root: &Ts) -> Result<Vec<Msg>, SlackError>;        // cursor-paged, limit=200
pub fn history(s: &dyn Slack, channel: &str, oldest: &Ts) -> Result<Vec<Msg>, SlackError>;      // cursor-paged, limit=200, top-level only (drop thread_ts != ts)
pub fn mpims(s: &dyn Slack) -> Result<Vec<String>, SlackError>;                                  // users.conversations types=mpim, exclude_archived, cursor-paged
pub fn user_name(s: &dyn Slack, id: &str) -> Result<String, SlackError>;                         // users.info → profile.display_name, else real_name, else name
pub fn channel_label(s: &dyn Slack, me: &str, id: &str, users: &mut dyn FnMut(&str) -> String) -> Result<String, SlackError>; // conversations.info: is_im → "@<partner>"; is_mpim → "@a, b" (members minus me); else "#name"
pub fn me(s: &dyn Slack) -> Result<Me, SlackError>;                                              // auth.test
```

- [ ] **Step 1: Write the fixtures** (invented, see Global Constraints): two search pages (`paging.pages: 2`), a `reactions.list` with 3 items (one `:eyes:` by `U0ME`, one `:eyes:` by someone else, one `:tada:` by `U0ME`), a `conversations.replies` thread (root by `U0ME`, two replies), two `conversations.history` pages linked by `response_metadata.next_cursor` and `has_more`.
- [ ] **Step 2: Failing tests:**

```rust
#[test] fn search_pages_to_end()            // 2 calls, page=1 then page=2; msgs from both; total from paging
#[test] fn search_marks_truncated_at_cap()  // paging.pages = 150 → stops after page 100, truncated = true
#[test] fn watches_one_call_limit_1000()    // calls == [("reactions.list", [("limit","1000")…])]; only U0ME's eyes message returned; complete = true (3 < 1000)
#[test] fn history_follows_cursor_and_drops_replies()
#[test] fn replies_returns_root_first()
#[test] fn error_codes_map() // FakeSlack answering Err(Auth("invalid_auth")) passes through; HttpSlack's code→variant mapping tested via `fn classify(method, body: &Value) -> Result<Value, SlackError>`
#[test] fn retry_after_parsed() // `fn retry_after(header: Option<&str>) -> Duration`: Some("7") → 7 s, None/garbage → 30 s
#[test] #[ignore] fn live_auth_and_search() // HttpSlack with token::read(); me() ok; search("from:me after:<7 days ago>") ok — prints counts only, never text
```

- [ ] **Step 3: Run** `cargo test slack sources` → FAIL.
- [ ] **Step 4: Implement.** `search` uses `after:` with the date **one day before** the floor (it's day-granular and exclusive) — callers pass the floor `DateTime`; signature takes the full query, so add `pub fn after(floor: DateTime<Utc>) -> String` producing `after:YYYY-MM-DD` for `floor - 1 day`, tested in `after_is_day_before`.
- [ ] **Step 5: Run** `cargo test` → PASS. `cargo test live_ -- --ignored` passes on Jay's machine (run it; note the result in the commit message body).
- [ ] **Step 6: Commit** `feat: slack client with paging, pacing and retries`.

---

### Task 5: Collection rules

**Files:**
- Create: `src/watch.rs`, `src/collect.rs`

**Interfaces:**
- Consumes: `model::*`, `state::{WatchState, Clears, Clear}`.
- Produces:

```rust
// watch.rs (§5 watch rules, §6 source 1)
/// Applies one reactions read. `read` = currently watched messages (Jay's reaction, any watch emoji);
/// `complete` = the read held every reaction; `present` = reactions.get answers for ids asked via `to_recheck`.
pub fn apply(state: &mut WatchState, read: &[Msg], complete: bool, present: &BTreeMap<String, bool>);
/// Previously watched ids missing from an incomplete read: re-check them with reactions.get.
pub fn to_recheck(state: &WatchState, read: &[Msg], complete: bool) -> Vec<(String, Ts)>;   // (channel, ts)

// collect.rs (§5, §7)
pub type Read<T> = Result<T, String>;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct ThreadId { pub channel: String, pub root: Ts }
pub struct Reads {
    pub me: String,
    pub ignore: BTreeSet<String>,                         // config.ignore_users
    pub watched: Read<Vec<Msg>>,                          // WatchState.watched after watch::apply, or Err
    pub mentions: Read<Vec<Msg>>,                         // all mention queries merged
    pub threads: BTreeMap<ThreadId, Read<Vec<Msg>>>,      // replies read this refresh (absent = not read)
    pub channels: BTreeMap<String, Read<Vec<Msg>>>,       // curated, top-level
    pub dms: BTreeMap<String, Read<Vec<Msg>>>,            // IM / MPIM history, top-level (absent = not read)
    pub dm_channels: BTreeSet<String>,                    // every channel id known to be an IM or MPIM
}
pub struct Windows { pub mention_floor: Ts, pub channel_floor: Ts, pub retention_floor: Ts }
pub fn collect(reads: &Reads, prev: &[Row], clears: &Clears, win: &Windows) -> Vec<Row>;
/// What the UI shows between refreshes: hides rows a newer clear covers (§9).
pub fn apply_clears(rows: Vec<Row>, clears: &Clears) -> Vec<Row>;
/// Threads Jay is in, from a refresh's reads: thread id → last activity.
pub fn activity(reads: &Reads) -> BTreeMap<ThreadId, Ts>;
```

Rules `collect` implements (from §5/§7; each is a test below):
1. A message by `me` or a user in `ignore` never creates a row; skip system subtypes (`channel_join`, `channel_leave`, `channel_topic`, `channel_purpose`, `channel_name`, `channel_archive`, `channel_unarchive`, `group_join`, `group_leave`, `group_topic`, `group_purpose`, `group_name`, `pinned_item`, `unpinned_item`, `message_deleted`, `message_changed`, `reminder_add`, `bot_add`, `bot_remove`, `joiner_notification_for_inviter`).
2. Every event is keyed `RowKey::thread(channel, root)`, except top-level messages in a `dm_channels` channel with `reply_count == 0` and not watched, which key `RowKey::dm(channel)`. One row per key; `kinds` is the union.
3. `reply`/`thread`: from `threads[id]` = messages by others after Jay's last message in that thread; `reply` if the root's user is `me`, else `thread`. No message by Jay in the thread → neither kind.
4. `mention`: dropped when `threads[id]` read OK and Jay posted in it after the mention.
5. `dm`: from `dms[ch]` = messages by others after Jay's last top-level message there; plus `dm` added to any thread row whose channel is in `dm_channels`.
6. `watch`: one per `watched` message, on its thread's row (`watched = Some(ts)`); not filtered by watermarks.
7. Watermarks: an event with `ts <= wm` is dropped, where `wm` = `clears[key].wm`, and for a thread row in a DM channel `max(clears[thread key].wm, clears[dm key].wm)`.
8. Carry-forward (§7): a `prev` row not rebuilt this refresh is kept unchanged when the read that would resolve it failed or didn't run (`threads[id]` absent/Err for thread rows; `dms[ch]` absent/Err for DM rows; `channels[ch]` Err for channel rows; `mentions` Err for mention-only rows), or when its `oldest` is older than the window floor for its kinds (aged out, not resolved) — but never when `newest < retention_floor`, unless it has `Watch`. A carried row loses `Watch` if `watched` is Ok and doesn't contain it.
9. `shown` = newest unhandled event's message (watch-only rows: the watched message); `oldest`/`newest`/`pending` from the unhandled events.

`apply_clears`: drop a row whose `newest <= wm` and (`!keep_watch` or no `Watch`); with `keep_watch` and `Watch`, keep it with `kinds == [Watch]`. Rows with no clear pass through.

`watch::apply`: first successful read with `baseline == None` → `baseline = ids in read`, `watched` stays empty. Later: `baseline` keeps only ids still in `read`; `watched` = read minus baseline, plus previously watched ids absent from an incomplete read whose `present` answer is `true` or missing (unknown stays).

- [ ] **Step 1: Failing tests** — build `Reads` with small helpers (`msg(ch, ts, user, body)`, `reply(ch, ts, root, user)`), `me = "U0ME"`:

```rust
#[test] fn one_row_per_thread_with_all_kinds()   // a thread Jay started, a reply mentioning him, and his :eyes: on the root → 1 row, kinds [Watch, Mention, Reply]
#[test] fn thread_vs_reply_by_root_author()
#[test] fn replies_only_after_my_last_post()      // others at t1, Jay t2, other t3 → pending 1, shown = t3
#[test] fn mention_drops_after_i_answer()
#[test] fn my_own_and_ignored_messages_never_count()
#[test] fn system_subtypes_skipped()
#[test] fn dm_toplevel_groups_into_one_row()      // 3 top-level DM messages → 1 row key "D0DANA:dm", pending 3, kinds [Dm]
#[test] fn dm_message_with_replies_moves_to_thread_row() // reply_count 2 → own thread row, kinds include Dm; DM row doesn't contain it
#[test] fn dm_watermark_carries_to_thread_row()   // clear on DM row at t5; that message later gets replies → thread row shows only replies after t5
#[test] fn channel_posts_top_level_only()
#[test] fn watermark_hides_old_shows_new()
#[test] fn failed_source_keeps_rows()             // prev has a channel row; channels[C] = Err → row unchanged in output
#[test] fn unread_thread_keeps_row()              // prev thread row; threads map lacks its id → kept
#[test] fn successful_read_resolves_row()         // thread read OK and Jay replied after → row gone
#[test] fn aged_out_row_kept_until_retention()    // oldest < mention_floor, not in reads → kept; newest < retention_floor → dropped
#[test] fn watch_never_ages_out()                 // watch row with newest < retention_floor stays while watched
#[test] fn carried_row_loses_watch_when_unwatched()
#[test] fn apply_clears_keep_watch()              // O-style clear: row stays with kinds [Watch]
#[test] fn apply_clears_hides_until_newer()
// watch.rs
#[test] fn first_read_sets_baseline_and_watches_nothing()
#[test] fn new_eyes_after_baseline_count()
#[test] fn baseline_entry_dropped_when_reaction_gone_then_readd_counts()
#[test] fn incomplete_read_rechecks_missing_watch() // to_recheck lists it; present=false → unwatched; missing answer → kept
#[test] fn complete_read_drops_missing_watch_without_recheck()
```

- [ ] **Step 2: Run** `cargo test collect watch` → FAIL.
- [ ] **Step 3: Implement** `watch.rs` and `collect.rs` per the rules above.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: collection rules for rows, clears and watches`.

---

### Task 6: One refresh

**Files:**
- Create: `src/refresh.rs`
- Test: in-module, with `FakeSlack`

**Interfaces:**
- Consumes: `sources::*`, `collect::*`, `watch::*`, `state::*`, `config::Config`.
- Produces:

```rust
/// One full refresh (§6). Err only when the whole refresh is invalid (auth.test fails);
/// per-source failures become `Cache.warnings` and carried-forward rows.
pub fn run(s: &dyn Slack, cfg: &Config, state: &mut State, clears: &mut Clears, prev: &Cache, now: DateTime<Utc>) -> Result<Cache, SlackError>;
pub fn thread_due(id: &str, last_activity: &Ts, refresh_no: u64, now: DateTime<Utc>) -> bool;
pub fn mpim_slice(all: &[String], cursor: usize, refresh_secs: u64) -> (Vec<String>, usize); // (to read, next cursor)
```

Order inside `run`: `me` → watches (+ `to_recheck` via `reaction_present`, + `watch::apply`) → each mention query (`<@me>` and `extra_mention_queries`, `after(now - lookback_days)`) → `to:me` → `from:me` → threads → curated channels (`history(ch, Ts::from_unix(now - channel_lookback_hours))`; `#name` entries resolved to ids via `conversations.list types=public_channel,private_channel` once, cached in `Names.channels` reverse lookup) → DMs → `collect` → names for every user/channel id in rows (`text::ids` + `Row.channel` + `shown.user`) not in `prev.names` → prune.

- Threads to read: ids from `known_threads` (plus new ones from this refresh's mention, `to:me`, `from:me` hits with a thread, and watched messages) where `thread_due` is true. New ids are always due. After the refresh, `known_threads` = old ∪ `collect::activity`, minus entries whose last activity is older than **14 days** (§6 step 5).
- `thread_due`: last activity within 2 days → true every refresh; else true when `(refresh_no + hash(id)) % 5 == 0` (each quiet thread read every 5th refresh, spread out). Use a stable hash (sum of bytes) so tests are deterministic.
- DMs to read: IM ids from `to:me` hits, every channel of a current DM row, and `mpim_slice` of `state.mpims` (re-list `mpims` when `mpims_listed_at` is over 6 hours old). `mpim_slice`: `k = max(1, 600 / refresh_secs)`; read `ceil(len / k)` ids starting at `cursor`, wrapping; returns the next cursor. History `oldest` = `now - lookback_days`.
- Warnings: `"<source>: <error>"` for each failed read; `"<query>: truncated"` when a search is truncated.
- Re-added watch (§9 note): for each `Clear` with `!keep_watch` and `at` before this refresh, if the row's message is in `watched` now, set `keep_watch = true` (the watch was re-added after `x`).
- Prune `clears` entries whose `wm` is older than `retention_days`, unless the row is watched.

- [ ] **Step 1: Failing tests** (FakeSlack with Task 4 fixtures plus small inline JSON):

```rust
#[test] fn full_refresh_builds_rows()       // one watch (after baseline seeded in State), one mention, one DM, one channel post → 4 rows, warnings empty, refresh_no incremented
#[test] fn source_failure_becomes_warning() // conversations.history for C0INC answers Err(Api) → warning "#incidents: …", prev row kept
#[test] fn auth_failure_is_err()            // auth.test Err(Auth) → Err(Auth(..)), state unchanged
#[test] fn thread_due_schedule()            // recent → always; 5 days quiet → true for exactly 1 of 5 consecutive refresh_no
#[test] fn mpim_slice_covers_all_in_window() { // 160 ids, refresh_secs 180 → k=3, 54 per call; 3 calls cover every id once
}
#[test] fn known_threads_expire_after_14_days_quiet()
#[test] fn new_threads_read_immediately()
#[test] fn readded_watch_flips_keep_watch()
#[test] fn names_fetched_once()             // second run with same rows makes no users.info calls
```

- [ ] **Step 2: Run** `cargo test refresh` → FAIL.
- [ ] **Step 3: Implement** `refresh.rs`.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: full refresh with schedules`.

---

### Task 7: Background worker

**Files:**
- Create: `src/fetch.rs`

**Interfaces:**
- Consumes: `refresh::run`, `store::StateDir`, `state::*`.
- Produces — the pr-pulse `fetch.rs` shape without `Recheck`/`Progress`:

```rust
pub enum FetchRequest { Scheduled, Forced }
pub enum FetchEvent { Started, Done(Cache) }
pub enum Plan { UseCache, Fetch, WaitForOther }
pub fn decide(req: &FetchRequest, cache_age: Option<chrono::Duration>, have_lock: bool, fresh_secs: i64) -> Plan;
pub struct Poller { /* next_scheduled, in_flight */ }
impl Poller { pub fn new(now: Instant, every: Duration) -> Poller; pub fn due(&mut self, now: Instant) -> Vec<FetchRequest>;
              pub fn forced(&mut self) -> FetchRequest; pub fn on_done(&mut self, now: Instant); }
pub fn spawn(slack: Box<dyn Slack>, cfg: Config, state: StateDir) -> (Sender<FetchRequest>, Receiver<FetchEvent>);
```

  The worker, holding the fetch lock, loads `State`, `Clears` and the previous `Cache`, runs `refresh::run`, then writes `state.json` and `cache.json` with `StateDir::update` and `clears.json` with an `update` closure that changes only `keep_watch` flags and pruned entries (re-read under the lock: never overwrite clears the UI wrote meanwhile). On `Err`, keep the previous rows and set `error`/`error_at` (pr-pulse's `save` pattern). `fresh_secs = cfg.refresh_secs`. Age = `max(fetched_at, error_at)`.

- [ ] **Step 1: Failing tests** — adapt pr-pulse's `decide_table`, `worker_keeps_prs_on_error` (→ `worker_keeps_rows_on_error`), `wait_for_other_waits_for_its_fetch` (→ `second_worker_waits`), `scheduled_after_error_uses_cache`, `worker_exits_when_receiver_dropped`, plus `worker_preserves_ui_clears` (a clear written to `clears.json` while the fake refresh is mid-flight survives the worker's write).
- [ ] **Step 2: Run** `cargo test fetch` → FAIL.
- [ ] **Step 3: Implement** `fetch.rs`.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: shared background refresh`.

---

### Task 8: App state and keys

**Files:**
- Create: `src/app.rs`, `src/fixtures.rs`

**Interfaces:**
- Consumes: `state::{Cache, Clears, Clear}`, `collect::apply_clears`, `model::*`, `ago::long`, `text::*`, `config::Config`.
- Produces:

```rust
pub enum Sort { OldestWaiting, NewestActivity }
pub enum Action {
    None, Quit, Refresh,
    Open { row: Row, clear: Option<Clear> },     // clear = Some for `O`
    Clear { row: Row, clear: Clear, unwatch: bool },   // `x`; unwatch when the row has Watch
    Undo(UndoEntry),
}
pub struct UndoEntry { pub key: RowKey, pub prev: Option<Clear>, pub rewatch: Option<(String, Ts)> } // (channel, ts) to re-add the reaction
pub struct App { /* cache, clears, sel, expanded: BTreeSet<RowKey>, sort, status, undo: Vec<UndoEntry>, cfg */ }
impl App {
    pub fn new(cfg: Config) -> App;
    pub fn set_cache(&mut self, c: Cache);                 // keeps the selection on the same RowKey when it still exists
    pub fn set_clears(&mut self, c: Clears);               // reloaded every 2 s by main
    pub fn set_loading(&mut self, on: bool);
    pub fn set_status(&mut self, s: String);
    pub fn handle_key(&mut self, key: KeyEvent, now: DateTime<Utc>) -> Action;
    /// After main carried out an action: record it locally (clears + undo) or report failure.
    pub fn applied(&mut self, action: &Action, result: Result<(), String>);
    pub fn rows(&self) -> Vec<Row>;                        // apply_clears(cache.rows, clears), sorted
    pub fn screen(&self, now: DateTime<Utc>) -> Screen;
}
pub struct Screen { pub header: Header, pub rows: Vec<RowView>, pub selected: usize, pub status: Option<String>, pub body: Body }
pub enum Body { Loading, Empty, Rows, AuthError(String) }
pub struct Header { pub count: usize, pub by_kind: Vec<(Kind, usize)>, pub sort: Sort, pub synced: Option<String>, pub warning: Option<String> }
pub struct RowView { pub text: String, pub age: String, pub author: String, pub channel: String, pub kinds: Vec<Kind>, pub expanded: bool }
// fixtures.rs
pub fn sample_cache(now: DateTime<Utc>) -> Cache;   // the 8 canvas items, `todo` → `watch`, plus one `dm` row; me = U0ME, team T0HEARD
```

Key map (§9): `j`/`Down` +1, `k`/`Up` −1 (clamped), `Space` toggle expanded, `o` → `Open{clear: None}`, `O` → `Open{clear: Some(Clear{wm: row.newest, keep_watch: has Watch && !cfg.open_clears_watch, at: now})}`, `x` → `Clear{clear: Clear{wm: row.newest, keep_watch: false, at: now}, unwatch: has Watch}`, `u` → `Undo(last)` or status `Nothing to undo`, `r` → `Refresh`, `s` toggles sort (selection to 0), `g`/`G` top/bottom, `q`/`Esc` → `Quit`. Sorting: `OldestWaiting` by `oldest` ascending; `NewestActivity` by `newest` descending; ties by key.

`applied` on `Ok` for `Clear`/`Open{Some}`: write the clear into its local `clears`, push `UndoEntry{prev: old clear, rewatch: unwatch.then(watched msg)}`, status `Cleared · u to undo` (watch: `Cleared · :eyes: removed · u to undo`; `O` on a watch with keep: `Opened · watch kept`). On `Err(e)`: nothing changes, status = `e`. For `Undo`: restore `prev` (or remove), status `Restored` / `Restored · :eyes: re-added`.

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn keys_move_and_clamp()
#[test] fn space_expands_selected_only()
#[test] fn sort_oldest_waiting_default()   // sample_cache: first row is the oldest `oldest`
#[test] fn x_makes_clear_action_with_unwatch_for_watch_rows()
#[test] fn big_o_keeps_watch_by_default()  // keep_watch true for a watch row; open_clears_watch=true → false
#[test] fn applied_ok_hides_row_and_undo_restores()
#[test] fn applied_err_changes_nothing()
#[test] fn undo_is_multilevel()
#[test] fn reload_clears_hides_row()        // set_clears with a clear from another pane → row gone from rows()
#[test] fn selection_follows_row_across_set_cache()
#[test] fn header_counts_by_kind()
#[test] fn auth_error_body()                // cache.error = Auth → Body::AuthError(banner text from §9/token.rs)
```

- [ ] **Step 2: Run** `cargo test app` → FAIL.
- [ ] **Step 3: Implement** `app.rs` and `fixtures.rs`. `RowView.author` = `names.users[shown.user]` or the id; `channel` = `names.channels[row.channel]` or the id; `text` = `one_line(display(shown.body, names))`; `age` = `ago::long(row.newest.to_utc(), now)`.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: app state, keys and undo`.

---

### Task 9: Drawing

**Files:**
- Create: `src/ui/mod.rs`, `src/ui/list.rs`, `src/ui/theme.rs`, snapshots under `src/ui/snapshots/`

**Interfaces:**
- Consumes: `app::{Screen, Body, Header, RowView}`.
- Produces: `ui::list::render(frame: &mut Frame, screen: &Screen, scroll: &mut usize)`; `theme::kind(k: Kind) -> Style` = Watch Yellow, Mention Red+BOLD, Reply Cyan, Thread Green, Channel Reset+BOLD, Dm Cyan+BOLD; `theme::dim()`, `theme::bold()`, `theme::bar()` (REVERSED) as pr-pulse.

Layout top to bottom: header bar (1 line), list, status line (1 line), key hints (1+ lines, wrapped).
- Header: `Slack inbox │ {n} need attention · {k} {kind} …   sort: oldest waiting │ synced {age}`; when narrow, drop the per-kind breakdown first, then `sort:`. A warning shows as `⚠ {warning}` replacing the breakdown.
- Each row: line 1 = `▌` (selected) or space, text truncated to fill, two spaces, age right-aligned; line 2 = author, two spaces, channel, two spaces, `[kind]` per kind in `theme::kind`; blank line between rows. Expanded: line 1 becomes up to **5** wrapped lines of the message with the age on the first.
- Line 2 at narrow widths: kinds always whole; channel truncates first, then author (Review Focus 5).
- Scroll keeps the selected row fully visible.
- Body states: `Loading` → `Loading Slack…`; `Empty` → `Nothing needs you right now.` / `New activity on cleared threads will bring them back.`; `AuthError(msg)` → msg in `theme::hot_row()`-style banner (copy pr-pulse's `hot_row`).
- Status line: `app.status`, else `⚠ {cache warning count} source(s) failed · data from {age}` when warnings exist; position `3/12` right-aligned.
- Key hints: `j/k move  ␣ expand  o open  O open+clear  x clear  u undo  r refresh  s sort  g/G top/bottom  q quit`.

- [ ] **Step 1: Failing snapshot tests** with `TestBackend` and `sample_cache` at a fixed `now`: `wide_120`, `medium_80`, `narrow_40`, `expanded_80`, `empty_80`, `loading_80`, `auth_error_80`, `warning_80`; plus `line2_keeps_types` (40 cols, a row with 3 kinds and a 30-char author: buffer line contains `[watch]`, `[mention]`, `[thread]`) and `selected_has_marker_no_background`.
- [ ] **Step 2: Run** `cargo test ui` → FAIL; then implement; `cargo insta review` and accept only after reading each snapshot against §9 and the canvas.
- [ ] **Step 3: Run** `cargo test` → PASS.
- [ ] **Step 4: Commit** `feat: draw the inbox`.

---

### Task 10: Wiring: live loop, actions, opening, fixtures mode

**Files:**
- Create: `src/open.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:

```rust
// open.rs
pub fn app_url(me: &Me, row: &Row) -> String;
// "slack://channel?team={team}&id={channel}&message={shown.ts}" + "&thread_ts={root}" when shown is a reply (thread_ts != ts)
pub fn launch(url: &str) -> Result<(), String>;   // `open` (macOS) / `xdg-open`, detached, stdio null, child reaped on a thread (pr-pulse's open_in_browser)
```

  `main.rs`: `Live { requests, events, poller, state: StateDir, slack_ui: HttpSlack (unpaced), next_clears_reload }` — `tick` sends due requests, applies `FetchEvent`s (`Started` → `set_loading(true)`, `Done(c)` → `set_cache(c)`, `set_loading(false)`, `poller.on_done`), and every 2 s reloads `clears.json` into the app. `run_action`:
  - `Open`: url = `app_url` (`open_mode: app`) or `chat.getPermalink(channel, shown.ts)` → `permalink` (`web`); `launch`; then if `clear` is Some, write it as for `Clear`.
  - `Clear`: if `unwatch`, `reactions.remove` for each configured watch emoji Jay has on `watched` (ignore `no_reaction`); on failure → `app.applied(.., Err)` and stop. Then `StateDir::update(CLEARS, …)` inserting the clear; `app.applied(.., Ok)`.
  - `Undo`: `reactions.add` (first watch emoji) if `rewatch`; then restore `prev` in `clears.json`.
  - Fixtures mode (`None` live): actions report `fixtures: would …` and clears stay in memory only; `App` gets `sample_cache(Utc::now())`.
  - On start (TUI, not fixtures): `token::read()` failure is not fatal: show `Body::AuthError` and keep running (so `q` works).

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn app_url_for_root_and_reply()
#[test] fn watch_clear_fails_without_clearing()  // run_action with a FakeSlack answering reactions.remove Err → clears.json unchanged, status shows the error
#[test] fn clear_writes_clears_json()
#[test] fn undo_readds_reaction_and_restores()
#[test] fn fixtures_mode_loads_sample()
```

(`run_action` takes `&dyn Slack` and `&StateDir` so tests inject `FakeSlack` and a temp dir.)

- [ ] **Step 2: Run** `cargo test` → FAIL; implement; → PASS.
- [ ] **Step 3: Manual check:** `cargo run -- --fixtures` in an 80-column terminal: every key in §9 behaves as listed; `q` quits and restores the terminal.
- [ ] **Step 4: Commit** `feat: live loop, actions and fixtures mode`.

---

### Task 11: verify, README, live check

**Files:**
- Create: `src/verify.rs`
- Modify: `src/main.rs`, `README.md`

**Interfaces:**
- Produces: `verify::run(s: &dyn Slack, cfg: &Config, cache: &Cache, clears: &Clears, now) -> Result<String, SlackError>` returning the printed report.

  For each search source (each mention query, `to:me`, `from:me`): run `sources::search` raw; classify every match as `row` (its thread/DM key is in `cache.rows`), `mine` (by Jay), `answered` (Jay posted after it in its thread, per a `replies` read), `cleared` (≤ a clear's `wm`), `ignored`, or **`unexplained`**. Print one line per source `"<query>: <total> matches · <row> in list · <mine> mine · <answered> answered · <cleared> cleared · <ignored> ignored · <unexplained> unexplained"`, then each unexplained match as `channel-label ts` (never message text). Plus `watches: <n> in list / <m> watched`. Exit code 1 when anything is unexplained.

- [ ] **Step 1: Failing test** `verify_counts` with FakeSlack: 4 mention matches (1 in rows, 1 by me, 1 cleared, 1 unexplained) → the report line equals `"<@U0ME> after:…: 4 matches · 1 in list · 1 mine · 0 answered · 1 cleared · 0 ignored · 1 unexplained"`.
- [ ] **Step 2: Run** → FAIL; implement; wire `Cmd::Verify` in `main` (reads token, config, state dir; prints report; exit 1 on unexplained or error) → PASS.
- [ ] **Step 3: README** with pr-pulse's sections: install (Rust 1.89+, Slack app setup from the spec's §4 scope list and the manifest Jay used, keychain command), use (workspace-manager snippet from §3), keys table (§9), config table (§10), how it works (§6–§7 in brief: sources, shared cache, never-miss rules, what isn't covered), development (`cargo test`, `cargo run -- --fixtures`, `cargo insta review`, `cargo test live_ -- --ignored`).
- [ ] **Step 4: Live check with Jay** (milestone 4 of the spec; needs Jay at the keyboard):
  - `cargo run --release` in a herdr pane; first refresh completes; note its duration and that no `RateLimited` warning appears with the full group DM sweep.
  - `cargo run -- verify` → zero unexplained, or each one explained and fixed.
  - `o` on a thread reply row: does Slack open **at the message**? If only the channel opens, set the config default `open_mode` to `web` and record the finding in spec §4.
  - Add `:eyes:` to a message → it appears as `[watch]` on the next refresh; `x` removes the reaction in Slack; `u` restores it.
- [ ] **Step 5: Commit** `feat: verify command and README`.
