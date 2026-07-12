use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::state;

/// The engine's current time as milliseconds since the Unix epoch. On a
/// production instance this is true calendar time. On a test instance
/// (`TEST_ENV=true`) it is *simulated* time, driven by three state files
/// so the test harness can move and re-rate the clock at runtime:
///
///   simtime = TIME_FACTOR_START
///             + (caltime - TIME_FACTOR_START) * TIME_FACTOR
///             + SIM_TIME_DIFF
///
/// SIM_TIME_DIFF shifts the clock; TIME_FACTOR makes it run faster or
/// slower than calendar time, anchored at TIME_FACTOR_START. All engine
/// logic must take "now" from this module, never from `SystemTime::now()`
/// directly.
pub fn now_millis(config: &Config) -> Result<i64, String> {
    let cal_ms = calendar_now_millis();
    if !config.test_env {
        return Ok(cal_ms);
    }
    Ok(simtime(
        cal_ms,
        state::time_factor_start_ms()?,
        state::time_factor()?,
        state::sim_time_diff_ms()?,
    ))
}

fn simtime(cal_ms: i64, factor_start_ms: i64, factor: f64, diff_ms: i64) -> i64 {
    factor_start_ms + ((cal_ms - factor_start_ms) as f64 * factor) as i64 + diff_ms
}

/// [`now_millis`] as a `SystemTime`, for APIs that want one.
pub fn now(config: &Config) -> Result<SystemTime, String> {
    let millis = now_millis(config)?;
    Ok(if millis >= 0 {
        UNIX_EPOCH + Duration::from_millis(millis as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(millis.unsigned_abs())
    })
}

/// True calendar time, ms since the Unix epoch - never simulated. Engine
/// logic must use [`now_millis`]; this exists for records that must hold
/// real-world time (e.g. a connection profile's established time).
pub fn calendar_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is set before 1970")
        .as_millis() as i64
}

/// The divisor for sleeps and waits: TIME_FACTOR on a test instance,
/// always 1 in production. Anything that sleeps or waits for a period of
/// engine time must divide it by this (via [`scale_wait_ms`]) to get the
/// real time to wait.
pub fn wait_factor(config: &Config) -> Result<f64, String> {
    if config.test_env {
        state::time_factor()
    } else {
        Ok(1.0)
    }
}

/// A wait of `engine_ms` of engine time as real milliseconds: divided by
/// the time factor (e.g. an engine-250ms wait at TIME_FACTOR=5 is a real
/// 50ms), never less than 1ms.
pub fn scale_wait_ms(engine_ms: u64, factor: f64) -> u64 {
    ((engine_ms as f64 / factor).max(1.0)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simtime_formula() {
        // start + (cal - start) * factor + diff
        assert_eq!(simtime(2_000, 1_000, 5.0, 30), 1_000 + 5_000 + 30);
        assert_eq!(simtime(3_000, 1_000, 0.5, 0), 1_000 + 1_000);
    }

    #[test]
    fn simtime_at_factor_one_is_calendar_plus_diff() {
        assert_eq!(simtime(123_456, 99_999, 1.0, 700), 123_456 + 700);
    }

    #[test]
    fn scale_wait_divides_by_the_factor() {
        assert_eq!(scale_wait_ms(250, 5.0), 50);
        assert_eq!(scale_wait_ms(100, 0.5), 200);
        assert_eq!(scale_wait_ms(250, 1.0), 250);
    }

    #[test]
    fn scale_wait_never_reaches_zero() {
        assert_eq!(scale_wait_ms(1, 1000.0), 1);
    }

    #[test]
    fn production_never_touches_state() {
        // DOCUMENT_ROOT is not set in unit tests, so the state reads would
        // error - a production config must not even try them.
        let config = Config { test_env: false, ..Default::default() };
        let real = calendar_now_millis();
        let engine = now_millis(&config).unwrap();
        assert!((engine - real).abs() < 1_000);
        assert_eq!(wait_factor(&config).unwrap(), 1.0);
    }

    #[test]
    fn test_instance_applies_state_files() {
        // The one test that sets DOCUMENT_ROOT (keep it that way - env vars
        // are process-global and cargo runs tests in parallel threads).
        let dir = std::env::temp_dir().join(format!("stadhouder-time-test-{}", std::process::id()));
        let state_dir = dir.join("stadhouder").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("sim_time_diff"), "86400000").unwrap();
        std::env::set_var("DOCUMENT_ROOT", dir.join("public_html"));

        let config = Config { test_env: true, ..Default::default() };

        // Factor files absent: factor 1, so simtime = cal + diff.
        let real = calendar_now_millis();
        let engine = now_millis(&config).unwrap();
        assert!((engine - real - 86_400_000).abs() < 1_000);
        assert_eq!(wait_factor(&config).unwrap(), 1.0);

        // With a rate: anchor at (about) now, run 100x. Right at the anchor
        // the factor contributes ~0, so simtime is still ~cal + diff; the
        // rate shows up in the wait factor immediately.
        std::fs::write(state_dir.join("time_factor"), "100").unwrap();
        std::fs::write(state_dir.join("time_factor_start"), real.to_string()).unwrap();
        let engine = now_millis(&config).unwrap();
        let cal = calendar_now_millis();
        let expected = real + (cal - real) * 100 + 86_400_000;
        assert!((engine - expected).abs() < 2_000, "engine {engine} vs expected {expected}");
        assert_eq!(wait_factor(&config).unwrap(), 100.0);

        std::env::remove_var("DOCUMENT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
