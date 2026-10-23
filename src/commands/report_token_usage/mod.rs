//! Report token usage from AI coding sessions to the tracker server.
//!
//! Triggered by `Stop` hooks in Claude Code and Codex, and by the OpenCode
//! plugin's `session.idle` event, after a turn ends. Reads the latest session
//! data from each platform's local store and uploads token usage statistics.

pub mod claude;
pub mod codex;
pub mod opencode;

use crate::commands::tracker::config as tracker_config;
use crate::commands::tracker::log::{LogKind, LogStatus, ReportOutcome, append_log};
use crate::mdm::utils::home_dir;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, Read};
use std::time::Duration;

const API_PATH: &str = "/ai-code-boost/open/report/token/usage";

/// Delay to wait for JSONL file to be fully written after Stop hook triggers.
/// This addresses the race condition where Stop hook fires before JSONL is complete.
const JSONL_WRITE_DELAY_MS: u64 = 500;

/// Stop hook stdin payload from Claude Code.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopHookPayload {
    session_id: String,
    transcript_path: String,
    cwd: String,
    #[serde(default)]
    hook_event_name: String,
}

/// Token usage payload sent to the tracker server.
#[derive(Debug, Serialize)]
pub struct TokenUsagePayload {
    pub team_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_key: Option<String>,
    pub platform: String,
    pub session_id: String,
    pub turn_index: i64,
    pub model: String,
    pub username: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_prompts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_responses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_completed_at: Option<String>,
    pub reported_at: String,
}

/// Result of reading token usage from a platform's local data.
pub struct TokenUsageData {
    pub session_id: String,
    pub turn_index: i64,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub cost_usd: Option<f64>,
    pub repo_url: Option<String>,
    pub project_name: Option<String>,
    pub user_prompts: Option<String>,
    pub assistant_responses: Option<String>,
    pub tool_uses: Option<String>,
    /// RFC3339 (UTC+8): when this turn's user message was created. `None` when
    /// the platform does not expose native timing.
    pub turn_started_at: Option<String>,
    /// RFC3339 (UTC+8): when this turn's final answer completed. `None` when
    /// the platform does not expose native timing.
    pub turn_completed_at: Option<String>,
}

/// Get the current user's git email for attribution.
fn get_git_username() -> String {
    // Try local, then global git config
    for scope in &["--local", "--global"] {
        if let Ok(output) = std::process::Command::new("git")
            .args([scope, "user.email"])
            .output()
        {
            if output.status.success() {
                let email = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !email.is_empty() {
                    return email;
                }
            }
        }
    }
    // Fallback to username from USER env var
    if let Ok(username) = std::env::var("USER") {
        if !username.is_empty() {
            return username;
        }
    }
    "unknown".to_string()
}

/// Get current repo URL if in a git repository.
fn get_repo_url() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Upload token usage to the tracker server.
///
/// Returns the parsed server outcome: a 2xx response only proves the request
/// arrived, and the server uses 200 even for "data did not grow, skipped".
fn upload_usage(
    config: &tracker_config::TrackerConfig,
    usage: &TokenUsagePayload,
) -> Result<ReportOutcome, String> {
    let payload_str = serde_json::to_string(usage).map_err(|e| e.to_string())?;
    let url = format!("{}{}", config.tracker_url, API_PATH);

    let agent = crate::http::build_agent(None);
    let request = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("X-Team-Key", &config.team_key);

    let response = crate::http::send_with_body(request, &payload_str)?;

    if response.status_code >= 200 && response.status_code < 300 {
        tracing::debug!(
            "token usage reported successfully: session={} platform={}",
            usage.session_id,
            usage.platform
        );
        Ok(parse_token_response(
            response.as_str().unwrap_or_default(),
            ReportOutcome::from_http_status(response.status_code),
        ))
    } else {
        let mut outcome = ReportOutcome::from_http_status(response.status_code);
        outcome.message = Some(format!("HTTP {}", response.status_code));
        Ok(outcome)
    }
}

