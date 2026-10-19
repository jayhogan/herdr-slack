//! Opening a row in Slack (spec §4 "Not yet verified" for the URL, §9 "Opening").

use std::process::Stdio;

use crate::model::{Row, Ts};
use crate::slack::{Slack, SlackError};
use crate::state::Me;

/// `slack://channel?team=…&id=…&message=<shown ts>`, plus `&thread_ts=<root>` for a reply.
pub fn app_url(me: &Me, row: &Row) -> String {
    let shown = &row.shown;
    let mut url = format!(
        "slack://channel?team={}&id={}&message={}",
        me.team_id, row.channel, shown.ts.0
    );
    let root = shown.root();
    if root != shown.ts {
        url.push_str(&format!("&thread_ts={}", root.0));
    }
    url
}

/// The https permalink of one message (`open_mode: web`).
pub fn permalink(s: &dyn Slack, channel: &str, ts: &Ts) -> Result<String, SlackError> {
    let body = s.call(
        "chat.getPermalink",
        &[
            ("channel", channel.to_owned()),
            ("message_ts", ts.0.clone()),
        ],
    )?;
    body["permalink"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SlackError::Api("chat.getPermalink: no permalink".into()))
}

/// `open` (macOS) / `xdg-open`, not waited on by the UI: a detached thread reaps the child.
pub fn launch(url: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = crate::token::command(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run {opener}: {e}"))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::sample_cache;
    use crate::slack::fake::FakeSlack;
    use chrono::Utc;

    fn me() -> Me {
        Me {
            user_id: "U0ME".into(),
            team_id: "T0HEARD".into(),
        }
    }

    #[test]
    fn app_url_for_root_and_reply() {
        let mut row = sample_cache(Utc::now()).rows.remove(0);
        row.channel = "C0CORE".into();
        row.shown.ts = Ts("1700000100.000200".into());
        row.shown.thread_ts = None;
        let root = "slack://channel?team=T0HEARD&id=C0CORE&message=1700000100.000200";
        assert_eq!(app_url(&me(), &row), root);
        // A root with replies carries thread_ts == ts: still a root.
        row.shown.thread_ts = Some(row.shown.ts.clone());
        assert_eq!(app_url(&me(), &row), root);
        row.shown.thread_ts = Some(Ts("1700000000.000100".into()));
        assert_eq!(
            app_url(&me(), &row),
            format!("{root}&thread_ts=1700000000.000100")
        );
    }

    #[test]
    fn permalink_reads_chat_get_permalink() {
        let fake = FakeSlack::new().on(
            "chat.getPermalink",
            &[("channel", "C0CORE"), ("message_ts", "1700000100.000200")],
            Ok(r#"{"ok":true,"permalink":"https://heard.slack.com/archives/C0CORE/p1700000100000200"}"#),
        );
        assert_eq!(
            permalink(&fake, "C0CORE", &Ts("1700000100.000200".into())),
            Ok("https://heard.slack.com/archives/C0CORE/p1700000100000200".into())
        );
        let empty = FakeSlack::new().on("chat.getPermalink", &[], Ok(r#"{"ok":true}"#));
        assert_eq!(
            permalink(&empty, "C0CORE", &Ts("1".into())),
            Err(SlackError::Api("chat.getPermalink: no permalink".into()))
        );
    }
}
