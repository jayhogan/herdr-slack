# herdr-slack

A [herdr](https://herdr.dev) pane that lists the Slack threads waiting on you, one row per
thread, oldest waiting first. Rows come from mentions, replies in threads you are in, curated
channels, DMs and group DMs, and messages you mark with `:eyes:`. Open or clear each from the
keyboard. The list opens instantly from a shared cache; the network never blocks the UI.

Each row has two lines: the newest unhandled message and its age, then the author, the channel
(or DM partner) and every type that applies: `[watch] [mention] [reply] [thread] [channel] [dm]`.

## Install

Requires a Rust toolchain (1.89+) and a Slack user token (below).

```sh
git clone <this repo> ~/dev/herdr-slack
herdr plugin link ~/dev/herdr-slack   # builds, registers the plugin
~/dev/herdr-slack/scripts/install.sh  # puts the binary on PATH
```

### Slack app

Create a Slack app from a manifest (api.slack.com/apps, "Create New App", "From a manifest") with
user-token scopes only:

```json
{
  "display_information": {
    "name": "herdr Slack inbox",
    "description": "Personal inbox of Slack mentions, threads and watched messages for herdr."
  },
  "oauth_config": {
    "scopes": {
      "user": [
        "search:read",
        "channels:history", "groups:history", "im:history", "mpim:history",
        "channels:read", "groups:read", "im:read", "mpim:read",
        "users:read",
        "reactions:read", "reactions:write"
      ]
    }
  },
  "settings": {
    "org_deploy_enabled": false,
    "socket_mode_enabled": false,
    "token_rotation_enabled": false
  }
}
```

Install the app to your workspace and copy the **User OAuth Token** (`xoxp-…`). Store it in the
macOS keychain:

```sh
security add-generic-password -s herdr-slack -a "$USER" -w
```

The token is read from the keychain (`security find-generic-password -s herdr-slack -w`), else
from the `HERDR_SLACK_TOKEN` environment variable. It is never written to disk, logged, or passed
as a process argument.

## Use

Run `herdr-slack` in any pane or terminal. With herdr-plugin-workspace-manager:

```yaml
panes:
  - command: herdr-slack
    persist: false
```

Several panes can run at once; they share one fetch.

### Keys

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | Move |
| `g` / `G` | Top / bottom |
| `space` | Expand or collapse the selected row (up to 5 lines of message) |
| `o` | Open in Slack, without clearing |
| `O` | Open in Slack and clear. A watch row stays unless `open_clears_watch` is true |
| `x` | Clear. On a watch row, also removes your `:eyes:` reaction in Slack |
| `u` | Undo the last clear (re-adds the reaction if `x` removed one); multi-level |
| `r` | Refresh now |
| `s` | Toggle sort: oldest waiting first / newest activity first |
| `q`, `Esc`, `Ctrl-C` | Quit |

Clearing is local: it stores the newest message ts in the row, and anything newer brings the row
back. If removing or re-adding the reaction fails, nothing is cleared or restored and the error
shows in the status line.

### Config

Optional `${XDG_CONFIG_HOME:-~/.config}/herdr-slack/config.json`. Every field is optional; a
missing file means defaults.

| Field | Default | Meaning |
|---|---|---|
| `channels` | `[]` | Curated channels whose new top-level posts show up, as `#name` or an id |
| `watch_emoji` | `["eyes"]` | Reactions that mark a message as a watch |
| `extra_mention_queries` | `[]` | Extra searches counted as mentions, e.g. a user group |
| `lookback_days` | `7` | Window for mentions, DMs and your own posts |
| `channel_lookback_hours` | `48` | Window for curated channels |
| `refresh_secs` | `180` | How old the shared cache may get before a copy fetches |
| `retention_days` | `14` | Cleared rows and watermarks are pruned after this |
| `open_mode` | `"app"` | `"app"` opens `slack://` in the desktop app; `"web"` opens the https permalink |
| `open_clears_watch` | `false` | Whether `O` also clears a watch |
| `ignore_users` | `[]` | User or bot ids dropped everywhere |

### Watches

Add `:eyes:` to a message and its thread shows as `[watch]` on the next refresh. Only reactions
added after the first run count: the first successful read records every message that already
carries your watch emoji as a baseline, and baseline messages never become rows. Removing the
reaction and adding it again later makes it a real watch. Watches never age out; a watch row stays
until the reaction is gone.

## How it works

- Each refresh reads, in this order: your reactions (one `reactions.list`, limit 1000), a
  `search.messages` for `<@you>` and each extra mention query, `to:me` (1:1 DMs) and `from:me`
  (your own posts), the threads those find, the curated channels, and the group DMs. Search
  can't see group DMs, so a slice of them is read every refresh and each one at least every ~10
  minutes. Threads stay tracked until 14 days with no activity; quiet ones are re-read less often.
- Every running copy shares one fetch. Results go to
  `${XDG_STATE_HOME:-~/.local/state}/herdr-slack/cache.json` (dir `0700`, files `0600`), alongside
  `state.json` (worker bookkeeping, watch baseline) and `clears.json` (your clears). A copy
  fetches only when the cache is older than `refresh_secs`, guarded by a lock file; `r` forces
  one. Calls are paced for Slack's rate tiers and honor `Retry-After`.
- Never-miss rules: every search is paged to the end of its window, and a truncation shows in the
  header. A failed source keeps the rows it supplied last time and shows the error. A row leaves
  only when a successful read shows it resolved, or when you clear it. Rows that age out of the
  lookback window stay until cleared or past retention. Search windows overlap by a day, and
  per-channel cursors only advance after a successful read.
- A DM becomes one row keyed by the channel; a DM message with replies or a watch gets its own
  thread row and leaves the DM row.
- Not covered: threads you follow without having posted in them, Slack's own "Later" list (no API;
  watches replace it), and more than one workspace. Messages you wrote never create a row.

### Checking for gaps

`herdr-slack verify` runs each search raw (each mention query, `to:me`, `from:me`) and prints,
per source, Slack's match count against how many matches the list explains:

```
<@U0ME> after:2026-10-10: 27 matches · 20 in list · 2 mine · 4 answered · 1 cleared · 0 ignored · 0 unexplained
watches: 3 watched · 3 in list · 0 cleared · 0 unexplained
```

The `watches` line checks the watches the app holds in `state.json` (baseline messages are
already left out) against the list: how many are in the list, how many were cleared, and how many
are neither. Each unexplained match or watch is listed as its channel and ts, never its text. The
exit code is 1 when anything is unexplained or on error. It uses the cache and state from your
last refresh, so run it after the app has refreshed.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo run -- --fixtures           # no network, sample data
cargo insta review                # after a UI snapshot change
cargo test live_ -- --ignored     # hits Slack with your token
```