/// Extract the server outcome from a token-usage response body.
///
/// Shape: `{"code":200,"data":{"success":true,"message":"上报成功","recordId":565}}`
///
/// `message` is `上报成功` for a new row or `数据未增长，已跳过` when the server
/// had nothing new to store -- the latter is a no-op, not a failure.
fn parse_token_response(body: &str, mut outcome: ReportOutcome) -> ReportOutcome {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return outcome;
    };
    let Some(data) = value.get("data") else {
        return outcome;
    };

    let success = data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    outcome.server_success = success;
    outcome.record_id = data.get("recordId").and_then(Value::as_i64);
    outcome.message = data
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);

    // "数据未增长，已跳过" keeps the existing recordId but stores nothing new;
    // treat it as saved only when the message indicates a real write.
    outcome.saved =
        success && outcome.record_id.is_some() && outcome.message.as_deref() == Some("上报成功");

    outcome
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn token_response_requires_explicit_persistence_confirmation() {
        for (message, saved, accepted) in [
            ("上报成功", true, true),
            ("数据未增长，已跳过", false, true),
            ("未知消息", false, false),
        ] {
            let body = serde_json::json!({"data":{"success":true,"message":message,"recordId":42}});
            let result =
                parse_token_response(&body.to_string(), ReportOutcome::from_http_status(200));
            assert_eq!(result.saved, saved);
            assert_eq!(result.accepted(), accepted);
        }
        assert!(!parse_token_response("{}", ReportOutcome::from_http_status(200)).accepted());
    }

    /// The lock is what makes concurrent reporters safe: exactly one process may
    /// hold it, and it must be released on drop so the loser's retry succeeds.
    #[test]
    fn report_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_report_lock_in(dir.path()).expect("uncontended lock must be acquired");
        assert!(
            acquire_report_lock_in(dir.path()).is_none(),
            "a second reporter must not hold the lock concurrently"
        );
        drop(first);
        assert!(
            acquire_report_lock_in(dir.path()).is_some(),
            "the lock must be released when the holder finishes"
        );
    }
}

/// Serialize the read-report-write cycle across processes.
///
/// Two reporters can race for the same turn: OpenCode 1.18 loads both plugin
/// files into one process, so its 1.x export and 2.x `server()` entry each kick
/// off `report-token-usage` for the same settle event — two CLI processes, one
/// state file. Without serialization both read the same `last_reported`, both
/// pass the "new turns" filter, and the turn is uploaded twice.
///
/// Returns `None` when the lock stays busy past the timeout. The caller then
/// proceeds unlocked rather than dropping the report: the tracker is the
/// authority on duplicates (its per-commit/per-turn results distinguish
/// `inserted` from `duplicated`), so losing a report is worse than a repeat.
fn acquire_report_lock() -> Option<crate::utils::LockFile> {
    acquire_report_lock_in(&home_dir().join(crate::config::APP_DIR_NAME))
}

fn acquire_report_lock_in(state_dir: &std::path::Path) -> Option<crate::utils::LockFile> {
    let _ = std::fs::create_dir_all(state_dir);
    let lock_path = state_dir.join("reported-turns.lock");

    // A competing reporter is only ever mid-POST for a handful of milliseconds,
    // so a short bounded wait captures every real race. It has to stay short:
    // `opencode run` tears down the plugin as soon as the session goes idle, and
    // the reporter runs inside that window.
    for _ in 0..40 {
        if let Some(lock) = crate::utils::LockFile::try_acquire(&lock_path) {
            return Some(lock);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    None
}

/// Read the state file that tracks which turns have already been reported.
/// Returns a map from JSONL file path → last reported turn index.
fn load_reported_turns() -> std::collections::HashMap<String, i64> {
    let state_path = home_dir()
        .join(crate::config::APP_DIR_NAME)
        .join("reported-turns.json");
    if !state_path.exists() {
        return std::collections::HashMap::new();
    }
    let content = std::fs::read_to_string(&state_path).unwrap_or_default();
    if content.trim().is_empty() {
        return std::collections::HashMap::new();
    }
    serde_json::from_str(&content).unwrap_or_default()
}

/// Save the reported turns state file.
fn save_reported_turns(state: &std::collections::HashMap<String, i64>) {
    let state_dir = home_dir().join(crate::config::APP_DIR_NAME);
    let _ = std::fs::create_dir_all(&state_dir);
    let state_path = state_dir.join("reported-turns.json");
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&state_path, json);
    }
}

/// Try to read Stop hook stdin payload.
/// Returns None if stdin is empty or not a valid JSON.
fn try_read_stop_hook_stdin() -> Option<StopHookPayload> {
    // Check if stdin has data
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    if handle.fill_buf().ok()?.is_empty() {
        return None;
    }

    // Read stdin content
    let mut input = String::new();
    if handle.read_to_string(&mut input).is_err() || input.trim().is_empty() {
        return None;
    }

    // Parse JSON
    serde_json::from_str::<StopHookPayload>(&input).ok()
}

