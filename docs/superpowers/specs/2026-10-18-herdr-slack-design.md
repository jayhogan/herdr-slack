# herdr-slack — design

Date: 2026-10-18 · Status: draft for approval

Design canvas (layout reference): https://claude.ai/artifact/JDnfTp2jW2YyajM7z1DaFr
Boards: "Inbox pane (interactive)" and "Row anatomy & keymap". The canvas predates three changes in
this doc: `todo` is now `watch` (§5), there is a new `dm` type (§5), and the app runs in a pane, not
an overlay (§3). Where the canvas and this doc disagree, this doc wins.

Prior art: a TypeScript version exists (`~/Desktop/herdr-slack-ts.zip`). Its behavior is the
starting point and is referenced below as "the TS version"; its code is not reused. Reasons:
Jay wants one stack (Rust) for all his herdr plugins.

## 1. Why

Jay misses Slack activity that needs him: asks buried in threads, mentions in busy channels, DMs,
things he meant to come back to. He spends his day in herdr, so the list of what's waiting on him
should live there, next to his agents, instead of in Slack's own unread badges, which mix "needs
me" with "happened".

herdr-slack is a TUI that runs in a herdr pane and lists the Slack conversations waiting on Jay,
one row per thread, oldest waiting first, and lets him open or clear each from the keyboard.

## 2. Goals, non-goals, anti-goals

Goals
- Every item in §5 that is waiting on Jay appears in the list.
- Exactly one row per thread, tagged with every type that applies.
- Fast keyboard triage: move, expand, open in Slack, clear, undo.
- Opens instantly from cache; never blocks the UI on the network.

Non-goals (v1)
- Threads Jay follows without having posted in them.
- Slack's own "Later" / saved list (no API; see §4). Watches (`:eyes:`) replace it.
- Replying, reacting (other than removing/re-adding `:eyes:`), or marking read in Slack.
- Notifications outside the pane.
- Multiple Slack workspaces.

Anti-goals (a failure even if everything else works)
- **Missing a message.** This is the one Jay called out: other problems he can work around, a
  silent gap he can't. It drives the rules in §7.
- Leaking the token or message text: token on disk or in a process argument list, or a
  world-readable cache.
- Burning Slack rate limits so badly that refreshes stall for minutes.

## 3. Constraints and template

Match `https://github.com/jayhogan/herdr-pr-pulse` (Jay's existing plugin; read its spec at
`docs/superpowers/specs/2026-10-17-pr-pulse-design.md` and its source):
- Rust 2024 edition, `rust-version = "1.89"`; ratatui 0.30; crossterm 0.29; serde/serde_json;
  chrono (`default-features = false`, `std`, `clock`, `serde`). No async runtime. Dev-deps:
  `tempfile`, `insta`.
- Flat `src/*.rs` modules with `//!` docs; errors are `Result<T, String>` with readable messages;
  subprocesses via `std::process::Command`.
- Background `std::thread` + `std::sync::mpsc` for fetches; UI loop on `crossterm::event::poll`
  with a tick.
- `herdr-plugin.toml` with `[[build]] cargo build --release`; `scripts/install.sh` symlinks the
  binary into `~/.local/bin`; installed with `herdr plugin link`.
- State in `~/.local/state/herdr-slack/` as JSON files; one shared cache guarded by a lock file.
  Copy pr-pulse's **code** (`src/store.rs`: advisory `File::lock`/`try_lock`, no stale timeout),
  not the `create_new`/timeout wording in its spec: a Slack refresh can take minutes waiting out
  429s, and a timeout would let a second copy start a parallel refresh.
- `--fixtures` mode: no network, sample data, throwaway state dir.
- `cargo test` with fixtures in `tests/fixtures/`, `insta` snapshots for the UI,
  `cargo clippy --all-targets` clean; live tests behind `#[ignore]` named `live_*`.
- README sections as in pr-pulse: install, use (workspace-manager layout snippet), how it works,
  development.

