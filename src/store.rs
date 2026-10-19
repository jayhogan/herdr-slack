//! State directory: locked JSON files shared by every pane's instance, plus the
//! shared fetch lock (spec §10 "Data on disk": dir 0700, files 0600).

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// `$XDG_STATE_HOME`, else `$HOME/.local/state`. An empty or relative `XDG_STATE_HOME`
/// counts as unset, as the XDG spec says.
fn state_home(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or_default()).join(".local/state"))
}

/// Directory of JSON files, each guarded by an advisory lock on `<name>.lock`.
pub struct StateDir {
    dir: PathBuf,
}

impl StateDir {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `${XDG_STATE_HOME:-~/.local/state}/herdr-slack`.
    pub fn default_location() -> Self {
        let home = state_home(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"));
        Self::new(home.join("herdr-slack"))
    }

    fn lock_file(&self, file_name: &str) -> io::Result<File> {
        self.ensure_dir()?;
        let mut options = File::options();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(self.dir.join(file_name))
    }

    /// Creates the directory if needed and keeps it private (0700).
    fn ensure_dir(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// Missing file -> default. Corrupt file -> moved to `<name>.bak`, default.
    pub fn load<T: Default + DeserializeOwned>(&self, name: &str) -> io::Result<T> {
        let path = self.dir.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(value),
            Err(e) => {
                eprintln!("herdr-slack: {name} is corrupt ({e}); starting fresh");
                fs::rename(&path, self.dir.join(format!("{name}.bak")))?;
                Ok(T::default())
            }
        }
    }

    /// Locks `<name>.lock`, loads the latest value, applies `change`, writes it
    /// back if `change` returned true, and returns the resulting value.
    pub fn update<T: Default + Serialize + DeserializeOwned>(
        &self,
        name: &str,
        change: impl FnOnce(&mut T) -> bool,
    ) -> io::Result<T> {
        let lock = self.lock_file(&format!("{name}.lock"))?;
        lock.lock()?;
        let mut value: T = self.load(name)?;
        if change(&mut value) {
            let tmp = self.dir.join(format!("{name}.tmp"));
            let mut options = File::options();
            options.create(true).truncate(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            options
                .open(&tmp)?
                .write_all(&serde_json::to_vec_pretty(&value)?)?;
            fs::rename(&tmp, self.dir.join(name))?;
        }
        Ok(value)
        // `lock` is released when dropped here.
    }

    /// Non-blocking: `None` when another instance holds the fetch lock. The
    /// lock is held until the returned file is dropped.
    pub fn try_fetch_lock(&self) -> io::Result<Option<File>> {
        let file = self.lock_file("fetch.lock")?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Blocking variant, for a manual refresh.
    pub fn fetch_lock(&self) -> io::Result<File> {
        let file = self.lock_file("fetch.lock")?;
        file.lock()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CACHE, Cache, Me};

    fn dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path().join("state"));
        (tmp, state)
    }

    #[test]
    fn empty_xdg_state_home_counts_as_unset() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            state_home(some("/x/state"), some("/home/j")),
            PathBuf::from("/x/state")
        );
        let fallback = PathBuf::from("/home/j/.local/state");
        assert_eq!(state_home(None, some("/home/j")), fallback);
        assert_eq!(state_home(some(""), some("/home/j")), fallback);
        // The XDG spec ignores a relative path too.
        assert_eq!(state_home(some("state"), some("/home/j")), fallback);
    }

    #[test]
    fn update_round_trips() {
        let (_tmp, state) = dir();
        let out: Cache = state
            .update(CACHE, |c: &mut Cache| {
                c.version = 1;
                c.me = Some(Me {
                    user_id: "U0ME".into(),
                    team_id: "T0HEARD".into(),
                });
                c.warnings.push("search failed".into());
                true
            })
            .unwrap();
        assert_eq!(out.me.as_ref().unwrap().user_id, "U0ME");
        let loaded: Cache = state.load(CACHE).unwrap();
        assert_eq!(loaded, out);
    }

    #[test]
    fn update_without_change_does_not_write() {
        let (_tmp, state) = dir();
        let _: Cache = state.update(CACHE, |_| false).unwrap();
        assert!(!state.dir.join(CACHE).exists());
    }

    #[test]
    fn missing_file_is_default() {
        let (_tmp, state) = dir();
        let c: Cache = state.load(CACHE).unwrap();
        assert_eq!(c, Cache::default());
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let (_tmp, state) = dir();
        fs::create_dir_all(&state.dir).unwrap();
        fs::write(state.dir.join(CACHE), b"{not json").unwrap();
        let c: Cache = state.load(CACHE).unwrap();
        assert_eq!(c, Cache::default());
        assert!(state.dir.join("cache.json.bak").exists());
        assert!(!state.dir.join(CACHE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        let (_tmp, state) = dir();
        let _: Cache = state
            .update(CACHE, |c: &mut Cache| {
                c.version = 1;
                true
            })
            .unwrap();
        let _lock = state.try_fetch_lock().unwrap();
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(state.dir.clone()), 0o700);
        assert_eq!(mode(state.dir.join(CACHE)), 0o600);
        assert_eq!(mode(state.dir.join("cache.json.lock")), 0o600);
        assert_eq!(mode(state.dir.join("fetch.lock")), 0o600);
    }

    #[test]
    fn default_location_is_named_for_the_app() {
        assert!(StateDir::default_location().dir.ends_with("herdr-slack"));
    }

    #[test]
    fn blocking_fetch_lock_waits_for_try_lock_holder() {
        let (_tmp, state) = dir();
        drop(state.try_fetch_lock().unwrap());
        let held = state.fetch_lock().unwrap();
        assert!(state.try_fetch_lock().unwrap().is_none());
        drop(held);
    }

    #[test]
    fn fetch_lock_is_exclusive() {
        let (_tmp, state) = dir();
        let first = state.try_fetch_lock().unwrap();
        assert!(first.is_some());
        assert!(state.try_fetch_lock().unwrap().is_none());
        drop(first);
        assert!(state.try_fetch_lock().unwrap().is_some());
    }
}
