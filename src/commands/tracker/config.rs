use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Clone)]
pub struct TrackerConfig {
    pub tracker_url: String,
    pub team_id: String,
    pub team_key: String,
    pub username: Option<String>,
    pub blacklist: Vec<String>,
    #[serde(
        default = "default_token_report",
        deserialize_with = "deserialize_token_report"
    )]
    pub token_report: bool,
}

fn default_token_report() -> bool {
    true
}

// Fail closed for token reporting without invalidating the shared config and
// accidentally disabling code reporting too. Missing fields default to true;
// explicitly malformed values (including null) disable only token reporting.
fn deserialize_token_report<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value.as_bool() {
        Some(enabled) => Ok(enabled),
        None => {
            tracing::warn!(
                "tracker config: token_report must be a JSON boolean; token reporting disabled"
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod token_report_tests {
    use super::*;

    fn config(value: Option<serde_json::Value>) -> TrackerConfig {
        let mut raw = serde_json::json!({
            "tracker_url": "http://localhost", "team_id": "1", "team_key": "key",
            "username": "user", "blacklist": ["private"]
        });
        if let Some(value) = value {
            raw["token_report"] = value;
        }
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn token_report_defaults_on_and_preserves_explicit_values() {
        assert!(config(None).token_report);
        assert!(config(Some(serde_json::json!(true))).token_report);
        let disabled = config(Some(serde_json::json!(false)));
        let mut saved = disabled.clone();
        saved.blacklist.push("another".into());
        let encoded = serde_json::to_value(&saved).unwrap();
        assert_eq!(encoded["token_report"], false);
        assert!(
            !serde_json::from_value::<TrackerConfig>(encoded)
                .unwrap()
                .token_report
        );
        assert_eq!(
            serde_json::to_value(config(None)).unwrap()["token_report"],
            true
        );
    }

    #[test]
    fn token_report_invalid_types_disable_only_tokens() {
        for value in [
            serde_json::json!("false"),
            serde_json::json!(0),
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let parsed = config(Some(value));
            assert!(!parsed.token_report);
            assert_eq!(parsed.tracker_url, "http://localhost");
            assert_eq!(parsed.team_key, "key");
            assert_eq!(parsed.blacklist, ["private"]);
        }
    }

    /// A user who turned reporting off must stay off: only a *missing* key is
    /// added, and every other byte of the file is preserved.
    #[test]
    fn expose_token_report_switch_adds_missing_key_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker-config.json");

        std::fs::write(&path, r#"{"tracker_url":"http://x","team_id":"1"}"#).unwrap();
        assert!(ensure_token_report_field_at(&path).unwrap());
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["token_report"], true);
        assert_eq!(written["tracker_url"], "http://x");
        // Idempotent.
        assert!(!ensure_token_report_field_at(&path).unwrap());

        std::fs::write(
            &path,
            r#"{"tracker_url":"http://x","team_id":"1","token_report":false}"#,
        )
        .unwrap();
        assert!(!ensure_token_report_field_at(&path).unwrap());
        let kept: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            kept["token_report"], false,
            "an explicit false must never be overwritten"
        );

        // No config at all is not an error, and must not create one.
        let missing = dir.path().join("nope.json");
        assert!(!ensure_token_report_field_at(&missing).unwrap());
        assert!(!missing.exists());
    }

    #[test]
    fn expose_token_report_switch_reports_broken_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker-config.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(ensure_token_report_field_at(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}

pub fn config_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(crate::config::APP_DIR_NAME)
        .join("tracker-config.json")
}

pub fn load_config() -> Option<TrackerConfig> {
    let path = config_path();
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str::<TrackerConfig>(&raw).ok()
}

pub fn save_config(config: &TrackerConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(&path, json).map_err(|e| format!("Failed to write config: {}", e))?;
    Ok(())
}

/// Write the `token_report` key into the config file if it is absent.
///
/// Reading defaults a missing key to `true`, so reporting already works without
/// this — but the field is meant to be user-discoverable, so the file should
/// show it. Only a *missing* key is added: an existing value (including a
/// deliberately disabled `false`) is never overwritten, which is why this works
/// on the raw JSON instead of going through `TrackerConfig` and re-serializing.
pub fn ensure_token_report_field() -> Result<bool, String> {
    ensure_token_report_field_at(&config_path())
}

fn ensure_token_report_field_at(path: &std::path::Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(path).map_err(|e| format!("Failed to read config: {}", e))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Failed to parse config: {}", e))?;
    let Some(map) = value.as_object_mut() else {
        return Err("Config file is not a JSON object".to_string());
    };
    if map.contains_key("token_report") {
        return Ok(false);
    }
    map.insert(
        "token_report".to_string(),
        serde_json::Value::Bool(default_token_report()),
    );
    let json = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(path, json).map_err(|e| format!("Failed to write config: {}", e))?;
    Ok(true)
}

pub fn add_to_blacklist(pattern: &str) -> Result<(), String> {
    let mut config = load_config().ok_or("Config file not found")?;
    if config.blacklist.contains(&pattern.to_string()) {
        return Err(format!("Pattern '{}' already in blacklist", pattern));
    }
    config.blacklist.push(pattern.to_string());
    save_config(&config)?;
    Ok(())
}

pub fn remove_from_blacklist(pattern: &str) -> Result<(), String> {
    let mut config = load_config().ok_or("Config file not found")?;
    let original_len = config.blacklist.len();
    config.blacklist.retain(|p| p != pattern);
    if config.blacklist.len() == original_len {
        return Err(format!("Pattern '{}' not found in blacklist", pattern));
    }
    save_config(&config)?;
    Ok(())
}

pub fn list_blacklist() -> Result<Vec<String>, String> {
    let config = load_config().ok_or("Config file not found")?;
    Ok(config.blacklist)
}
