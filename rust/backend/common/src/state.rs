use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout;

/// File-based runtime state, under `<home>/stadhouder/state/`
/// (`site/stadhouder/state/` locally) - see [`crate::layout`]. Stadhouder
/// has no database; anything the engine framework remembers between
/// requests lives as files here.
pub fn state_dir() -> Result<PathBuf, String> {
    Ok(layout::stadhouder_dir()?.join("state"))
}

/// The difference between the simulation time and the true calendar time,
/// in milliseconds, read from `state/sim_time_diff` - a bare integer,
/// mutable at runtime so tests can move time around (see
/// [`crate::time::now_millis`]). A missing file means 0: simulation time
/// runs at true calendar time. Only ever consulted on a test instance.
pub fn sim_time_diff_ms() -> Result<i64, String> {
    Ok(read_int_file("sim_time_diff")?.unwrap_or(0))
}

/// The rate simulation time passes at relative to calendar time, read from
/// `state/time_factor` - e.g. 5 means five simulated seconds per real one;
/// fractions slow time down. A missing file means 1: simulation time at
/// calendar rate. Must be > 0. Only ever consulted on a test instance -
/// see [`crate::time::now_millis`].
pub fn time_factor() -> Result<f64, String> {
    match read_float_file("time_factor")? {
        None => Ok(1.0),
        Some(factor) if factor > 0.0 => Ok(factor),
        Some(factor) => Err(format!("time_factor must be > 0, not {factor}")),
    }
}

/// The calendar-time anchor (ms since the Unix epoch) the TIME_FACTOR rate
/// applies from, read from `state/time_factor_start`. A missing file means
/// 0. Only ever consulted on a test instance.
pub fn time_factor_start_ms() -> Result<i64, String> {
    Ok(read_int_file("time_factor_start")?.unwrap_or(0))
}

/// The engine time (ms since the Unix epoch) of the service's most recent
/// cron run, from `state/last_run` - `None` if the service has never run.
pub fn last_run_ms() -> Result<Option<i64>, String> {
    read_int_file("last_run")
}

/// The service flag file: proof of a live service instance. Cron starts
/// the service every minute; a starting instance that finds a fresh flag
/// (another instance still updating it) terminates instead of competing.
/// Holds the engine time (ms) it was last updated at.
pub fn service_flag_path(state_dir: &Path) -> PathBuf {
    state_dir.join("service_flag")
}

/// The engine time the flag was last updated at, or `None` if no flag
/// exists (no instance is running, or the last one crashed - staleness is
/// the caller's judgement). An empty file reads as `None`, like the other
/// state files.
pub fn read_service_flag_ms(state_dir: &Path) -> Result<Option<i64>, String> {
    let path = service_flag_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    parse_int(&contents)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn write_service_flag_ms(state_dir: &Path, engine_ms: i64) -> Result<(), String> {
    fs::create_dir_all(state_dir)
        .map_err(|e| format!("failed to create {}: {e}", state_dir.display()))?;
    let path = service_flag_path(state_dir);
    fs::write(&path, format!("{engine_ms}\n"))
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Best-effort - a missing flag is already the desired end state.
pub fn remove_service_flag(state_dir: &Path) {
    let _ = fs::remove_file(service_flag_path(state_dir));
}

/// Records a service run's engine time in `state/last_run`, creating the
/// state directory if this is the very first thing to write state.
pub fn write_last_run_ms(engine_ms: i64) -> Result<(), String> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let path = dir.join("last_run");
    fs::write(&path, format!("{engine_ms}\n"))
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// A client connection's profile, written to the state directory by the
/// client CGI program when a client requests a connection, and picked up
/// from there by the running service program. The file is named
/// `connection_<uuid>.json` (dashes as underscores - see
/// [`layout::underscore_uuid`]); deleting it is how a client-side close is
/// signalled, and the service deletes it when it closes the connection.
#[derive(Serialize, Deserialize)]
pub struct ConnectionProfile {
    /// The connection uuid (dashed form).
    pub connection_id: String,
    /// Meaningful to the specific state engine; opaque to the framework.
    pub user_id: String,
    /// The client-to-server pipe's name (a full platform pipe name - see
    /// `crate::pipes`).
    pub client_to_server_pipe: String,
    /// The server-to-client pipe's name.
    pub server_to_client_pipe: String,
    /// The calendar time the connection was established (ms since the Unix
    /// epoch) - always true calendar time, never simulated.
    pub established_ms: i64,
    /// The simtime the connection was established - only present when
    /// TEST_ENV=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub established_sim_ms: Option<i64>,
}

impl ConnectionProfile {
    pub fn path_in(state_dir: &Path, connection_id: &str) -> PathBuf {
        state_dir.join(format!(
            "connection_{}.json",
            layout::underscore_uuid(connection_id)
        ))
    }

    /// Writes the profile into `state_dir`, atomically (tmp + rename), so
    /// the service's directory scan never sees a half-written profile.
    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(state_dir)
            .map_err(|e| format!("failed to create {}: {e}", state_dir.display()))?;
        let path = Self::path_in(state_dir, &self.connection_id);
        let tmp = state_dir.join(format!(
            ".tmp_connection_{}",
            layout::underscore_uuid(&self.connection_id)
        ));
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize connection profile: {e}"))?;
        fs::write(&tmp, contents).map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .map_err(|e| format!("failed to move profile into place at {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> Result<ConnectionProfile, String> {
        let contents = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&contents).map_err(|e| format!("failed to parse {}: {e}", path.display()))
    }
}

/// Every connection profile file currently in `state_dir`, as paths.
pub fn connection_profile_paths(state_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("connection_") && name.ends_with(".json") {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

/// Removes a connection's profile. `Ok(true)` if it existed.
pub fn delete_connection_profile(state_dir: &Path, connection_id: &str) -> Result<bool, String> {
    let path = ConnectionProfile::path_in(state_dir, connection_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("failed to delete {}: {e}", path.display())),
    }
}

/// A state file holding one bare integer. `None` if the file doesn't exist
/// or is (momentarily) empty - a shell's `echo x > file` truncates before
/// it writes, and these files are re-read constantly, so an empty read is
/// normal, not an error. An unparseable non-empty file IS an error, never
/// silently a default.
fn read_int_file(name: &str) -> Result<Option<i64>, String> {
    let path = state_dir()?.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    parse_int(&contents)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_int(contents: &str) -> Result<i64, String> {
    contents
        .trim()
        .parse::<i64>()
        .map_err(|_| format!("must hold one integer, not '{}'", contents.trim()))
}

/// Like [`read_int_file`] but for a state file holding one number that may
/// have a fractional part.
fn read_float_file(name: &str) -> Result<Option<f64>, String> {
    let path = state_dir()?.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    parse_float(&contents)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_float(contents: &str) -> Result<f64, String> {
    contents
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("must hold one number, not '{}'", contents.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_integer() {
        assert_eq!(parse_int("86400000").unwrap(), 86_400_000);
    }

    #[test]
    fn parses_negative_integer() {
        assert_eq!(parse_int("-3600000").unwrap(), -3_600_000);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(parse_int("  1000\n").unwrap(), 1_000);
    }

    #[test]
    fn rejects_non_integer() {
        assert!(parse_int("tomorrow").is_err());
    }

    #[test]
    fn parses_float_with_fraction() {
        assert_eq!(parse_float("2.5").unwrap(), 2.5);
        assert_eq!(parse_float(" 0.25\n").unwrap(), 0.25);
    }

    #[test]
    fn rejects_non_number_float() {
        assert!(parse_float("fast").is_err());
    }
}