/// Extract the value of `--session-id <value>` / `--session-id=<value>`.
///
/// OpenCode has no per-session file to sort by, so its plugin passes the
/// session id explicitly on the `session.idle` event. Absent (manual runs),
/// the reader falls back to the most recently updated session.
fn parse_session_id_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--session-id=")
            && !value.trim().is_empty()
        {
            return Some(value.to_string());
        } else if arg == "--session-id"
            && let Some(value) = iter.next()
            && !value.trim().is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

/// Main entry point: `git-ai report-token-usage <platform>`
pub fn handle_report_token_usage(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: easylife-ai report-token-usage <platform> [--session-id <id>]");
        eprintln!("Platforms: claude-code, codex, opencode");
        std::process::exit(1);
    }

    let platform = &args[0];
    let session_id_arg = parse_session_id_arg(&args[1..]);

    // Load tracker config
    let config = match tracker_config::load_config() {
        Some(c) => c,
        None => {
            tracing::debug!("report-token-usage: tracker config not found, skipping");
            return;
        }
    };

    if !config.token_report {
        tracing::debug!("token reporting disabled by token_report=false");
        return;
    }

    // Get user identity: prefer config username, fallback to git email
    let username = config
        .username
        .as_ref()
        .filter(|u| !u.is_empty())
        .cloned()
        .unwrap_or_else(get_git_username);
    let repo_url = get_repo_url();

    // Parse team_id with proper error handling
    let team_id = match config.team_id.parse::<i64>() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                "report-token-usage: invalid team_id '{}': {}. Skipping report.",
                config.team_id,
                e
            );
            return;
        }
    };

    // Try to read Stop hook stdin (contains transcript_path)
    let stdin_payload = if platform == "claude-code" || platform == "codex" {
        try_read_stop_hook_stdin()
    } else {
        None
    };

    // If stdin contains transcript_path, wait for JSONL to be fully written
    // This fixes the race condition where Stop hook fires before JSONL is complete
    if stdin_payload.is_some() {
        tracing::debug!(
            "report-token-usage: received stdin from Stop hook, waiting {}ms for JSONL write",
            JSONL_WRITE_DELAY_MS
        );
        std::thread::sleep(Duration::from_millis(JSONL_WRITE_DELAY_MS));
    }

    // Read per-turn data from platform-specific data source.
    // The first element is the incremental-state key: a JSONL path for
    // Claude/Codex, or `opencode:<session_id>` for OpenCode.
    let (state_key, mut turns) = match platform.as_str() {
        "claude-code" => {
            // If stdin provided transcript_path, use it directly
            if let Some(payload) = &stdin_payload {
                // Parse turns from the specific JSONL file
                match claude::parse_turns_from_path(&payload.transcript_path) {
                    Ok(turns) => (payload.transcript_path.clone(), turns),
                    Err(e) => {
                        tracing::debug!(
                            "report-token-usage: failed to read Claude data from stdin path: {}",
                            e
                        );
                        return;
                    }
                }
            } else {
                match claude::parse_turns() {
                    Ok(Some((path, turns))) => (path, turns),
                    Ok(None) => {
                        tracing::debug!("report-token-usage: no Claude session data found");
                        return;
                    }
                    Err(e) => {
                        tracing::debug!("report-token-usage: failed to read Claude data: {}", e);
                        return;
                    }
                }
            }
        }
        "codex" => match if let Some(payload) = &stdin_payload {
            codex::parse_turns_from_path(
                std::path::Path::new(&payload.transcript_path),
                &payload.session_id,
            )
        } else {
            codex::parse_turns()
        } {
            Ok(Some((path, turns))) => (path, turns),
            Ok(None) => {
                tracing::debug!("report-token-usage: no Codex session data found");
                return;
            }
            Err(e) => {
                tracing::debug!("report-token-usage: failed to read Codex data: {}", e);
                return;
            }
        },
        "opencode" => {
            match opencode::parse_turns(session_id_arg.as_deref()) {
                Ok(Some((key, turns))) => (key, turns),
                Ok(None) => {
                    tracing::debug!("report-token-usage: no OpenCode session data found");
                    // Let the OpenCode plugin retry after SQLite finishes flushing.
                    std::process::exit(2);
                }
                Err(e) => {
                    tracing::debug!("report-token-usage: failed to read OpenCode data: {}", e);
                    // A transient SQLite/read error is retryable from the plugin.
                    std::process::exit(2);
                }
            }
        }
        _ => {
            eprintln!("Unknown platform: {}", platform);
            eprintln!("Supported platforms: claude-code, codex, opencode");
            std::process::exit(1);
        }
    };

    if turns.is_empty() {
        tracing::debug!("report-token-usage: no turns found for {}", platform);
        return;
    }

    // Load incremental state. Hold the lock across the whole read-report-write
    // cycle so a concurrent reporter for the same turn cannot read the stale
    // index and upload it a second time.
    let _report_lock = acquire_report_lock();
    let mut state = load_reported_turns();
    let last_reported = state.get(&state_key).copied().unwrap_or(0);

    // Filter to only unreported turns
    let new_turns: Vec<_> = turns
        .iter_mut()
        .filter(|t| t.turn_index > last_reported)
        .collect();

    if new_turns.is_empty() {
        tracing::debug!(
            "report-token-usage: no new turns to report for {}",
            platform
        );
        return;
    }

    let mut max_reported = last_reported;
    let mut reported_count = 0u64;

    for turn in &new_turns {
        let payload = TokenUsagePayload {
            team_id,
            team_key: Some(config.team_key.clone()),
            platform: platform.to_string(),
            session_id: turn.session_id.clone(),
            turn_index: turn.turn_index,
            model: turn.model.clone(),
            username: username.clone(),
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_creation_tokens: turn.cache_creation_tokens,
            total_tokens: turn.total_tokens,
            cost_usd: turn.cost_usd,
            repo_url: turn.repo_url.clone().or(repo_url.clone()),
            project_name: turn.project_name.clone(),
            user_prompts: turn.user_prompts.clone(),
            assistant_responses: turn.assistant_responses.clone(),
            tool_uses: turn.tool_uses.clone(),
            turn_started_at: turn.turn_started_at.clone(),
            turn_completed_at: turn.turn_completed_at.clone(),
            reported_at: {
                let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("valid offset");
                chrono::Utc::now().with_timezone(&offset).to_rfc3339()
            },
        };

        match upload_usage(&config, &payload) {
            Ok(outcome) => {
                tracing::debug!(
                    "token usage reported: session={} turn={}",
                    payload.session_id,
                    payload.turn_index
                );
                // One POST per turn, so one log line per turn -- that keeps the
                // per-turn recordId auditable against the server.
                let mut logged = outcome.clone();
                logged.turn = Some(payload.turn_index);
                logged.session_id = Some(payload.session_id.clone());
                append_log(
                    if logged.accepted() {
                        LogStatus::Uploaded
                    } else {
                        LogStatus::NotSaved
                    },
                    LogKind::Token,
                    &payload.session_id,
                    &payload.platform,
                    &payload.model,
                    &state_key,
                    None,
                    Some(&logged),
                );
                if !outcome.accepted() {
                    break; // Never advance past a rejected or malformed response.
                }
                max_reported = max_reported.max(payload.turn_index);
                reported_count += 1;
            }
            Err(e) => {
                tracing::debug!(
                    "report-token-usage upload failed for turn {}: {}",
                    payload.turn_index,
                    e
                );
                let logged = ReportOutcome {
                    turn: Some(payload.turn_index),
                    session_id: Some(payload.session_id.clone()),
                    message: Some(e.clone()),
                    ..Default::default()
                };
                append_log(
                    LogStatus::Failed,
                    LogKind::Token,
                    &payload.session_id,
                    &payload.platform,
                    &payload.model,
                    &state_key,
                    Some(&e),
                    Some(&logged),
                );
                break; // Stop on first failure to maintain ordering
            }
        }
    }

    // Update state file
    if max_reported > last_reported {
        if platform == "codex" {
            let marker = format!("codex-index-mode:{state_key}");
            state.entry(marker).or_insert_with(|| {
                if last_reported == 0 || (last_reported == 1 && turns[0].turn_index == 1) {
                    1 // New contiguous numbering.
                } else {
                    2 // Preserve indices reported by the previous parser.
                }
            });
        }
        state.insert(state_key, max_reported);
        save_reported_turns(&state);
    }

    if reported_count > 0 {
        // Codex interprets non-empty stdout as a structured hook response.
        // Keep diagnostics on stderr so a successful upload stays a successful hook.
        eprintln!(
            "[{} token-report] {} reported: {} turns (up to turn {})",
            crate::config::PRODUCT_NAME,
            platform,
            reported_count,
            max_reported
        );
    }
}
