//! On-disk state files and their shapes (spec §7 "Never miss a message", §9 "Clearing", §10 "Data on disk").

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Msg, Names, Row, RowKey, Ts};
use crate::slack::SlackError;

pub const CACHE: &str = "cache.json"; // written by the fetching worker
pub const STATE: &str = "state.json"; // worker-owned bookkeeping
pub const CLEARS: &str = "clears.json"; // UI-owned (worker only flips keep_watch, Task 6)

/// `cache.json`: the last refresh, shared by all instances.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub fetched_at: Option<DateTime<Utc>>,
    pub error: Option<SlackError>, // whole-refresh failure (auth)
    pub error_at: Option<DateTime<Utc>>,
    pub warnings: Vec<String>, // per-source failures and truncations from the last refresh
    pub me: Option<Me>,
    pub rows: Vec<Row>,
    pub names: Names,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Me {
    pub user_id: String,
    pub team_id: String,
}

/// `state.json`: worker bookkeeping.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub refresh_no: u64,
    pub watch: WatchState,
    pub known_threads: BTreeMap<String, Ts>, // msg_id(channel, root) -> last activity ts
    pub mpims: Vec<String>,
    pub mpims_listed_at: Option<DateTime<Utc>>,
    pub mpim_cursor: usize,
    /// Read cursors (§7 "Overlap"): source -> start of the refresh whose read of it last
    /// succeeded. `ch:<id>` for curated channels and DMs, `search:<query>` per search query.
    #[serde(default)]
    pub last_ok: BTreeMap<String, Ts>,
    /// When the last successful refresh started; sizes the group-DM slice.
    #[serde(default)]
    pub last_refresh_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WatchState {
    pub baseline: Option<BTreeSet<String>>, // msg ids; None until the first successful read
    pub watched: BTreeMap<String, Msg>,     // msg id -> the watched message
}

/// `clears.json`: per-row watermarks.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Clears {
    pub entries: BTreeMap<RowKey, Clear>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clear {
    pub wm: Ts,
    pub keep_watch: bool,
    pub at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StateDir;

    #[test]
    fn state_defaults_and_file_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::new(tmp.path().join("state"));
        let state: State = dir
            .update(STATE, |s: &mut State| {
                s.watch.baseline = Some(BTreeSet::new());
                s.refresh_no += 1;
                true
            })
            .unwrap();
        assert_eq!(dir.load::<State>(STATE).unwrap(), state);
        assert_eq!(dir.load::<Clears>(CLEARS).unwrap(), Clears::default());
        assert_ne!(CLEARS, STATE);
    }

    /// A state.json written before the read cursors existed still loads.
    #[test]
    fn older_state_json_loads() {
        let json = r#"{"version":1,"refresh_no":4,"watch":{"baseline":null,"watched":{}},
            "known_threads":{},"mpims":["G0MPDM1"],"mpims_listed_at":null,"mpim_cursor":0}"#;
        let state: State = serde_json::from_str(json).unwrap();
        assert_eq!(state.refresh_no, 4);
        assert!(state.last_ok.is_empty());
        assert_eq!(state.last_refresh_at, None);
    }

    #[test]
    fn clears_round_trip_through_json() {
        let mut clears = Clears::default();
        clears.entries.insert(
            RowKey("C0CORE:1700000000.000100".into()),
            Clear {
                wm: Ts("1700000100.000200".into()),
                keep_watch: true,
                at: Utc::now(),
            },
        );
        let json = serde_json::to_vec(&clears).unwrap();
        assert_eq!(serde_json::from_slice::<Clears>(&json).unwrap(), clears);
    }
}
