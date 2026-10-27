//! Release an outgoing capture shortly before Omarchy's idle screensaver.
//!
//! While this host forwards input to a peer, the capture surface holds
//! exclusive keyboard focus. Omarchy's screensaver exits as soon as it is not
//! focused, and Omarchy treats that exit as a dismissal that cancels the lock.
//! An idle sender would therefore never lock. Releasing the capture just
//! before the first idle action hands focus back to the desktop in time.

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

/// How long before the first Omarchy idle action the capture is released.
const LEAD: Duration = Duration::from_secs(5);
/// Omarchy's built-in idle timeouts, used when a configured value is absent.
const DEFAULT_SCREENSAVER_SECONDS: u64 = 150;
const DEFAULT_LOCK_SECONDS: u64 = 300;

/// Time without forwarded input after which the capture is released, or
/// `None` when Omarchy idle actions are disabled.
pub(crate) fn release_after() -> Option<Duration> {
    let home = PathBuf::from(env::var_os("HOME")?);
    // Mirror the Omarchy shell: a valid user shell.json replaces the
    // defaults entirely; otherwise the packaged defaults apply.
    let config = read_config(&home.join(".config/omarchy/shell.json")).or_else(|| {
        let omarchy = env::var_os("OMARCHY_PATH")?;
        read_config(&Path::new(&omarchy).join("config/omarchy/shell.json"))
    });
    release_after_for(config.as_ref())
}

/// Whether Omarchy's Stay Awake toggle is on, which disables idle actions.
pub(crate) fn stay_awake() -> bool {
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/state/omarchy/indicators/stay-awake"))
        .is_some_and(|path| path.exists())
}

fn read_config(path: &Path) -> Option<Value> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let valid = value.is_object() && value.get("version").and_then(Value::as_f64) == Some(1.0);
    valid.then_some(value)
}

fn release_after_for(config: Option<&Value>) -> Option<Duration> {
    let idle = config.and_then(|c| c.get("idle"));
    let screensaver = seconds(
        idle.and_then(|i| i.get("screensaver")),
        DEFAULT_SCREENSAVER_SECONDS,
    );
    let lock = seconds(idle.and_then(|i| i.get("lock")), DEFAULT_LOCK_SECONDS);
    // 0 disables an action; the first remaining one is what needs focus.
    let first = [screensaver, lock].into_iter().filter(|&s| s > 0).min()?;
    let first = Duration::from_secs(first);
    let release = first.saturating_sub(LEAD).max(first / 2);
    Some(release.max(Duration::from_secs(1)))
}

/// Omarchy's `secondsFromConfig`: non-negative numbers are floored, anything
/// else falls back to the default.
fn seconds(value: Option<&Value>, fallback: u64) -> u64 {
    let n = match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match n {
        Some(n) if n.is_finite() && n >= 0.0 => n.floor() as u64,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn after(config: Value) -> Option<Duration> {
        release_after_for(Some(&config))
    }

    #[test]
    fn releases_five_seconds_before_the_screensaver() {
        let config = json!({"version": 1, "idle": {"screensaver": 600, "lock": 900}});
        assert_eq!(after(config), Some(Duration::from_secs(595)));
    }

    #[test]
    fn a_disabled_screensaver_leaves_the_lock_as_the_first_action() {
        let config = json!({"version": 1, "idle": {"screensaver": 0, "lock": 300}});
        assert_eq!(after(config), Some(Duration::from_secs(295)));
    }

    #[test]
    fn no_release_when_every_idle_action_is_disabled() {
        let config = json!({"version": 1, "idle": {"screensaver": 0, "lock": 0}});
        assert_eq!(after(config), None);
    }

    #[test]
    fn missing_values_use_omarchy_defaults() {
        assert_eq!(after(json!({"version": 1})), Some(Duration::from_secs(145)));
        assert_eq!(release_after_for(None), Some(Duration::from_secs(145)));
        let bad = json!({"version": 1, "idle": {"screensaver": -3, "lock": "x"}});
        assert_eq!(after(bad), Some(Duration::from_secs(145)));
    }

    #[test]
    fn short_timeouts_release_at_half_the_timeout() {
        let config = json!({"version": 1, "idle": {"screensaver": 6, "lock": 0}});
        assert_eq!(after(config), Some(Duration::from_secs(3)));
        let config = json!({"version": 1, "idle": {"screensaver": 1, "lock": 0}});
        assert_eq!(after(config), Some(Duration::from_secs(1)));
    }

    #[test]
    fn numeric_strings_and_fractions_are_accepted() {
        let config = json!({"version": 1, "idle": {"screensaver": "60", "lock": 90.9}});
        assert_eq!(after(config), Some(Duration::from_secs(55)));
    }

    #[test]
    fn configs_without_version_one_are_rejected() {
        let dir = tempdir();
        let path = dir.join("shell.json");
        fs::write(&path, r#"{"idle": {"screensaver": 10}}"#).unwrap();
        assert!(read_config(&path).is_none());
        fs::write(&path, r#"{"version": 1, "idle": {"screensaver": 10}}"#).unwrap();
        assert!(read_config(&path).is_some());
        fs::write(&path, "not json").unwrap();
        assert!(read_config(&path).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    fn tempdir() -> PathBuf {
        let dir = env::temp_dir().join(format!("glide-idle-release-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
