//! Read token usage from Claude Code's local data — per-turn parsing.
//!
//! Data source: `~/.claude/projects/<project>/<session>.jsonl`
//!
//! Each "turn" is identified by an explicit user prompt with `promptSource: "typed"`
//! or `promptSource: "sdk"` (including Claude Code's `-p` mode).
//! A turn may contain multiple assistant messages (text + tool_use + thinking)
//! and tool_result messages.
//!
//! Token usage is per model response, identified by `message.id`. Sum all usage
//! fields across distinct responses, counting repeated content blocks for the
//! same response only once.

use super::TokenUsageData;
use crate::mdm::utils::home_dir;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A single tool use call extracted from an assistant message.
#[derive(Debug, Clone)]
struct ToolUseEntry {
    name: String,
    id: String,
    arguments: Value,
    order: usize,
}

/// A single tool result from a user message.
#[derive(Debug, Clone)]
struct ToolResultEntry {
    tool_use_id: String,
    content: String,
}

/// Per-turn data parsed from a Claude JSONL file.
pub struct ParsedTurn {
    pub session_id: String,
    pub turn_index: i64,
    pub model: String,
    pub user_timestamp: String,
    pub user_content: String,
    pub assistant_text: String,
    pub assistant_timestamp: String,
    pub tool_uses_json: Option<String>,

    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub cost_usd: Option<f64>,
    pub project_name: Option<String>,
    pub repo_url: Option<String>,
}

// ---------------------------------------------------------------------------
// JSONL line parsing
// ---------------------------------------------------------------------------

/// Minimal struct to parse type/timestamp from any JSONL line.
#[derive(Debug, Deserialize)]
struct JsonlLine {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    message: Option<Value>,
    cost_usd: Option<f64>,
    #[serde(rename = "costUSD")]
    cost_usd_alt: Option<f64>,
}

/// Maximum length for tool_result content (characters).
const MAX_TOOL_RESULT_LEN: usize = 5000;

/// Truncate a string to max_len characters, appending ...(truncated) if needed.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{}...(truncated)", truncated)
    }
}

// ---------------------------------------------------------------------------
// Turn parsing
// ---------------------------------------------------------------------------

