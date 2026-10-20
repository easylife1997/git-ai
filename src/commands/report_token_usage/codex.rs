//! Read token usage from Codex's local data — per-turn parsing.
//!
//! Data source: `~/.codex/sessions/**/*.jsonl` — per-turn JSONL session logs.
//!
//! Each turn is identified by `task_started` / `task_complete` events sharing
//! the same `turn_id`. User messages, assistant responses, tool calls, and
//! token counts are all associated with a turn_id.

use super::TokenUsageData;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Per-turn data parsed from a Codex JSONL file.
struct CodexTurn {
    model: String,
    user_content: String,
    assistant_text: String,
    tool_uses_json: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    total_tokens: i64,
    legacy_turn_index: i64,
    /// RFC3339 (UTC) timestamp of the `task_started` event.
    started_at: Option<String>,
    /// RFC3339 (UTC) timestamp of the `task_complete` event.
    completed_at: Option<String>,
}

/// Parse all turns from a Codex JSONL file content.
fn parse_turns_from_content(content: &str) -> Vec<CodexTurn> {
    // Rollout response items don't carry turn_id; the current turn is defined
    // by task_started/task_complete boundaries.
    let mut user_messages: HashMap<String, String> = HashMap::new();
    let mut assistant_messages: HashMap<String, Vec<String>> = HashMap::new();
    let mut token_counts: HashMap<String, (i64, i64, i64, i64, i64)> = HashMap::new();
    let mut models: HashMap<String, String> = HashMap::new();
    let mut tool_uses: HashMap<String, Vec<Value>> = HashMap::new();
    let mut turn_order: Vec<String> = Vec::new();
    let mut turn_started_at: HashMap<String, String> = HashMap::new();
    let mut turn_completed_at: HashMap<String, String> = HashMap::new();
    let mut aborted_turns: HashMap<String, bool> = HashMap::new();
    let mut active_turn: Option<String> = None;
    let mut legacy_user_messages: HashMap<String, String> = HashMap::new();
    let mut legacy_final_answers: HashMap<String, String> = HashMap::new();
    let mut final_answers: HashMap<String, String> = HashMap::new();
    let mut legacy_turn_order: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let line_ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());

        match event_type {
            "event_msg" => {
                let payload = v.get("payload");
                let payload_type = payload
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                match payload_type {
                    "task_started" => {
                        if let Some(turn_id) = payload
                            .and_then(|p| p.get("turn_id"))
                            .and_then(|t| t.as_str())
                        {
                            if !turn_order.iter().any(|known| known == turn_id) {
                                turn_order.push(turn_id.to_string());
                            }
                            legacy_turn_order.push(turn_id.to_string());
                            active_turn = Some(turn_id.to_string());
                            if let Some(ts) = &line_ts {
                                turn_started_at
                                    .entry(turn_id.to_string())
                                    .or_insert_with(|| ts.clone());
                            }
                        }
                    }
                    "user_message" => {
                        let turn_id = payload
                            .and_then(|p| p.get("turn_id"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let message = payload
                            .and_then(|p| p.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("");

                        if !message.is_empty()
                            && !message.starts_with("# Context from my IDE setup")
                        {
                            if !turn_id.is_empty() {
                                legacy_user_messages
                                    .insert(turn_id.to_string(), message.to_string());
                            }
                        }
                    }
                    "agent_message" => {
                        let turn_id = payload
                            .and_then(|p| p.get("turn_id"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let phase = payload
                            .and_then(|p| p.get("phase"))
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        let message = payload
                            .and_then(|p| p.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("");

                        // Only take final_answer, which is the definitive response
                        if phase == "final_answer" && !message.is_empty() && !turn_id.is_empty() {
                            legacy_final_answers.insert(turn_id.to_string(), message.to_string());
                        }
                    }
                    "token_count" => {
                        // Extract turn_id from the event
                        // Codex token_count events don't always have turn_id directly,
                        // but we can infer from context. Use the latest turn.
                        if let Some(info) = payload.and_then(|p| p.get("info")) {
                            if let Some(last_usage) = info.get("last_token_usage") {
                                let input = last_usage
                                    .get("input_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                                let output = last_usage
                                    .get("output_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                                let cached = last_usage
                                    .get("cached_input_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                                let total = last_usage
                                    .get("total_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);

                                // token_count is emitted when the active turn completes.
                                if let Some(turn_id) = active_turn.as_ref() {
                                    let counts = token_counts.entry(turn_id.clone()).or_default();
                                    counts.0 += input;
                                    counts.1 += output;
                                    counts.2 += cached;
                                    counts.4 += total;
                                }
                            }
                        }
                    }
                    "task_complete" => {
                        if let Some(turn_id) = payload
                            .and_then(|p| p.get("turn_id"))
                            .and_then(|t| t.as_str())
                        {
                            if active_turn.as_deref() == Some(turn_id) {
                                active_turn = None;
                            }
                            if let Some(ts) = &line_ts {
                                turn_completed_at
                                    .entry(turn_id.to_string())
                                    .or_insert_with(|| ts.clone());
                            }
                        }
                    }
                    "turn_aborted" => {
                        let turn_id = payload
                            .and_then(|p| p.get("turn_id"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        if !turn_id.is_empty() {
                            aborted_turns.insert(turn_id.to_string(), true);
                        }
                    }
                    _ => {}
                }
            }
            "turn_context" => {
                let payload = v.get("payload");
                let turn_id = payload
                    .and_then(|p| p.get("turn_id"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let model = payload
                    .and_then(|p| p.get("model"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");

                if !turn_id.is_empty() && !model.is_empty() {
                    models.insert(turn_id.to_string(), model.to_string());
                }

                // Older rollout files may only expose turn_context boundaries.
                if !turn_id.is_empty() && !turn_order.iter().any(|known| known == turn_id) {
                    turn_order.push(turn_id.to_string());
                    active_turn = Some(turn_id.to_string());
                }
                if !turn_id.is_empty() && !legacy_user_messages.contains_key(turn_id) {
                    legacy_turn_order.push(turn_id.to_string());
                }
            }
            "response_item" => {
                let payload = v.get("payload");
                let payload_type = payload
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                if payload_type == "message" {
                    if let (Some(turn_id), Some(role), Some(blocks)) = (
                        active_turn.as_ref(),
                        payload.and_then(|p| p.get("role")).and_then(Value::as_str),
                        payload
                            .and_then(|p| p.get("content"))
                            .and_then(Value::as_array),
                    ) {
                        let text = blocks
                            .iter()
                            .filter_map(|block| {
                                let block_type = block.get("type").and_then(Value::as_str)?;
                                let expected = match role {
                                    "user" => "input_text",
                                    "assistant" => "output_text",
                                    _ => return None,
                                };
                                (block_type == expected)
                                    .then(|| block.get("text").and_then(Value::as_str))
                                    .flatten()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.trim().is_empty() {
                            match role {
                                "user"
                                    if !text.starts_with("<")
                                        && !text.starts_with("# Context from my IDE setup") =>
                                {
                                    user_messages
                                        .entry(turn_id.clone())
                                        .and_modify(|existing| {
                                            existing.push('\n');
                                            existing.push_str(&text);
                                        })
                                        .or_insert(text);
                                }
                                "assistant" => {
                                    if payload.and_then(|p| p.get("phase")).and_then(Value::as_str)
                                        == Some("final_answer")
                                    {
                                        final_answers.insert(turn_id.clone(), text.clone());
                                    }
                                    assistant_messages
                                        .entry(turn_id.clone())
                                        .or_default()
                                        .push(text);
                                }
                                _ => {}
                            }
                        }
                    }
                } else if matches!(payload_type, "function_call" | "custom_tool_call") {
                    let name = payload
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let call_id = payload
                        .and_then(|p| p.get("call_id"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let arguments_str = payload
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");

                    let arguments: Value = if payload_type == "custom_tool_call" {
                        Value::String(
                            payload
                                .and_then(|p| p.get("input"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        )
                    } else {
                        serde_json::from_str(arguments_str)
                            .unwrap_or(Value::String(arguments_str.to_string()))
                    };

                    let tool_entry = serde_json::json!({
                        "name": name,
                        "id": call_id,
                        "arguments": arguments,
                    });

                    if let Some(turn_id) = active_turn.as_ref() {
                        tool_uses
                            .entry(turn_id.clone())
                            .or_default()
                            .push(tool_entry);
                    }
                } else if matches!(
                    payload_type,
                    "function_call_output" | "custom_tool_call_output"
                ) {
                    if let (Some(turn_id), Some(call_id)) = (
                        active_turn.as_ref(),
                        payload
                            .and_then(|p| p.get("call_id"))
                            .and_then(Value::as_str),
                    ) {
                        if let Some(entry) = tool_uses.get_mut(turn_id).and_then(|list| {
                            list.iter_mut().rev().find(|entry| {
                                entry.get("id").and_then(Value::as_str) == Some(call_id)
                            })
                        }) {
                            entry["result"] = payload
                                .and_then(|p| p.get("output"))
                                .cloned()
                                .unwrap_or(Value::Null);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Build turns from collected data
    let mut turns = Vec::new();
    for turn_id in &turn_order {
        // Skip aborted turns
        if aborted_turns.contains_key(turn_id) {
            continue;
        }

        let user_content = user_messages
            .get(turn_id)
            .or_else(|| legacy_user_messages.get(turn_id))
            .cloned()
            .unwrap_or_default();
        let assistant_text = final_answers
            .get(turn_id)
            .cloned()
            .or_else(|| {
                assistant_messages
                    .get(turn_id)
                    .map(|parts| parts.join("\n\n"))
            })
            .or_else(|| legacy_final_answers.get(turn_id).cloned())
            .unwrap_or_default();
        let (input, output, cache_read, cache_create, total) = token_counts
            .get(turn_id)
            .cloned()
            .unwrap_or((0, 0, 0, 0, 0));
        let model = models
            .get(turn_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        // Build tool_uses JSON
        let tool_uses_json = tool_uses.get(turn_id).and_then(|list| {
            if list.is_empty() {
                None
            } else {
                // Add order field
                let ordered: Vec<Value> = list
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let mut obj = entry.as_object().cloned().unwrap_or_default();
                        obj.insert("order".to_string(), Value::Number((i + 1).into()));
                        Value::Object(obj)
                    })
                    .collect();
                serde_json::to_string(&ordered).ok()
            }
        });

        // Skip turns with no data
        if user_content.is_empty() && total == 0 {
            continue;
        }

        // Codex input_tokens includes cached, need to subtract
        let non_cached_input = input.saturating_sub(cache_read);

        turns.push(CodexTurn {
            model,
            user_content,
            assistant_text,
            tool_uses_json,
            input_tokens: non_cached_input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_create,
            total_tokens: total,
            legacy_turn_index: legacy_turn_order
                .iter()
                .rposition(|id| id == turn_id)
                .map(|index| index as i64 + 1)
                .unwrap_or(turns.len() as i64 + 1),
            started_at: turn_started_at.get(turn_id).cloned(),
            completed_at: turn_completed_at.get(turn_id).cloned(),
        });
    }

    turns
}

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

/// Find the latest JSONL session file in `~/.codex/sessions/`.
fn find_latest_session() -> Option<(PathBuf, String)> {
    let sessions_dir =
        crate::commands::checkpoint_agent::agent_presets::CodexPreset::codex_home_dir()
            .join("sessions");
    if !sessions_dir.exists() {
        return None;
    }

    let mut latest: Option<(String, PathBuf)> = None;

    fn walk(dir: &std::path::Path, latest: &mut Option<(String, PathBuf)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, latest);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Some(start) = stem.strip_prefix("rollout-") {
                            if start.len() >= 19 {
                                let ts_part = &start[..19];
                                let sortable = format!(
                                    "{}:{}:{}",
                                    &ts_part[..10],
                                    &ts_part[11..13],
                                    &ts_part[14..16],
                                );
                                let full_sort = if ts_part.len() >= 19 {
                                    format!("{}:{}", sortable, &ts_part[17..19])
                                } else {
                                    sortable
                                };
                                match latest {
                                    None => *latest = Some((full_sort, path)),
                                    Some((best_ts, _)) if full_sort > *best_ts => {
                                        *latest = Some((full_sort, path));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    walk(&sessions_dir, &mut latest);

    let Some((_, path)) = latest else {
        return None;
    };

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| {
            if let Some(dash_pos) = stem.find("-019") {
                let uuid_start = &stem[dash_pos + 1..];
                if uuid_start.len() >= 36 {
                    return Some(uuid_start[..36].to_string());
                }
            }
            Some(stem.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    Some((path, session_id))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A successful first report fixes the numbering scheme for the entire session.
/// Existing pre-fix sessions have no marker; their old indices started at 2
/// when task_started and turn_context both registered the same turn.
fn uses_legacy_indices(state: &HashMap<String, i64>, path: &str, turns: &[CodexTurn]) -> bool {
    let marker = format!("codex-index-mode:{path}");
    if let Some(mode) = state.get(&marker) {
        return *mode == 2;
    }
    match (state.get(path), turns.first()) {
        (Some(&1), Some(first)) if first.legacy_turn_index != 1 => false,
        (Some(_), _) => true,
        _ => false,
    }
}

/// Parse all turns from the latest Codex session.
///
/// Returns (jsonl_file_path, turns_with_indices_assigned).
pub fn parse_turns() -> Result<Option<(String, Vec<TokenUsageData>)>, String> {
    let (path, session_id) =
        find_latest_session().ok_or("No Codex session logs found in ~/.codex/sessions/")?;
    parse_turns_from_path(&path, &session_id)
}

/// Read the exact rollout identified by a Stop event, avoiding cross-session selection.
pub fn parse_turns_from_path(
    path: &std::path::Path,
    session_id: &str,
) -> Result<Option<(String, Vec<TokenUsageData>)>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let parsed = parse_turns_from_content(&content);

    if parsed.is_empty() {
        return Ok(None);
    }

    let path_str = path.to_string_lossy().to_string();
    // Preserve old rollout indices for sessions that were already reported by
    // the parser which counted task_started and turn_context separately.
    let state = super::load_reported_turns();
    let use_legacy_indices = uses_legacy_indices(&state, &path_str, &parsed);

    let mut turns_data = Vec::new();
    for (i, turn) in parsed.iter().enumerate() {
        turns_data.push(TokenUsageData {
            session_id: session_id.to_string(),
            turn_index: if use_legacy_indices {
                turn.legacy_turn_index
            } else {
                (i + 1) as i64
            },
            model: turn.model.clone(),
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_creation_tokens: turn.cache_creation_tokens,
            total_tokens: turn.total_tokens,
            cost_usd: None,
            repo_url: None,
            project_name: None,
            user_prompts: if turn.user_content.is_empty() {
                None
            } else {
                Some(turn.user_content.clone())
            },
            assistant_responses: if turn.assistant_text.is_empty() {
                None
            } else {
                Some(turn.assistant_text.clone())
            },
            tool_uses: turn.tool_uses_json.clone(),
            turn_started_at: turn.started_at.clone(),
            turn_completed_at: turn.completed_at.clone(),
        });
    }

    tracing::debug!(
        "report-token-usage: parsed {} turns from Codex session {}",
        turns_data.len(),
        session_id
    );

    Ok(Some((path_str, turns_data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_rollout_uses_hook_session_id_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selected.jsonl");
        fs::write(&path, rollout(&[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"selected"}}),
            json!({"type":"turn_context","payload":{"turn_id":"selected","model":"selected-model"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"cached_input_tokens":0}}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"selected"}}),
        ])).unwrap();
        let (key, turns) = parse_turns_from_path(&path, "hook-session-id")
            .unwrap()
            .unwrap();
        assert_eq!(key, path.to_string_lossy());
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_id, "hook-session-id");
        assert_eq!(turns[0].model, "selected-model");
        assert_eq!(turns[0].total_tokens, 120);
    }

    fn rollout(events: &[Value]) -> String {
        events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn modern_rollout_associates_messages_and_tool_results_with_each_turn() {
        let content = rollout(&[
            json!({"timestamp":"2026-02-11T05:53:33.340Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first prompt"}]}}),
            json!({"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-test"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"duplicate UI event"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{\"command\":\"pwd\"}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"/tmp"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first answer"}],"phase":"final_answer"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":25,"output_tokens":10,"total_tokens":110}}}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":40,"cached_input_tokens":10,"output_tokens":5,"total_tokens":45}}}}),
            json!({"timestamp":"2026-02-11T05:53:36.420Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}),
            json!({"timestamp":"2026-02-11T05:53:36.500Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t2"}}),
            json!({"type":"turn_context","payload":{"turn_id":"t2","model":"gpt-test"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"second prompt"}]}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"c2","input":"patch"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c2","output":"done"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second answer"}]}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20,"total_tokens":220}}}}),
            json!({"timestamp":"2026-02-11T05:53:40.100Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t2"}}),
        ]);
        let turns = parse_turns_from_content(&content);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user_content, "first prompt");
        assert_eq!(turns[0].assistant_text, "first answer");
        assert_eq!(turns[0].model, "gpt-test");
        assert_eq!(turns[0].legacy_turn_index, 2);
        assert_eq!(
            (
                turns[0].input_tokens,
                turns[0].cache_read_tokens,
                turns[0].total_tokens
            ),
            (105, 35, 155)
        );
        let tools: Value = serde_json::from_str(turns[0].tool_uses_json.as_ref().unwrap()).unwrap();
        assert_eq!(tools[0]["arguments"]["command"], "pwd");
        assert_eq!(tools[0]["result"], "/tmp");
        assert_eq!(turns[1].user_content, "second prompt");
        assert_eq!(turns[1].legacy_turn_index, 4);
        assert_eq!(turns[1].assistant_text, "second answer");
        assert_eq!(turns[1].input_tokens, 150);
        let tools: Value = serde_json::from_str(turns[1].tool_uses_json.as_ref().unwrap()).unwrap();
        assert_eq!(tools[0]["name"], "apply_patch");
        assert_eq!(tools[0]["arguments"], "patch");
        assert_eq!(tools[0]["result"], "done");
        // Native timing surfaces from task_started / task_complete event timestamps.
        assert_eq!(
            turns[0].started_at.as_deref(),
            Some("2026-02-11T05:53:33.340Z")
        );
        assert_eq!(
            turns[0].completed_at.as_deref(),
            Some("2026-02-11T05:53:36.420Z")
        );
        assert_eq!(
            turns[1].started_at.as_deref(),
            Some("2026-02-11T05:53:36.500Z")
        );
        assert_eq!(
            turns[1].completed_at.as_deref(),
            Some("2026-02-11T05:53:40.100Z")
        );
    }

    #[test]
    fn numbering_mode_stays_fixed_after_first_report() {
        let turns = parse_turns_from_content(&rollout(&[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"turn_context","payload":{"turn_id":"one"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
            json!({"type":"turn_context","payload":{"turn_id":"two"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}}),
        ]));
        let path = "/tmp/rollout.jsonl";
        let mut state = HashMap::new();
        assert!(!uses_legacy_indices(&state, path, &turns));
        state.insert(path.to_string(), 1);
        assert!(!uses_legacy_indices(&state, path, &turns));
        state.insert(format!("codex-index-mode:{path}"), 1);
        state.insert(path.to_string(), 4);
        assert!(!uses_legacy_indices(&state, path, &turns));
        state.insert(format!("codex-index-mode:{path}"), 2);
        assert!(uses_legacy_indices(&state, path, &turns));
        state.remove(&format!("codex-index-mode:{path}"));
        assert!(uses_legacy_indices(&state, path, &turns));
    }

    #[test]
    fn aborted_turn_does_not_mix_following_messages() {
        let content = rollout(&[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"aborted"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"aborted prompt"}]}}),
            json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"aborted"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"next"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"next prompt"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"next answer"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"next"}}),
        ]);
        let turns = parse_turns_from_content(&content);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_content, "next prompt");
        assert_eq!(turns[0].assistant_text, "next answer");
    }

    #[test]
    fn legacy_rollout_uses_explicit_turn_ids_without_response_items() {
        let content = rollout(&[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","turn_id":"old","message":"legacy prompt"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","turn_id":"old","phase":"final_answer","message":"legacy answer"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        ]);
        let turns = parse_turns_from_content(&content);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_content, "legacy prompt");
        assert_eq!(turns[0].assistant_text, "legacy answer");
    }
}
