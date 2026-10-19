//! Slack Web API client and its typed errors (spec §6): one `call`, pacing per rate tier,
//! `Retry-After` handling, and Slack's `ok: false` mapped to `SlackError`.

use std::cell::RefCell;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why a Slack call failed, stored in the cache so every pane can show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SlackError {
    /// not_authed, invalid_auth, token_revoked, account_inactive, or a missing token.
    Auth(String),
    /// The method name, after retries.
    RateLimited(String),
    /// `"<method>: <error code>"`.
    Api(String),
    Http(String),
}

pub trait Slack: Send {
    /// GET `https://slack.com/api/<method>` with Bearer auth; the parsed body when `"ok": true`.
    fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value, SlackError>;
}

const AUTH_CODES: [&str; 4] = [
    "not_authed",
    "invalid_auth",
    "token_revoked",
    "account_inactive",
];
const TIER2_GAP: Duration = Duration::from_secs(4);
const OTHER_GAP: Duration = Duration::from_millis(1500);
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;

/// Maps a Slack response body to `Ok(body)` or the matching error.
pub fn classify(method: &str, body: &Value) -> Result<Value, SlackError> {
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(body.clone());
    }
    let code = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    if AUTH_CODES.contains(&code) {
        Err(SlackError::Auth(code.to_owned()))
    } else {
        Err(SlackError::Api(format!("{method}: {code}")))
    }
}

/// The `Retry-After` header in seconds; 30 s when absent or unparsable.
pub fn retry_after(header: Option<&str>) -> Duration {
    header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
}

fn is_tier2(method: &str) -> bool {
    matches!(method, "search.messages" | "reactions.list")
}

pub struct HttpSlack {
    token: String,
    agent: ureq::Agent,
    paced: bool,
    /// Last call time for (tier 2, every other method).
    last: RefCell<(Option<Instant>, Option<Instant>)>,
}

impl HttpSlack {
    pub fn new(token: String, paced: bool) -> HttpSlack {
        let config = ureq::Agent::config_builder()
            // Read 429 ourselves for its Retry-After header.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        HttpSlack {
            token,
            agent: config.into(),
            paced,
            last: RefCell::new((None, None)),
        }
    }

    fn pace(&self, method: &str) {
        if !self.paced {
            return;
        }
        let (gap, tier2) = if is_tier2(method) {
            (TIER2_GAP, true)
        } else {
            (OTHER_GAP, false)
        };
        let mut last = self.last.borrow_mut();
        let slot = if tier2 { &mut last.0 } else { &mut last.1 };
        if let Some(at) = *slot {
            let wait = gap.saturating_sub(at.elapsed());
            if !wait.is_zero() {
                thread::sleep(wait);
            }
        }
        *slot = Some(Instant::now());
    }
}

impl Slack for HttpSlack {
    fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value, SlackError> {
        let url = format!("https://slack.com/api/{method}");
        let mut retries = 0;
        loop {
            self.pace(method);
            let mut resp = self
                .agent
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .query_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
                .call()
                .map_err(|e| SlackError::Http(format!("{method}: {e}")))?;
            let status = resp.status().as_u16();
            if status == 429 {
                if retries == MAX_RETRIES {
                    return Err(SlackError::RateLimited(method.to_owned()));
                }
                retries += 1;
                let header = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok());
                thread::sleep(retry_after(header));
                continue;
            }
            if status >= 300 {
                return Err(SlackError::Http(format!("{method}: HTTP {status}")));
            }
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| SlackError::Http(format!("{method}: {e}")))?;
            let body: Value = serde_json::from_str(&text)
                .map_err(|e| SlackError::Http(format!("{method}: bad response: {e}")))?;
            return classify(method, &body);
        }
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;

    pub type Call = (String, Vec<(String, String)>);
    type Rule = (String, Vec<(String, String)>, Result<String, SlackError>);

    /// Scripted `Slack` for tests; records every call.
    #[derive(Default)]
    pub struct FakeSlack {
        pub calls: RefCell<Vec<Call>>,
        rules: Vec<Rule>,
    }

    impl FakeSlack {
        pub fn new() -> Self {
            Self::default()
        }

        /// Answers `method` when every (k, v) in `when` is among the call's params; later rules win.
        pub fn on(
            mut self,
            method: &str,
            when: &[(&str, &str)],
            answer: Result<&str, SlackError>,
        ) -> Self {
            self.rules.push((
                method.to_owned(),
                when.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                answer.map(str::to_owned),
            ));
            self
        }
    }

    impl Slack for FakeSlack {
        fn call(&self, method: &str, params: &[(&str, String)]) -> Result<Value, SlackError> {
            let params: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            self.calls
                .borrow_mut()
                .push((method.to_owned(), params.clone()));
            let rule = self
                .rules
                .iter()
                .rev()
                .find(|(m, when, _)| m == method && when.iter().all(|kv| params.contains(kv)));
            match rule {
                Some((_, _, Ok(body))) => serde_json::from_str(body)
                    .map_err(|e| SlackError::Http(format!("{method}: bad fake body: {e}"))),
                Some((_, _, Err(e))) => Err(e.clone()),
                None => Err(SlackError::Api(format!("{method}: no fake answer"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSlack;
    use super::*;
    use serde_json::json;

    #[test]
    fn error_codes_map() {
        let ok = json!({"ok": true, "x": 1});
        assert_eq!(classify("m", &ok), Ok(ok.clone()));
        for code in AUTH_CODES {
            assert_eq!(
                classify("m", &json!({"ok": false, "error": code})),
                Err(SlackError::Auth(code.into()))
            );
        }
        assert_eq!(
            classify(
                "conversations.info",
                &json!({"ok": false, "error": "channel_not_found"})
            ),
            Err(SlackError::Api(
                "conversations.info: channel_not_found".into()
            ))
        );
        assert_eq!(
            classify("m", &json!({"ok": false})),
            Err(SlackError::Api("m: unknown_error".into()))
        );
        let fake = FakeSlack::new().on(
            "auth.test",
            &[],
            Err(SlackError::Auth("invalid_auth".into())),
        );
        assert_eq!(
            fake.call("auth.test", &[]),
            Err(SlackError::Auth("invalid_auth".into()))
        );
    }

    #[test]
    fn retry_after_parsed() {
        assert_eq!(retry_after(Some("7")), Duration::from_secs(7));
        assert_eq!(retry_after(Some(" 12 ")), Duration::from_secs(12));
        assert_eq!(retry_after(None), Duration::from_secs(30));
        assert_eq!(retry_after(Some("soon")), Duration::from_secs(30));
    }

    #[test]
    fn tiers() {
        assert!(is_tier2("search.messages"));
        assert!(is_tier2("reactions.list"));
        assert!(!is_tier2("reactions.get"));
        assert!(!is_tier2("conversations.history"));
    }

    #[test]
    fn fake_later_rules_win_and_unmatched_errors() {
        let fake = FakeSlack::new()
            .on("m", &[], Ok(r#"{"ok":true,"v":1}"#))
            .on("m", &[("a", "2")], Ok(r#"{"ok":true,"v":2}"#));
        assert_eq!(fake.call("m", &[("a", "2".into())]).unwrap()["v"], 2);
        assert_eq!(fake.call("m", &[("a", "3".into())]).unwrap()["v"], 1);
        assert_eq!(
            fake.call("other", &[]),
            Err(SlackError::Api("other: no fake answer".into()))
        );
        assert_eq!(fake.calls.borrow().len(), 3);
    }
}
