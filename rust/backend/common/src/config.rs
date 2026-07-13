use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::layout;

/// Instance configuration read from a `KEY=value` file outside the webroot:
/// `~/stadhouder/cfg/stadhouder.conf` on cPanel,
/// `site/stadhouder/cfg/stadhouder.conf` locally. A missing file is a valid
/// production configuration - every key has a safe default. Mutable runtime
/// state (e.g. the simulated-time offset) does NOT belong here; that lives
/// under `stadhouder/state/` instead (see [`crate::state`]).
pub struct Config {
    /// True if this is a test instance of stadhouder. Only a test instance
    /// may run on simulated time.
    pub test_env: bool,
    /// When set, the client CGI program's `connect` action requires a
    /// cookie of this name to be present on the request (its value is
    /// never inspected - only presence). Unset means no such check: a
    /// deployment that fronts stadhouder with its own authentication
    /// (e.g. checking a session itself before ever calling `connect`)
    /// isn't forced to also duplicate a cookie name here.
    pub cookie_name: Option<String>,
    /// When set, the client CGI program's `connect` action calls this
    /// same-host path (e.g. `/cgi-bin/keyscarf/api_session_check`),
    /// forwarding the request's own Cookie header, to verify who is
    /// actually calling instead of trusting the request's self-asserted
    /// user_id. Stadhouder assumes nothing about the response's shape
    /// beyond it being JSON - see `user_id_json` for how the verified id
    /// is found in it. Unset means no such check - the request's user_id
    /// is trusted as given, which is only appropriate when whatever
    /// fronts stadhouder has already authenticated the caller by some
    /// other means.
    pub user_id_url: Option<String>,
    /// The dot-separated field path to read the verified id from in
    /// `user_id_url`'s JSON response (e.g. `data.id`) - see
    /// [`crate::identity`]. Required whenever `user_id_url` is set.
    pub user_id_json: Option<String>,
    /// The service's own memory ceiling, in bytes, from `MEMORY_LIMIT_MB`.
    /// Unset means no limit - the service never measures its own memory and
    /// `connect` never refuses on that basis (see
    /// [`crate::state::ServiceFlag::memory_pct`]).
    pub memory_limit_bytes: Option<u64>,
    /// The `memory_pct` (percentage of `memory_limit_bytes`) at or above
    /// which the client CGI program's `connect` action refuses new
    /// connections, from `MEMORY_REJECT_PCT`. Defaults to `100.0`. Only
    /// meaningful when `memory_limit_bytes` is also set - `memory_pct` is
    /// always `0.0` otherwise, so this never triggers.
    pub memory_reject_pct: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            test_env: false,
            cookie_name: None,
            user_id_url: None,
            user_id_json: None,
            memory_limit_bytes: None,
            memory_reject_pct: 100.0,
        }
    }
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }

        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::from_str(&contents).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn from_str(contents: &str) -> Result<Config, String> {
        let values = parse_kv(contents);

        let test_env = match values.get("TEST_ENV").map(String::as_str) {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("true") => true,
            Some(v) if v.eq_ignore_ascii_case("false") => false,
            Some(v) => return Err(format!("TEST_ENV must be true or false, not '{v}'")),
        };

        // Blank counts as unset, same leniency as the other optional keys
        // here - a stray "COOKIE_NAME=" line shouldn't require a cookie
        // literally named the empty string.
        let cookie_name = values
            .get("COOKIE_NAME")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let user_id_url = values
            .get("USER_ID_URL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let user_id_json = values
            .get("USER_ID_JSON")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let memory_limit_bytes = match values.get("MEMORY_LIMIT_MB").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => {
                let mb: u64 = v
                    .parse()
                    .map_err(|_| format!("MEMORY_LIMIT_MB must be a positive integer, not '{v}'"))?;
                if mb == 0 {
                    return Err("MEMORY_LIMIT_MB must be > 0".to_string());
                }
                Some(mb * 1024 * 1024)
            }
        };

        let memory_reject_pct = match values.get("MEMORY_REJECT_PCT").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => 100.0,
            Some(v) => {
                let pct: f64 = v
                    .parse()
                    .map_err(|_| format!("MEMORY_REJECT_PCT must be a positive number, not '{v}'"))?;
                if !(pct > 0.0) {
                    return Err("MEMORY_REJECT_PCT must be > 0".to_string());
                }
                pct
            }
        };

        Ok(Config {
            test_env,
            cookie_name,
            user_id_url,
            user_id_json,
            memory_limit_bytes,
            memory_reject_pct,
        })
    }

    /// `<home>/stadhouder/cfg/stadhouder.conf` (see [`crate::layout`]).
    fn config_path() -> Result<PathBuf, String> {
        Ok(layout::stadhouder_dir()?.join("cfg").join("stadhouder.conf"))
    }
}

