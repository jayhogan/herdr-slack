//! Invented sample data for `--fixtures` and tests (spec §9, canvas "Inbox pane").

use chrono::{DateTime, Duration, Utc};

use crate::model::{Kind, Msg, Names, Row, RowKey, Ts};
use crate::state::{Cache, Me};

struct Sample {
    waiting: i64, // minutes since the oldest unhandled message
    recent: i64,  // minutes since the newest message
    user: &'static str,
    channel: &'static str,
    kinds: &'static [Kind],
    pending: usize,
    body: &'static str,
}

const SAMPLES: [Sample; 9] = [
    Sample {
        waiting: 4320,
        recent: 4320,
        user: "U0DANA",
        channel: "C0EPD",
        kinds: &[Kind::Watch],
        pending: 1,
        body: "Review the Q4 audit-log retention proposal before Thursday’s EPD sync. We need a yes/no on 90 vs 365 days for actor + diff payloads, and whether the 365-day tier moves to cold storage after 30. Doc is linked in the thread, comments welcome inline.",
    },
    Sample {
        waiting: 2880,
        recent: 4,
        user: "U0MARCUS",
        channel: "C0CORE",
        kinds: &[Kind::Watch, Kind::Mention, Kind::Thread],
        pending: 2,
        body: "<@U0ME> can you sanity check the migration plan for the ledger backfill? I’m worried about holding the lock on transactions during the morning peak. Thinking we chunk by account and run it behind a flag so we can pause it if p95 climbs.",
    },
    Sample {
        waiting: 1440,
        recent: 1440,
        user: "U0PRIYA",
        channel: "C0CI",
        kinds: &[Kind::Mention],
        pending: 1,
        body: "<!subteam^S0CORE|@core> anyone own the flaky checkout e2e? It’s failed 3 of the last 5 runs on main, always on the payment-method step. Happy to quarantine it but don’t want to hide a real regression.",
    },
    Sample {
        waiting: 360,
        recent: 360,
        user: "U0THEO",
        channel: "C0CORE",
        kinds: &[Kind::Reply],
        pending: 1,
        body: "Re your RFC: I’d keep the poller separate from the TUI so the overlay opens instantly from cache. One question, do watermarks live per thread or per channel? Per thread seems right if we want cleared rows to resurface.",
    },
    Sample {
        waiting: 180,
        recent: 180,
        user: "U0LENA",
        channel: "C0INC",
        kinds: &[Kind::Channel],
        pending: 1,
        body: "SEV3 resolved: delayed bank sync for a subset of accounts between 9:10 and 10:40 ET. Root cause was a stuck worker after the deploy. Postmortem doc to follow tomorrow.",
    },
    Sample {
        waiting: 52,
        recent: 52,
        user: "U0SAM",
        channel: "C0BOOKS",
        kinds: &[Kind::Thread],
        pending: 1,
        body: "Pushed a fix for the date picker timezone bug, mind taking another look when you get a sec? It now normalizes to the practice’s timezone before comparing period boundaries.",
    },
    Sample {
        waiting: 40,
        recent: 18,
        user: "U0AVA",
        channel: "C0DOCKET",
        kinds: &[Kind::Mention, Kind::Reply],
        pending: 2,
        body: "<@U0ME> thanks, merged! One follow-up: should the Docket queue show resolved threads too, or only open ones? I lean toward a toggle that defaults to open.",
    },
    Sample {
        waiting: 2,
        recent: 2,
        user: "U0NORA",
        channel: "C0CORE",
        kinds: &[Kind::Channel],
        pending: 1,
        body: "Reminder: deploy freeze starts Friday 5pm ET and runs through Monday 9am. Get anything risky merged by Thursday EOD.",
    },
    Sample {
        waiting: 25,
        recent: 10,
        user: "U0DANA",
        channel: "D0DANA",
        kinds: &[Kind::Dm],
        pending: 2,
        body: "Quick one: are you joining the vendor call at 2? If not I’ll take notes and send them over.",
    },
];

fn ago(now: DateTime<Utc>, mins: i64) -> Ts {
    Ts::from_unix((now - Duration::minutes(mins)).timestamp())
}

/// Nine rows (the canvas's eight, `todo` renamed `watch`, plus a DM), relative to `now`.
pub fn sample_cache(now: DateTime<Utc>) -> Cache {
    let rows = SAMPLES
        .iter()
        .map(|s| {
            let oldest = ago(now, s.waiting);
            let newest = ago(now, s.recent);
            let is_dm = s.kinds.contains(&Kind::Dm);
            let key = if is_dm {
                RowKey::dm(s.channel)
            } else {
                RowKey::thread(s.channel, &oldest)
            };
            Row {
                key,
                channel: s.channel.into(),
                kinds: s.kinds.to_vec(),
                shown: Msg {
                    channel: s.channel.into(),
                    ts: newest.clone(),
                    thread_ts: (!is_dm && oldest != newest).then(|| oldest.clone()),
                    reply_count: 0,
                    user: s.user.into(),
                    subtype: None,
                    body: s.body.into(),
                },
                watched: s.kinds.contains(&Kind::Watch).then(|| oldest.clone()),
                oldest,
                newest,
                pending: s.pending,
            }
        })
        .collect();
    let pairs = |p: &[(&str, &str)]| {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    Cache {
        version: 1,
        fetched_at: Some(now - Duration::minutes(2)),
        me: Some(Me {
            user_id: "U0ME".into(),
            team_id: "T0HEARD".into(),
        }),
        rows,
        names: Names {
            users: pairs(&[
                ("U0ME", "Jay"),
                ("U0DANA", "Dana Okafor"),
                ("U0MARCUS", "Marcus Lee"),
                ("U0PRIYA", "Priya Shah"),
                ("U0THEO", "Theo Marsh"),
                ("U0LENA", "Lena Ruiz"),
                ("U0SAM", "Sam Patel"),
                ("U0AVA", "Ava Chen"),
                ("U0NORA", "Nora Kim"),
            ]),
            channels: pairs(&[
                ("C0EPD", "#epd"),
                ("C0CORE", "#core-eng"),
                ("C0CI", "#ci-alerts"),
                ("C0INC", "#incidents"),
                ("C0BOOKS", "#books-pod"),
                ("C0DOCKET", "#docket"),
                ("D0DANA", "@Dana Okafor"),
            ]),
        },
        ..Cache::default()
    }
}