Additions to the pr-pulse stack (Jay's call: "keep 1 stack"; the specific crate is our call):
- One blocking HTTP client for the Slack Web API (e.g. `ureq` with rustls). Don't shell out to
  `curl`: the token would sit in a process argument list.

How it runs (Jay's decision): **in an existing pane, not an overlay.** Jay runs `herdr-slack` in a
pane or lists it in a herdr-plugin-workspace-manager layout, like `herdr-pr-pulse mine`:

```yaml
panes:
  - command: herdr-slack
    persist: false
```

No herdr actions, keybinds or overlay panes. `q` quits. The app doesn't need herdr's API; it
should run in any terminal (useful for development). It may run in several panes at once (§6).

## 4. Research findings

Verified live against Heard's workspace on 2026-10-18 with Jay's user token (user `U06Q9KM53SL`,
team `TFW7Z6L8Y`):

- **Mentions:** `search.messages` with `<@U06Q9KM53SL> after:<YYYY-MM-DD>` works: 27 matches in 7
  days, all genuine. 10 of the 27 have the mention only in `blocks` or `attachments`, not in
  `text`, and 8 of those have **empty `text`** (workflow/alert-style posts). Two matches were
  Jay's own posts. So: filter `user == me`, and render text from blocks/attachments when `text`
  is empty (§8).
- **Own posts:** `from:<@U06Q9KM53SL>` and `from:me` return the same results: 99 in 7 days (64
  thread replies, 10 thread roots, 25 top-level). More than one page at `count=100`, so paging
  is required.
- **Thread detection:** a search match's `permalink` carries `?thread_ts=` for both roots and
  replies; `thread_ts == ts` means a root. No `thread_ts` means a top-level message.
- **1:1 DMs:** `to:me after:<date>` (same as `to:<@ID>`) returns only 1:1 DM messages to Jay: 61
  in 7 days. `is:dm` returns both directions.
- **Group DMs cannot be searched.** `is:mpim` / `is:mpdm` are treated as free text. Jay is in 160
  group DMs (`conversations.list types=mpim`) and 66 IMs. A conversation's `updated` field does
  not track new messages. Reading `conversations.history` (`limit=1`, `oldest=<7 days ago>`) on
  all 160 succeeded with no 429; none had a message in the last 7 days.
- **1:1 DMs from bots/apps are found by `to:me`:** an app DM with 12 messages this week had 12
  search hits. Across the 5 IMs with activity, search hits ≥ history counts (search also counts
  threaded replies).
- **Watches:** `reactions.list` (user token) returns items Jay reacted to, **newest reaction
  first**, with no reaction timestamp. **Cursor paging is broken**: the returned
  `next_cursor` gives `invalid_cursor` (GET and POST), and `page=` is ignored. But `limit=1000`
  works in one call: 1000 reactions reaching back to March 2026. So one call per refresh covers
  Jay's last 1000 reactions.
- **Jay already has `:eyes:` on 114 messages** within those 1000 reactions (20 in the newest 100).
- **Slack "Later" list:** no public API (Slack documents this); `stars.list` doesn't reflect new
  saves. Hence watches.
- **Slack app:** created by Jay (he is an admin). User-token scopes only: `search:read`,
  `channels:history`, `groups:history`, `im:history`, `mpim:history`, `channels:read`,
  `groups:read`, `im:read`, `mpim:read`, `users:read`, `reactions:read`, `reactions:write`. Token
  is in the macOS keychain: `security find-generic-password -s herdr-slack -w`.

Not yet verified (milestone check, §11):
- `slack://channel?team=T…&id=C…&message=<ts>` (plus `&thread_ts=` for replies) opening the
  Slack app at the message. Not officially documented. Fallback: `chat.getPermalink` and `open`
  the https permalink.

## 5. What goes in the list

Every row is one **thread**, keyed by `channel + root ts`. A top-level message with no replies is
its own root. A thread that qualifies several ways is **one row**, tagged with every type that
applies (Jay: "be sure there is only ever 1 row per thread").

| Type | Shows when | Goes away when |
|---|---|---|
| `watch` | Jay has an `:eyes:` reaction on a message (root or reply). Row is that message's thread. | The reaction is removed, in Slack or by `x`. |
| `mention` | Someone else's message mentions `@Jay` (plus configured extra queries, e.g. a user group). | Jay posts in that thread after the mention, or clears it. |
| `reply` | New messages from others in a thread **Jay started**, after Jay's last post in it. | Jay posts again, or clears it. |
| `thread` | New messages from others in a thread Jay **posted in but didn't start**, after his last post. | Same as `reply`. |
| `channel` | New top-level posts from others in a configured channel. | Cleared. |
| `dm` | Messages from others in a 1:1 DM or group DM, after Jay's last message there. | Jay posts there again, or clears it. |

Details:
- **Watch emoji** is `eyes` by default, configurable (a list). Jay renamed this type from "todo" to
  "watch".

- **Only `:eyes:` added from now on count** (Jay's decision; he already has `:eyes:` on 114
  messages since March). The first successful watch read records every message that already
  carries his watch emoji as a **baseline** in state; baseline messages never become watch rows.
  A baseline entry is dropped once a successful read no longer shows the reaction, so removing
  and later re-adding `:eyes:` makes it a real watch. If the baseline can't be recorded (first
  read fails), no watch rows are created until it is.
- **DM rows** (our call, so a busy DM doesn't flood the list): top-level messages in a DM since
  Jay's last top-level message there form **one row** keyed `channel + "dm"`. Rules that keep it
  one row per thread:
  - A top-level DM message that **has replies or is watched** belongs to its own thread row
    (`channel + ts`, typed as usual plus `dm`) and is removed from the DM row. Nothing is in
    both.
  - Jay's "last message" for the DM row counts only his top-level messages; for a thread row,
    only his messages in that thread.
  - Watermarks are per row. When a message moves from the DM row to a thread row, the DM row's
    watermark carries over to it, so a cleared message doesn't come back unless there's newer
    activity.
  - A mention in a top-level DM message is just part of the DM row (tag `dm` and `mention`).
- **Bot and workflow posts count** like people's posts. Config `ignore_users` (user/bot ids,
  default empty) drops a noisy sender everywhere (our call). Skip channel-join/leave/topic and similar
  system subtypes (the TS version's `SKIP_SUBTYPES` list is a good start).
- **Curated channels** use our own per-channel cursor, never Slack's `last_read`.
- Messages by Jay never create a row.

## 6. Data fetching

One refresh runs in a background thread and writes `~/.local/state/herdr-slack/cache.json`
(mode `0600`). Like pr-pulse, every running copy shares it: a copy fetches only when the cache is
older than the refresh interval (default 180 s), guarded by a lock file; `r` forces a fetch.
Copies re-read the cache on a short tick so all panes agree.

Sources per refresh:
1. **Watches:** one `reactions.list` call with `limit=1000` every refresh (cursor paging is
   broken, §4). It's the source of truth for watches. Watches **never age out** (not subject to
   retention): a watch row stays until the reaction is gone. A watch already in the cache that
   falls past the 1000 newest reactions is kept and re-checked directly (e.g.
   `reactions.get` on that message, on a slower cycle) rather than dropped.
2. **Mentions:** `search.messages` for `<@me> after:<date>` and each extra query, all pages.
3. **1:1 DMs:** `search.messages` for `to:me after:<date>`, all pages.
4. **Own posts:** `search.messages` for `from:me after:<date>`, all pages, to learn which
   threads and DMs Jay is in.
5. **Threads:** `conversations.replies` for each thread found by 1, 2, 4 (and 3 if threaded), to
   find Jay's last post, the root author, and newer messages. Found threads go into a durable
   **known-threads** list in state, and stay tracked until **14 days with no activity** in the
   thread (our call, from the never-miss rule: a reply 10 days after Jay's last post must
   still show). This adds to the `conversations.replies` budget; the builder decides how to
   spread the re-reads (e.g. threads quiet for days are read less often).
6. **Curated channels:** `conversations.history` since the channel's cursor.
7. **Group DMs:** `conversations.history` on each group DM (`users.conversations types=mpim`,
   refreshed occasionally). Spread across refreshes so **every group DM is checked at least every
   ~10 minutes**; the exact schedule is the builder's call.
8. Names: `users.info` / `conversations.info` for ids not yet cached (names cached on disk).

Lookback windows (defaults, configurable): mentions, DMs and own posts 7 days; channels 48 h.
Retention 14 days.

Rate limits: `search.messages` is Tier 2 (~20/min), `conversations.*` Tier 3 (~50/min). A refresh
must stay within these with headroom (Jay's other tools use the same workspace). Honor
`Retry-After` on 429 and resume; spread calls rather than burst. If a full refresh can't fit in
the interval, it's fine for a refresh to take longer; it's not fine to skip sources (§7).
Reducing `conversations.replies` calls (e.g. skipping threads whose latest reply ts hasn't
changed) is the builder's call.

## 7. Never miss a message (Jay's anti-goal, our rules)

- **No silent caps.** Every search and list is paged to the end of its window. If any hard limit
  stops paging, show it in the header (e.g. `⚠ mentions truncated`).
- **A failed source never drops rows.** If any source fails (429 after retries, network, auth,
  a channel we can't read), keep the previous rows from that source and show the error and data
  age in the status line. A row only disappears when a **successful** read shows it's resolved,
  or when Jay clears it.
- **Aging out isn't resolving.** A row whose messages fall outside the lookback window stays
  until cleared or until retention (14 d).
- **Overlap.** Search windows use dates (`after:` is day-granular), and per-channel/per-DM
  cursors are only advanced after a successful read, so a slow or failed refresh can't leave a
  gap. Search indexing lag is covered by the same overlap.
- **Rich-format messages count** (§4, §8).
- **`herdr-slack verify`** (CLI subcommand, no TUI): runs each search raw and prints, per source,
  Slack's match count vs how many of those matches map to a row or to a known-resolved reason
  (Jay's own post, cleared, answered). Lets Jay spot gaps in the first weeks.

## 8. Message text

Line 1 must be readable:
- Resolve `<@U…>` → `@name`, `<#C…|name>` / `<#C…>` → `#name`, `<!subteam^S…|@name>` → `@name`,
  `<!here>`/`<!channel>`, `<url|label>` → `label`, bare `<url>` → url; unescape `&amp; &lt; &gt;`.
- When `text` is empty, build it from `blocks` (rich_text, section, header, context text) and then
  `attachments` (`fallback`/`title`/`text`).
- Collapse whitespace/newlines to single spaces for line 1; keep line breaks when expanded.
- Leave `:emoji:` shortcodes as text.
- Width must be measured in terminal cells (wide chars, emoji), not bytes or chars.

## 9. The TUI

Layout per the canvas board "Inbox pane (interactive)".

- **Header:** `Slack inbox`, count (`6 need attention · 2 watch · 3 mention …`), sort, `synced 2m
  ago`, and any truncation warning (§7).
- **Rows** — each item is 2 lines, with a blank line between items (canvas):
  - Line 1: message text truncated to one line, right-aligned age of the **newest** message in
    the row (`2 min ago`, `3 hours ago`, `3 days ago`).
  - Line 2: author, channel (`#name`, or the DM partner / group DM members), and every type in
    brackets: `[watch] [mention] [thread]`, each type its own color (canvas palette; not color
    alone: the label carries the meaning).
  - The message shown is the newest unhandled one in the row; the author is its author.
  - Selected row: a `▌` marker and a highlighted background.
- **Sort:** oldest waiting first (by the row's oldest unhandled message), default. `s` toggles
  newest activity first.
- **Status line:** last action / error toast and `3/12` position.
- **Key hints footer:** as in the canvas.
- **Empty state:** "Nothing needs you right now." (canvas). Loading and error states follow
  pr-pulse's patterns (error banner for auth failure: "Slack token missing or invalid" with the
  keychain command).
- **Narrow panes:** must work down to ~40 columns: line 1 text shrinks first, line 2 truncates
  channel then author, types always visible.

Keys (Jay's list plus the TS version's additions, which Jay accepted: "everything else as noted"):

| Key | Action |
|---|---|
| `j`/`k`, `↓`/`↑` | Move |
| `space` | Expand/collapse the selected row to up to 5 lines of message |
| `O` | Open in Slack **and clear** (watches stay: see below) |
| `o` | Open in Slack without clearing |
| `x` | Clear. On a watch row, also removes Jay's `:eyes:` reaction in Slack |
| `u` | Undo the last clear (re-adds the reaction if `x` removed one); multi-level |
| `r` | Refresh now |
| `s` | Toggle sort |
| `g`/`G` | Top / bottom |
| `q` | Quit |

- **Clearing** is local: store a per-row watermark (the newest message ts in the row) in
  `~/.local/state/herdr-slack/`. Anything newer brings the row back. Clearing a multi-type row
  clears all of its types.
- **`O` on a watch row** opens it and clears the other types but keeps the watch (our call: opening
  isn't the same as dealing with it; Jay said a watch goes away when the emoji is removed or `x`
  is pressed). Config `open_clears_watch` (default false) flips it.
- If `reactions.remove` / `reactions.add` fails, don't clear/restore; show the error.
- Opening uses `open <url>` (macOS) / `xdg-open` (Linux); see §4 for the URL.

## 10. Config and secrets

- **Token:** read at startup by running `security find-generic-password -s herdr-slack -w`
  (macOS). Fallback env var `HERDR_SLACK_TOKEN`. Never written to disk, never logged.
- **Config:** `~/.config/herdr-slack/config.json` (JSON to avoid a new dep), optional; defaults
  apply when missing:

```json
{
  "channels": ["#incidents"],
  "watch_emoji": ["eyes"],
  "extra_mention_queries": [],
  "lookback_days": 7,
  "channel_lookback_hours": 48,
  "refresh_secs": 180,
  "retention_days": 14,
  "open_mode": "app",
  "open_clears_watch": false
}
```

- **Data on disk** may include customer data: cache and state files `0600`, state dir `0700`;
  prune items and watermarks past retention.

## 11. Milestones (suggested; the builder may reshape)

1. Skeleton: manifest, CLI args, `--fixtures` TUI with the full keymap on sample data, snapshots.
2. Slack client, token, collection of all sources into rows (fixture-driven tests of the §5
   rules, especially one-row-per-thread and the §7 failure rules).
3. Shared cache, lock, background refresh, clear/undo, reaction write-back.
4. Live check with Jay: `verify`, deep-link behavior (decide `open_mode` default), rate-limit
   behavior with the full group DM sweep.

## 12. Left to the builder

Module layout; HTTP crate choice; exact refresh scheduling and group-DM sweep spreading; how
thread reads are minimized; colors (within the canvas palette); exact header/status wording;
fixture design; how the cache schema is versioned.