/// `KEY=value` lines; blank lines and `#` comments are ignored, whitespace
/// around keys and values is trimmed.
fn parse_kv(contents: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(key.trim().to_string(), value.trim().to_string());
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_reads_simple_pairs() {
        let values = parse_kv("TEST_ENV=true\nOTHER=1000");
        assert_eq!(values.get("TEST_ENV").map(String::as_str), Some("true"));
        assert_eq!(values.get("OTHER").map(String::as_str), Some("1000"));
    }

    #[test]
    fn parse_kv_skips_comments_and_blank_lines() {
        let values = parse_kv("# a comment\n\nTEST_ENV=true\n");
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn parse_kv_trims_whitespace() {
        let values = parse_kv("  TEST_ENV =  true  ");
        assert_eq!(values.get("TEST_ENV").map(String::as_str), Some("true"));
    }

    #[test]
    fn empty_config_gives_defaults() {
        let config = Config::from_str("").unwrap();
        assert!(!config.test_env);
        assert_eq!(config.cookie_name, None);
    }

    #[test]
    fn cookie_name_is_read() {
        let config = Config::from_str("COOKIE_NAME=keyscarf_session").unwrap();
        assert_eq!(config.cookie_name.as_deref(), Some("keyscarf_session"));
    }

    #[test]
    fn blank_cookie_name_counts_as_unset() {
        let config = Config::from_str("COOKIE_NAME=").unwrap();
        assert_eq!(config.cookie_name, None);
    }

    #[test]
    fn user_id_url_is_read() {
        let config = Config::from_str("USER_ID_URL=/cgi-bin/keyscarf/api_session_check").unwrap();
        assert_eq!(config.user_id_url.as_deref(), Some("/cgi-bin/keyscarf/api_session_check"));
    }

    #[test]
    fn blank_user_id_url_counts_as_unset() {
        let config = Config::from_str("USER_ID_URL=").unwrap();
        assert_eq!(config.user_id_url, None);
    }

    #[test]
    fn user_id_json_is_read() {
        let config = Config::from_str("USER_ID_JSON=data.id").unwrap();
        assert_eq!(config.user_id_json.as_deref(), Some("data.id"));
    }

    #[test]
    fn blank_user_id_json_counts_as_unset() {
        let config = Config::from_str("USER_ID_JSON=").unwrap();
        assert_eq!(config.user_id_json, None);
    }

    #[test]
    fn test_env_is_read() {
        assert!(Config::from_str("TEST_ENV=true").unwrap().test_env);
    }

    #[test]
    fn test_env_is_case_insensitive() {
        assert!(Config::from_str("TEST_ENV=True").unwrap().test_env);
        assert!(!Config::from_str("TEST_ENV=FALSE").unwrap().test_env);
    }

    #[test]
    fn bad_test_env_is_rejected() {
        assert!(Config::from_str("TEST_ENV=yes").is_err());
    }

    #[test]
    fn memory_limit_mb_is_read_as_bytes() {
        let config = Config::from_str("MEMORY_LIMIT_MB=256").unwrap();
        assert_eq!(config.memory_limit_bytes, Some(256 * 1024 * 1024));
    }

    #[test]
    fn blank_memory_limit_mb_counts_as_unset() {
        let config = Config::from_str("MEMORY_LIMIT_MB=").unwrap();
        assert_eq!(config.memory_limit_bytes, None);
    }

    #[test]
    fn zero_memory_limit_mb_is_rejected() {
        assert!(Config::from_str("MEMORY_LIMIT_MB=0").is_err());
    }

    #[test]
    fn non_numeric_memory_limit_mb_is_rejected() {
        assert!(Config::from_str("MEMORY_LIMIT_MB=lots").is_err());
    }

    #[test]
    fn memory_reject_pct_defaults_to_100() {
        let config = Config::from_str("").unwrap();
        assert_eq!(config.memory_reject_pct, 100.0);
    }

    #[test]
    fn memory_reject_pct_is_read() {
        let config = Config::from_str("MEMORY_REJECT_PCT=90").unwrap();
        assert_eq!(config.memory_reject_pct, 90.0);
    }

    #[test]
    fn blank_memory_reject_pct_defaults_to_100() {
        let config = Config::from_str("MEMORY_REJECT_PCT=").unwrap();
        assert_eq!(config.memory_reject_pct, 100.0);
    }

    #[test]
    fn zero_memory_reject_pct_is_rejected() {
        assert!(Config::from_str("MEMORY_REJECT_PCT=0").is_err());
    }

    #[test]
    fn negative_memory_reject_pct_is_rejected() {
        assert!(Config::from_str("MEMORY_REJECT_PCT=-5").is_err());
    }

    #[test]
    fn non_numeric_memory_reject_pct_is_rejected() {
        assert!(Config::from_str("MEMORY_REJECT_PCT=lots").is_err());
    }
}
