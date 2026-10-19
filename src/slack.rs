//! Slack Web API client and its typed errors (spec §6). Only the error type exists so far.

use serde::{Deserialize, Serialize};

/// Why a Slack call failed, stored in the cache so every pane can show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SlackError {
    Auth(String),
    RateLimited(String),
    Api(String),
    Http(String),
}