/// Parse all turns from a Claude JSONL file.
///
/// A turn starts with an explicit `typed` or `sdk` user prompt with string content.
/// Tool results, attachments, and unmarked/system-injected messages do not start turns.
/// Everything until the next explicit user prompt belongs to that turn.
fn parse_turns_from_content(content: &str, file_path: &str) -> Vec<ParsedTurn> {
    let mut turns: Vec<ParsedTurn> = Vec::new();

    // Current turn state
    let mut in_turn = false;
    let mut turn_user_ts = String::new();
    let mut turn_user_content = String::new();
    let mut turn_assistant_texts: Vec<String> = Vec::new();
    let mut turn_assistant_ts = String::new();
    let mut turn_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut turn_tool_results: Vec<ToolResultEntry> = Vec::new();
    let mut turn_tool_use_order = 0usize;

    let mut turn_input_tokens: i64 = 0;
    let mut turn_output_tokens: i64 = 0;
    let mut turn_cache_read: i64 = 0;
    let mut turn_cache_creation: i64 = 0;
    let mut turn_model: Option<String> = None;
    let mut turn_cost: Option<f64> = None;
    let mut seen_msg_ids: HashSet<String> = HashSet::new();

    let mut session_id: Option<String> = None;
    let mut cost_accum: f64 = 0.0;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: JsonlLine = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if session_id.is_none() {
            session_id = entry.session_id.clone();
        }

        // Accumulate cost
        let line_cost = entry.cost_usd.or(entry.cost_usd_alt).unwrap_or(0.0);
        if line_cost > 0.0 {
            cost_accum += line_cost;
        }

        let Some(msg) = &entry.message else { continue };
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let ts = entry.timestamp.clone().unwrap_or_default();

        // Check message type from top-level "type" field
        let msg_type = {
            // Parse the full line to get the "type" field
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                v.get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            }
        };

        match (msg_type.as_str(), role) {
            // --- User message (real user input) ---
            ("user", "user") => {
                let content_val = msg.get("content");

                // STRING content = real user input
                if let Some(content_str) = content_val.and_then(|c| c.as_str()) {
                    // Skip context continuation
                    if content_str.starts_with("This session is being continued") {
                        continue;
                    }

                    // Require an explicit source; user-role strings alone may be
                    // system injections. Claude Code's -p mode uses "sdk".
                    let is_user_prompt = if let Ok(v) = serde_json::from_str::<Value>(line) {
                        match v.get("promptSource").and_then(|s| s.as_str()) {
                            Some("typed") => true,
                            Some("sdk") => v.get("isMeta").and_then(|m| m.as_bool()) != Some(true),
                            _ => false,
                        }
                    } else {
                        false
                    };

                    if !is_user_prompt {
                        continue;
                    }

                    // If we were already in a turn, finalize it
                    if in_turn {
                        let total = turn_input_tokens
                            + turn_output_tokens
                            + turn_cache_read
                            + turn_cache_creation;
                        if total > 0 || !turn_assistant_texts.is_empty() {
                            turns.push(build_turn(
                                &session_id,
                                &turn_user_ts,
                                &turn_user_content,
                                &turn_assistant_texts,
                                &turn_assistant_ts,
                                &turn_tool_uses,
                                &turn_tool_results,
                                turn_input_tokens,
                                turn_output_tokens,
                                turn_cache_read,
                                turn_cache_creation,
                                &turn_model,
                                turn_cost,
                                file_path,
                                cost_accum,
                            ));
                        }
                    }

                    // Start new turn
                    in_turn = true;
                    turn_user_ts = ts;
                    turn_user_content = content_str.to_string();
                    turn_assistant_texts.clear();
                    turn_assistant_ts.clear();
                    turn_tool_uses.clear();
                    turn_tool_results.clear();
                    turn_tool_use_order = 0;

                    turn_input_tokens = 0;
                    turn_output_tokens = 0;
                    turn_cache_read = 0;
                    turn_cache_creation = 0;
                    turn_model = None;
                    turn_cost = None;
                    seen_msg_ids.clear();
                } else {
                    // LIST content = tool_result messages
                    let Some(content_list) = content_val.and_then(|c| c.as_array()) else {
                        continue;
                    };

                    for item in content_list {
                        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if item_type == "tool_result" {
                            let tool_use_id = item
                                .get("tool_use_id")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();

                            let result_content = extract_tool_result_content(item);
                            let truncated = truncate_str(&result_content, MAX_TOOL_RESULT_LEN);

                            turn_tool_results.push(ToolResultEntry {
                                tool_use_id,
                                content: truncated,
                            });
                        }
                    }
                }
            }

            // --- Assistant message ---
            ("assistant", "assistant") => {
                let msg_id = msg
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();

                // Extract usage (only once per message.id, as all blocks share the same usage)
                if !msg_id.is_empty() && !seen_msg_ids.contains(&msg_id) {
                    if let Some(usage) = msg.get("usage") {
                        let input = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let cache_read = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        let cache_creation = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);

                        // Usage is per model response, not cumulative per user turn.
                        // Distinct message IDs represent separate billed API calls,
                        // even when their input counts happen to be identical.
                        turn_input_tokens += input;
                        turn_output_tokens += output;
                        turn_cache_read += cache_read;
                        turn_cache_creation += cache_creation;

                        turn_cost = Some(
                            turn_cost.unwrap_or(0.0)
                                + usage
                                    .get("output_tokens")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as f64
                                    * 0.0, // approximate, real cost from cost_usd
                        );
                    }

                    // Extract model
                    if turn_model.is_none() {
                        if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                            turn_model = Some(m.to_string());
                        }
                    }

                    seen_msg_ids.insert(msg_id);
                }

                // Extract content blocks
                let Some(content_list) = msg.get("content").and_then(|c| c.as_array()) else {
                    continue;
                };

                let stop_reason = msg
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");

                for block in content_list {
                    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match block_type {
                        "text" => {
                            // Only collect text from the final assistant message (end_turn).
                            // Intermediate tool_use turns may also contain a text block (e.g.
                            // Claude's "thinking out loud" before calling a tool), but the
                            // user-visible reply is always the end_turn text.
                            if stop_reason == "end_turn" {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    turn_assistant_texts.push(text.to_string());
                                    turn_assistant_ts = ts.clone();
                                }
                            }
                        }
                        "tool_use" => {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = block.get("input").cloned().unwrap_or(Value::Null);

                            turn_tool_use_order += 1;
                            turn_tool_uses.push(ToolUseEntry {
                                name,
                                id,
                                arguments,
                                order: turn_tool_use_order,
                            });
                        }
                        "thinking" => {
                            // Skip thinking blocks
                        }
                        _ => {}
                    }
                }
            }

            _ => {
                // Skip other message types (system, attachment, etc.)
            }
        }
    }

    // Finalize last turn
    if in_turn {
        let total = turn_input_tokens + turn_output_tokens + turn_cache_read + turn_cache_creation;
        if total > 0 || !turn_assistant_texts.is_empty() {
            turns.push(build_turn(
                &session_id,
                &turn_user_ts,
                &turn_user_content,
                &turn_assistant_texts,
                &turn_assistant_ts,
                &turn_tool_uses,
                &turn_tool_results,
                turn_input_tokens,
                turn_output_tokens,
                turn_cache_read,
                turn_cache_creation,
                &turn_model,
                turn_cost,
                file_path,
                cost_accum,
            ));
        }
    }

    turns
}

