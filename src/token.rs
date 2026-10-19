//! Slack token lookup: keychain, then env (spec §10 "Config and secrets"; banner text from §9).

use std::env;
use std::process::Command;

/// Shown as the auth banner (spec §9) and as the error when no token is found.
pub const MISSING_TOKEN: &str = "Slack token missing or invalid. Add it with: security add-generic-password -s herdr-slack -a \"$USER\" -w";

/// Keychain first (macOS), then `HERDR_SLACK_TOKEN`. The token is never written or logged.
pub fn read() -> Result<String, String> {
    pick(keychain(), env::var("HERDR_SLACK_TOKEN").ok())
}

fn keychain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "herdr-slack", "-w"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pick(keychain: Option<String>, env: Option<String>) -> Result<String, String> {
    [keychain, env]
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
        .ok_or_else(|| MISSING_TOKEN.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn token_prefers_keychain() {
        assert_eq!(pick(s("xoxp-key\n"), s("xoxp-env")), Ok("xoxp-key".into()));
    }

    #[test]
    fn token_falls_back_to_env() {
        assert_eq!(pick(None, s("xoxp-env")), Ok("xoxp-env".into()));
        assert_eq!(pick(s("  \n"), s("xoxp-env")), Ok("xoxp-env".into()));
    }

    #[test]
    fn token_missing_errors() {
        assert_eq!(pick(None, None), Err(MISSING_TOKEN.to_string()));
        assert_eq!(pick(s(""), s("")), Err(MISSING_TOKEN.to_string()));
    }
}