/// Extract text content from a tool_result block.
/// The content can be a string or a list of content blocks.
fn extract_tool_result_content(item: &Value) -> String {
    let content_val = item.get("content");
    match content_val {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(list)) => {
            let mut parts = Vec::new();
            for block in list {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Build a ParsedTurn from accumulated turn data.
fn build_turn(
    session_id: &Option<String>,
    user_ts: &str,
    user_content: &str,
    assistant_texts: &[String],
    assistant_ts: &str,
    tool_uses: &[ToolUseEntry],
    tool_results: &[ToolResultEntry],
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_creation: i64,
    model: &Option<String>,
    _cost: Option<f64>,
    file_path: &str,
    _cost_accum: f64,
) -> ParsedTurn {
    let total = input_tokens + output_tokens + cache_read + cache_creation;

    // Build assistant_responses: concatenate all text blocks
    let assistant_responses = if assistant_texts.is_empty() {
        None
    } else {
        Some(assistant_texts.join("\n\n"))
    };

    // Build tool_uses JSON (merged with tool_results by order)
    let tool_uses_json = if tool_uses.is_empty() {
        None
    } else {
        let arr: Vec<Value> = tool_uses
            .iter()
            .map(|tu| {
                // Find the matching tool_result by tool_use_id
                let result_content = tool_results
                    .iter()
                    .find(|tr| tr.tool_use_id == tu.id)
                    .map(|tr| Value::String(tr.content.clone()))
                    .unwrap_or(Value::Null);

                serde_json::json!({
                    "name": tu.name,
                    "id": tu.id,
                    "arguments": tu.arguments,
                    "order": tu.order,
                    "result": result_content,
                })
            })
            .collect();
        serde_json::to_string(&arr).ok()
    };

    let project_name = extract_project_name(std::path::Path::new(file_path));

    ParsedTurn {
        session_id: session_id.clone().unwrap_or_else(|| "unknown".to_string()),
        turn_index: 0, // Will be set by caller
        model: model.clone().unwrap_or_else(|| "unknown".to_string()),
        user_timestamp: user_ts.to_string(),
        user_content: user_content.to_string(),
        assistant_text: assistant_responses.unwrap_or_default(),
        assistant_timestamp: assistant_ts.to_string(),
        tool_uses_json,

        input_tokens,
        output_tokens,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        total_tokens: total,
        cost_usd: None, // Will be calculated by backend
        project_name,
        repo_url: None,
    }
}

/// Extract project name from a Claude JSONL file path.
fn extract_project_name(path: &std::path::Path) -> Option<String> {
    let components: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();

    for (i, comp) in components.iter().enumerate() {
        if comp == "projects" && i + 1 < components.len() {
            let raw = &components[i + 1];
            if let Some(rest) = raw.strip_prefix("-Users-") {
                let parts: Vec<&str> = rest.split('-').collect();
                if parts.len() >= 2 {
                    return Some(parts[1..].join("/"));
                }
            }
            return Some(raw.clone());
        }
    }
    None
}

/// Extract session ID from a JSONL file path (filename without extension).
fn extract_session_id(path: &std::path::Path) -> Option<String> {
    let components: Vec<&std::ffi::OsStr> = path.components().map(|c| c.as_os_str()).collect();

    for (i, comp) in components.iter().enumerate() {
        if comp.to_string_lossy() == "subagents" {
            if i > 0 {
                return Some(components[i - 1].to_string_lossy().to_string());
            }
        }
    }

    if let Some(file_name) = path.file_stem() {
        let name = file_name.to_string_lossy().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

/// Recursively find all .jsonl files under the projects directory.
fn find_jsonl_files(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        } else if file_type.is_dir() {
            find_jsonl_files(&path, files);
        }
    }
}

/// Find Claude project directories.
fn find_claude_project_dirs() -> Vec<std::path::PathBuf> {
    let home = home_dir();
    let mut dirs = Vec::new();

    let claude_dir = home.join(".claude").join("projects");
    if claude_dir.is_dir() {
        dirs.push(claude_dir);
    }

    let xdg_dir = home.join(".config").join("claude").join("projects");
    if xdg_dir.is_dir() {
        dirs.push(xdg_dir);
    }

    if let Ok(env_paths) = std::env::var("CLAUDE_CONFIG_DIR") {
        for raw in env_paths
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let path = std::path::PathBuf::from(raw).join("projects");
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }

    dirs
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse all turns from the latest Claude Code session.
///
/// Returns (jsonl_file_path, turns_with_indices_assigned).
pub fn parse_turns() -> Result<Option<(String, Vec<TokenUsageData>)>, String> {
    let project_dirs = find_claude_project_dirs();
    if project_dirs.is_empty() {
        tracing::debug!("report-token-usage: no Claude project directories found");
        return Ok(None);
    }

    let mut all_files: Vec<(std::path::PathBuf, i64)> = Vec::new(); // (path, mtime_ms)

    for dir in &project_dirs {
        let mut jsonl_files = Vec::new();
        find_jsonl_files(dir, &mut jsonl_files);

        for file in &jsonl_files {
            let mtime_ms = file
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            all_files.push((file.clone(), mtime_ms));
        }
    }

    if all_files.is_empty() {
        tracing::debug!("report-token-usage: no Claude session data found");
        return Ok(None);
    }

    // Pick the most recently modified file
    all_files.sort_by_key(|(_, mtime)| *mtime);
    let (latest_path, _) = all_files.last().unwrap();

    let path_str = latest_path.to_string_lossy().to_string();
    let turns = parse_turns_from_path(&path_str)?;
    if turns.is_empty() {
        return Ok(None);
    }
    Ok(Some((path_str, turns)))
}

/// Parse all turns from a specific Claude Code JSONL file path.
///
/// Used when Stop hook provides transcript_path via stdin.
/// Returns turns_with_indices_assigned.
pub fn parse_turns_from_path(path: &str) -> Result<Vec<TokenUsageData>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;

    let mut parsed = parse_turns_from_content(&content, path);

    if parsed.is_empty() {
        tracing::debug!("report-token-usage: no turns found in {}", path);
        return Ok(Vec::new());
    }

    // Assign turn_index (1-based)
    let session_id = extract_session_id(std::path::Path::new(path))
        .or_else(|| parsed.first().map(|t| t.session_id.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    let mut turns_data = Vec::new();
    for (i, turn) in parsed.iter_mut().enumerate() {
        turn.session_id = session_id.clone();
        turn.turn_index = (i + 1) as i64;

        turns_data.push(TokenUsageData {
            session_id: turn.session_id.clone(),
            turn_index: turn.turn_index,
            model: turn.model.clone(),
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_creation_tokens: turn.cache_creation_tokens,
            total_tokens: turn.total_tokens,
            cost_usd: turn.cost_usd,
            repo_url: turn.repo_url.clone(),
            project_name: turn.project_name.clone(),
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
        });
    }

    tracing::debug!(
        "report-token-usage: parsed {} turns from session {}",
        turns_data.len(),
        session_id
    );

    Ok(turns_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transcript(entries: &[Value]) -> String {
        entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prompt(source: &str, text: &str) -> Value {
        json!({
            "type": "user", "sessionId": "session",
            "promptSource": source, "timestamp": "2026-10-20T07:13:47.686Z",
            "message": {"role": "user", "content": text}
        })
    }

    fn response(id: &str, input: i64, output: i64, read: i64, creation: i64) -> Value {
        json!({
            "type": "assistant", "sessionId": "session",
            "message": {
                "role": "assistant", "id": id, "model": "claude-sonnet-4-6",
                "content": [{"type": "text", "text": "Done."}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": creation
                }
            }
        })
    }

    #[test]
    fn sdk_prompt_with_tool_result_matches_cli_usage() {
        // Reduced from a Claude Code 2.1.304 -p transcript. The CLI cost-state
        // reports inputTokens=200/outputTokens=40 for these two message IDs.
        let mut request = prompt("sdk", "Create generated.py with a function returning 42.");
        request["turnOrigin"] = json!("sdk");
        request["entrypoint"] = json!("sdk-cli");
        let mut tool_call = response("msg_verify_1", 100, 20, 0, 0);
        tool_call["message"]["stop_reason"] = json!("tool_use");
        tool_call["message"]["content"] = json!([{
            "type": "tool_use", "id": "tool_verify_1", "name": "Write",
            "input": {"file_path": "generated.py", "content": "def generated():\n    return 42\n"}
        }]);
        let content = transcript(&[
            request,
            json!({"type": "attachment", "renderedRole": "user",
                "message": {"role": "user", "content": "Automatically attached context"}}),
            tool_call,
            json!({"type": "user", "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "tool_verify_1",
                "content": "File created successfully"
            }]}}),
            response("msg_verify_2", 100, 20, 0, 0),
        ]);

        let turns = parse_turns_from_content(&content, "session.jsonl");
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(
            turn.user_content,
            "Create generated.py with a function returning 42."
        );
        assert_eq!(turn.model, "claude-sonnet-4-6");
        assert_eq!(turn.assistant_text, "Done.");
        assert_eq!(turn.input_tokens, 200);
        assert_eq!(turn.output_tokens, 40);
        assert_eq!(turn.total_tokens, 240);
        let tools: Value = serde_json::from_str(turn.tool_uses_json.as_ref().unwrap()).unwrap();
        assert_eq!(tools[0]["name"], "Write");
        assert_eq!(tools[0]["result"], "File created successfully");
    }

    #[test]
    fn sums_usage_per_distinct_model_message_not_per_content_block() {
        let first = response("msg-1", 100, 20, 30, 40);
        let mut another_block = first.clone();
        another_block["message"]["content"] = json!([{"type": "thinking", "thinking": "..."}]);
        let content = transcript(&[
            prompt("typed", "Do the work"),
            first,
            another_block,
            response("msg-2", 150, 25, 35, 45),
            prompt("typed", "Next turn"),
            response("msg-3", 10, 2, 3, 4),
        ]);

        let turns = parse_turns_from_content(&content, "session.jsonl");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].input_tokens, 250);
        assert_eq!(turns[0].output_tokens, 45);
        assert_eq!(turns[0].cache_read_tokens, 65);
        assert_eq!(turns[0].cache_creation_tokens, 85);
        assert_eq!(turns[0].total_tokens, 445);
        assert_eq!(turns[1].total_tokens, 19);
    }

    #[test]
    fn typed_session_indices_ignore_injections_and_tool_results() {
        let mut meta = prompt("sdk", "SDK metadata, not a prompt");
        meta["isMeta"] = json!(true);
        let content = transcript(&[
            prompt("typed", "First prompt"),
            response("msg-1", 100, 20, 0, 0),
            prompt("", "Unmarked system injection"),
            prompt("system", "System injection"),
            meta,
            prompt(
                "sdk",
                "This session is being continued from a previous conversation.",
            ),
            json!({"type": "user", "promptSource": "sdk", "message": {
                "role": "user", "content": [{"type": "tool_result",
                    "tool_use_id": "tool-1", "content": "Tool output"}]
            }}),
            response("msg-2", 100, 20, 0, 0),
            prompt("typed", "Second prompt"),
            response("msg-3", 10, 2, 0, 0),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, content).unwrap();

        let turns = parse_turns_from_path(path.to_str().unwrap()).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_index, 1);
        assert_eq!(turns[0].user_prompts.as_deref(), Some("First prompt"));
        assert_eq!(turns[0].total_tokens, 240);
        assert_eq!(turns[1].turn_index, 2);
        assert_eq!(turns[1].user_prompts.as_deref(), Some("Second prompt"));
        assert_eq!(turns[1].total_tokens, 12);
    }
}
