//! Read token usage from OpenCode's local data — per-turn parsing.
//!
//! Data source: OpenCode's SQLite database (`opencode.db`), tables
//! `session`, `message`, `part`.
//!
//! Turn model: a *turn* starts at a `role: "user"` message. Every assistant
//! message produced while answering that prompt carries `parentID` pointing at
//! the user message id — an agentic turn emits one assistant message per model
//! step (tool call + follow-up), so a single turn is typically many assistant
//! messages. Turn token usage is therefore the **sum over all assistant
//! messages whose `parentID` is that user message**.
//!
//! Verified against a live DB: `session.tokens_input/output/cache_read` equals
//! the sum of the same fields across all of the session's assistant messages.

use super::TokenUsageData;
use crate::commands::checkpoint_agent::opencode_preset::OpenCodePreset;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// Maximum length for a single tool result (characters), matching the Claude reader.
const MAX_TOOL_RESULT_LEN: usize = 5000;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Row from the `session` table.
#[derive(Debug, Deserialize)]
struct SessionRow {
    directory: Option<String>,
    /// JSON text, e.g. `{"id":"claude-opus-4-8","providerID":"anthropic",...}`
    model: Option<String>,
}

/// Message payload from `message.data`.
#[derive(Debug, Deserialize)]
struct MessageData {
    role: Option<String>,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    tokens: Option<MessageTokens>,
    /// Native timing metadata, e.g. `{"created":1706459830000,"completed":1706459835000}`.
    time: Option<MessageTime>,
}

/// `message.data.time` — epoch-milliseconds of message creation and (for
/// completed assistant messages) completion. Mirrors what turn-timing.mjs reads.
#[derive(Debug, Default, Deserialize)]
struct MessageTime {
    created: Option<i64>,
    completed: Option<i64>,
}

/// `message.data.tokens`.
#[derive(Debug, Deserialize)]
struct MessageTokens {
    #[serde(default)]
    input: i64,
    #[serde(default)]
    output: i64,
    #[serde(default)]
    reasoning: i64,
    cache: Option<CacheTokens>,
}

/// `message.data.tokens.cache`.
#[derive(Debug, Default, Deserialize)]
struct CacheTokens {
    #[serde(default)]
    read: i64,
    #[serde(default)]
    write: i64,
}

/// Part payload from `part.data`. Only the fields we consume are declared;
/// OpenCode uses one table for text / reasoning / tool / step-* parts.
#[derive(Debug, Deserialize)]
struct PartData {
    #[serde(rename = "type")]
    part_type: Option<String>,
    text: Option<String>,
    #[serde(rename = "callID")]
    call_id: Option<String>,
    tool: Option<String>,
    /// Present on tool parts: `{status, input, output, ...}`
    state: Option<Value>,
}

/// A message row plus its parts, kept in creation order.
struct MessageEntry {
    id: String,
    data: MessageData,
    parts: Vec<PartData>,
}

/// A single tool invocation accumulated over one turn.
struct ToolUseEntry {
    name: String,
    id: String,
    arguments: Value,
    result: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a string to `max_len` characters, appending `...(truncated)` if cut.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{}...(truncated)", truncated)
    }
}

/// OpenCode stores the session's model as a JSON object in a TEXT column.
/// Extract the model id, falling back to the raw value for older shapes.
fn parse_model_column(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(id) = v.get("id").and_then(|i| i.as_str())
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    Some(trimmed.to_string())
}

/// Resolve the origin remote URL for a session's working directory.
///
/// The backend derives `project_name` by looking up (or auto-creating) a project
/// row keyed on `repo_url`, so this must be populated whenever possible.
fn repo_url_from_directory(directory: &str) -> Option<String> {
    if directory.trim().is_empty() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["-C", directory, "remote", "get-url", "origin"])
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

/// Concatenate all `text` parts of a message.
fn collect_text(parts: &[PartData]) -> String {
    let mut texts: Vec<&str> = Vec::new();
    for part in parts {
        if part.part_type.as_deref() != Some("text") {
            continue;
        }
        if let Some(text) = part.text.as_deref()
            && !text.is_empty()
        {
            texts.push(text);
        }
    }
    texts.join("\n")
}

/// Collect tool invocations from a message's parts, in order.
fn collect_tool_uses(parts: &[PartData]) -> Vec<ToolUseEntry> {
    let mut entries = Vec::new();
    for part in parts {
        if part.part_type.as_deref() != Some("tool") {
            continue;
        }
        let state = part.state.as_ref();
        let arguments = state
            .and_then(|s| s.get("input"))
            .cloned()
            .unwrap_or(Value::Null);
        let result = state
            .and_then(|s| s.get("output"))
            .and_then(|o| o.as_str())
            .map(|s| truncate_str(s, MAX_TOOL_RESULT_LEN));

        entries.push(ToolUseEntry {
            name: part.tool.clone().unwrap_or_default(),
            id: part.call_id.clone().unwrap_or_default(),
            arguments,
            result,
        });
    }
    entries
}

// ---------------------------------------------------------------------------
// SQLite loading
// ---------------------------------------------------------------------------

/// Newest `(id, time_updated)` in `table`, or `None` when the table is missing
/// or empty. `table` is always a compile-time constant, never user input.
fn newest_session_in(conn: &Connection, table: &str) -> Option<(String, i64)> {
    let sql = format!(
        "SELECT id, time_updated FROM {} ORDER BY time_updated DESC LIMIT 1",
        table
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let mut rows = stmt.query([]).ok()?;
    let row = rows.next().ok()??;
    Some((
        row.get::<_, String>(0).ok()?,
        row.get::<_, i64>(1).unwrap_or(0),
    ))
}

/// Find the most recently updated session id (manual-invocation fallback),
/// across both the 1.x (`session`) and 2.x (`session_v2`) tables.
fn latest_session_id(conn: &Connection) -> Result<Option<String>, String> {
    let v1 = newest_session_in(conn, "session");
    let v2 = newest_session_in(conn, "session_v2");
    Ok(match (v1, v2) {
        (Some(a), Some(b)) => Some(if b.1 >= a.1 { b.0 } else { a.0 }),
        (Some(a), None) => Some(a.0),
        (None, Some(b)) => Some(b.0),
        (None, None) => None,
    })
}

/// Load the session row. Returns None when the session does not exist.
fn load_session(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>, String> {
    let mut stmt = conn
        .prepare("SELECT directory, model FROM session WHERE id = ? LIMIT 1")
        .map_err(|e| format!("prepare session query failed: {}", e))?;

    let mut rows = stmt
        .query([session_id])
        .map_err(|e| format!("session query failed: {}", e))?;

    match rows.next().map_err(|e| format!("row read failed: {}", e))? {
        Some(row) => {
            let directory: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            Ok(Some(SessionRow { directory, model }))
        }
        None => Ok(None),
    }
}

/// Load all messages for a session in creation order.
fn load_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<(String, MessageData)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, data FROM message WHERE session_id = ? \
             ORDER BY time_created ASC, id ASC",
        )
        .map_err(|e| format!("prepare message query failed: {}", e))?;

    let mut rows = stmt
        .query([session_id])
        .map_err(|e| format!("message query failed: {}", e))?;

    let mut messages = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("row read failed: {}", e))? {
        let id: String = row.get(0).map_err(|e| format!("id read failed: {}", e))?;
        let data_text: String = row.get(1).map_err(|e| format!("data read failed: {}", e))?;

        match serde_json::from_str::<MessageData>(&data_text) {
            Ok(data) => messages.push((id, data)),
            Err(_) => continue,
        }
    }

    Ok(messages)
}

/// Load all parts for a session, grouped by message id, each group in creation order.
fn load_parts(
    conn: &Connection,
    session_id: &str,
) -> Result<HashMap<String, Vec<PartData>>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT message_id, data FROM part WHERE session_id = ? \
             ORDER BY time_created ASC, id ASC",
        )
        .map_err(|e| format!("prepare part query failed: {}", e))?;

    let mut rows = stmt
        .query([session_id])
        .map_err(|e| format!("part query failed: {}", e))?;

    let mut grouped: HashMap<String, Vec<PartData>> = HashMap::new();
    while let Some(row) = rows.next().map_err(|e| format!("row read failed: {}", e))? {
        let message_id: String = row
            .get(0)
            .map_err(|e| format!("message_id read failed: {}", e))?;
        let data_text: String = row.get(1).map_err(|e| format!("data read failed: {}", e))?;

        if let Ok(part) = serde_json::from_str::<PartData>(&data_text) {
            grouped.entry(message_id).or_default().push(part);
        }
    }

    Ok(grouped)
}

// ---------------------------------------------------------------------------
// Turn construction
// ---------------------------------------------------------------------------

/// Accumulator for one turn while walking the message list.
#[derive(Default)]
struct TurnAccumulator {
    user_content: String,
    assistant_entries: Vec<usize>, // indices into the assembled message list
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_creation: i64,
    model: Option<String>,
    /// Epoch-ms when this turn's user message was created.
    started_ms: Option<i64>,
    /// Epoch-ms when the turn's final completed assistant message finished.
    completed_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// OpenCode 2.x (`session_v2` / `session_message`)
// ---------------------------------------------------------------------------
//
// 2.x keeps one JSON row per message in `session_message`, ordered by `seq`:
//   user      {text}
//   assistant {content:[text|reasoning|tool...], tokens:{input,output,reasoning,cache:{read,write}}, model:{id}}
//   system / idle  (ignored)
// A turn starts at a `user` row and owns every following `assistant` row until
// the next `user` row.

/// Extract the text of a V2 tool result: `state.content[].text`, falling back
/// to `state.metadata.output`.
fn v2_tool_result(state: &Value) -> Option<String> {
    let from_content = state
        .get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());
    from_content
        .or_else(|| {
            state
                .get("metadata")
                .and_then(|m| m.get("output"))
                .and_then(|o| o.as_str())
                .map(|s| s.to_string())
        })
        .map(|s| truncate_str(&s, MAX_TOOL_RESULT_LEN))
}

/// Parse a session stored in the 2.x tables. Returns `Ok(None)` when the
/// session is not present there (or the tables do not exist), so the caller
/// can fall back to the 1.x layout.
fn parse_turns_v2(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<Vec<TokenUsageData>>, String> {
    let session: Option<(Option<String>, Option<String>)> = conn
        .prepare("SELECT directory, model FROM session_v2 WHERE id = ? LIMIT 1")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_row([session_id], |r| Ok((r.get(0).ok(), r.get(1).ok())))
                .ok()
        });
    let Some((directory, model_col)) = session else {
        return Ok(None);
    };

    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM session_message WHERE session_id = ? \
             AND type IN ('user','assistant') ORDER BY seq ASC",
        )
        .map_err(|e| format!("prepare session_message query failed: {}", e))?;
    let mut rows = stmt
        .query([session_id])
        .map_err(|e| format!("session_message query failed: {}", e))?;

    #[derive(Default)]
    struct V2Turn {
        user: String,
        reply: String,
        tools: Vec<Value>,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        model: Option<String>,
        assistants: usize,
        started_ms: Option<i64>,
        completed_ms: Option<i64>,
    }

    let mut turns: Vec<V2Turn> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("row read failed: {}", e))? {
        let kind: String = row.get(0).map_err(|e| format!("type read failed: {}", e))?;
        let text: String = row.get(1).map_err(|e| format!("data read failed: {}", e))?;
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };

        if kind == "user" {
            turns.push(V2Turn {
                user: data
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                started_ms: data
                    .get("time")
                    .and_then(|t| t.get("created"))
                    .and_then(|v| v.as_i64()),
                ..Default::default()
            });
            continue;
        }

        // Assistant rows before any user row cannot be attributed to a turn.
        let Some(turn) = turns.last_mut() else {
            continue;
        };
        turn.assistants += 1;

        if let Some(completed) = data
            .get("time")
            .and_then(|t| t.get("completed"))
            .and_then(|v| v.as_i64())
        {
            turn.completed_ms = Some(completed);
        }

        if let Some(t) = data.get("tokens") {
            let n = |v: Option<&Value>| v.and_then(|x| x.as_i64()).unwrap_or(0);
            turn.input += n(t.get("input"));
            // Reasoning is billed as output, matching the 1.x reader.
            turn.output += n(t.get("output")) + n(t.get("reasoning"));
            turn.cache_read += n(t.get("cache").and_then(|c| c.get("read")));
            turn.cache_write += n(t.get("cache").and_then(|c| c.get("write")));
        }
        if let Some(id) = data
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(|i| i.as_str())
            .filter(|s| !s.is_empty())
        {
            turn.model = Some(id.to_string());
        }

        let mut last_text = String::new();
        if let Some(items) = data.get("content").and_then(|c| c.as_array()) {
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str())
                            && !t.is_empty()
                        {
                            last_text = t.to_string();
                        }
                    }
                    Some("tool") => {
                        let state = item.get("state").cloned().unwrap_or(Value::Null);
                        turn.tools.push(serde_json::json!({
                            "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "id": item.get("id").and_then(|n| n.as_str()).unwrap_or(""),
                            "arguments": state.get("input").cloned().unwrap_or(Value::Null),
                            "order": turn.tools.len() + 1,
                            "result": v2_tool_result(&state),
                        }));
                    }
                    _ => {}
                }
            }
        }
        // User-visible reply = text of the last assistant row that has any.
        if !last_text.is_empty() {
            turn.reply = last_text;
        }
    }

    let session_model = model_col.as_deref().and_then(parse_model_column);
    let repo_url = directory.as_deref().and_then(repo_url_from_directory);

    let mut result = Vec::new();
    for (pos, t) in turns.iter().enumerate() {
        let total = t.input + t.output + t.cache_read + t.cache_write;
        // Same rule as 1.x: a prompt that never reached the model is skipped,
        // while `turn_index` keeps the pre-filter position.
        if t.assistants == 0 && total == 0 {
            continue;
        }
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
        result.push(TokenUsageData {
            session_id: session_id.to_string(),
            turn_index: (pos + 1) as i64,
            model: t
                .model
                .clone()
                .or_else(|| session_model.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            input_tokens: t.input,
            output_tokens: t.output,
            cache_read_tokens: t.cache_read,
            cache_creation_tokens: t.cache_write,
            total_tokens: total,
            cost_usd: None,
            repo_url: repo_url.clone(),
            project_name: None,
            user_prompts: non_empty(&t.user),
            assistant_responses: non_empty(&t.reply),
            tool_uses: if t.tools.is_empty() {
                None
            } else {
                serde_json::to_string(&t.tools).ok()
            },
            turn_started_at: t.started_ms.and_then(epoch_ms_to_rfc3339),
            turn_completed_at: t.completed_ms.and_then(epoch_ms_to_rfc3339),
        });
    }

    tracing::debug!(
        "report-token-usage: parsed {} turns from OpenCode 2.x session {}",
        result.len(),
        session_id
    );
    Ok(Some(result))
}

/// Format native epoch-milliseconds as an RFC3339 timestamp in UTC+8, matching
/// the shape the backend expects for `turn_started_at` / `turn_completed_at`.
fn epoch_ms_to_rfc3339(epoch_ms: i64) -> Option<String> {
    use chrono::TimeZone;
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?;
    offset
        .timestamp_millis_opt(epoch_ms)
        .single()
        .map(|dt| dt.to_rfc3339())
}

/// Parse all turns for a session out of the OpenCode database at `db_path`.
fn parse_turns_from_db(db_path: &Path, session_id: &str) -> Result<Vec<TokenUsageData>, String> {
    let conn = OpenCodePreset::open_sqlite_readonly(db_path)
        .map_err(|e| format!("failed to open {}: {}", db_path.display(), e))?;

    // OpenCode 2.x stores sessions in `session_v2` / `session_message` and no
    // longer writes `message` / `part`. Prefer that layout when the session
    // lives there; everything below is the unchanged 1.x path.
    if let Some(turns) = parse_turns_v2(&conn, session_id)? {
        return Ok(turns);
    }

    let Some(session) = load_session(&conn, session_id)? else {
        return Ok(Vec::new());
    };

    let raw_messages = load_messages(&conn, session_id)?;
    if raw_messages.is_empty() {
        return Ok(Vec::new());
    }

    let mut parts_by_message = load_parts(&conn, session_id)?;

    // Assemble messages with their parts attached.
    let messages: Vec<MessageEntry> = raw_messages
        .into_iter()
        .map(|(id, data)| {
            let parts = parts_by_message.remove(&id).unwrap_or_default();
            MessageEntry { id, data, parts }
        })
        .collect();

    // First pass: locate user messages — these are the turn boundaries.
    let mut turn_of_user: HashMap<String, usize> = HashMap::new();
    let mut turns: Vec<TurnAccumulator> = Vec::new();

    for msg in messages.iter() {
        if msg.data.role.as_deref() != Some("user") {
            continue;
        }
        let turn_pos = turns.len();
        turn_of_user.insert(msg.id.clone(), turn_pos);
        let mut acc = TurnAccumulator {
            user_content: collect_text(&msg.parts),
            started_ms: msg.data.time.as_ref().and_then(|t| t.created),
            ..Default::default()
        };
        // Carry the user message's own model hint when present.
        acc.model = msg.data.model_id.clone();
        turns.push(acc);
    }

    if turns.is_empty() {
        return Ok(Vec::new());
    }

    // Second pass: attribute assistant messages (and their usage) to a turn.
    // `parentID` is authoritative; when it is absent (older OpenCode versions)
    // fall back to the most recent user message seen so far.
    let mut current_turn: Option<usize> = None;

    for (idx, msg) in messages.iter().enumerate() {
        let role = msg.data.role.as_deref().unwrap_or("");
        if role == "user" {
            current_turn = turn_of_user.get(&msg.id).copied();
            continue;
        }
        if role != "assistant" {
            continue;
        }

        let target = msg
            .data
            .parent_id
            .as_ref()
            .and_then(|pid| turn_of_user.get(pid).copied())
            .or(current_turn);

        let Some(turn_pos) = target else {
            continue;
        };

        let acc = &mut turns[turn_pos];
        acc.assistant_entries.push(idx);

        if let Some(completed) = msg.data.time.as_ref().and_then(|t| t.completed) {
            // The final completed assistant message (latest in creation order)
            // finishes the turn; later messages in the same turn overwrite it.
            acc.completed_ms = Some(completed);
        }

        if let Some(tokens) = &msg.data.tokens {
            acc.input_tokens += tokens.input;
            // Reasoning tokens are billed as output; fold them in so that the
            // reported total matches OpenCode's own `tokens.total`.
            acc.output_tokens += tokens.output + tokens.reasoning;
            if let Some(cache) = &tokens.cache {
                acc.cache_read += cache.read;
                acc.cache_creation += cache.write;
            }
        }

        if let Some(model) = msg.data.model_id.as_ref().filter(|m| !m.is_empty()) {
            acc.model = Some(model.clone());
        }
    }

    // Session-level model is the last-resort fallback.
    let session_model = session.model.as_deref().and_then(parse_model_column);

    let repo_url = session
        .directory
        .as_deref()
        .and_then(repo_url_from_directory);

    // Third pass: materialise turns.
    let mut result = Vec::new();
    for (turn_pos, acc) in turns.iter().enumerate() {
        let total = acc.input_tokens + acc.output_tokens + acc.cache_read + acc.cache_creation;

        // Skip a prompt that never reached the model (no assistant messages and
        // no usage) rather than inserting an empty record. Note `turn_index` is
        // assigned from the pre-filter position, so indices stay stable across
        // runs as long as the same turns are skipped.
        if acc.assistant_entries.is_empty() && total == 0 {
            continue;
        }

        // User-visible reply = text of the last assistant message that has any.
        let mut assistant_text = String::new();
        for &msg_idx in acc.assistant_entries.iter().rev() {
            let text = collect_text(&messages[msg_idx].parts);
            if !text.is_empty() {
                assistant_text = text;
                break;
            }
        }

        // Tool calls across the whole turn, in execution order.
        let mut tool_uses: Vec<ToolUseEntry> = Vec::new();
        for &msg_idx in &acc.assistant_entries {
            tool_uses.extend(collect_tool_uses(&messages[msg_idx].parts));
        }
        let tool_uses_json = if tool_uses.is_empty() {
            None
        } else {
            let arr: Vec<Value> = tool_uses
                .iter()
                .enumerate()
                .map(|(i, tu)| {
                    serde_json::json!({
                        "name": tu.name,
                        "id": tu.id,
                        "arguments": tu.arguments,
                        "order": i + 1,
                        "result": tu.result,
                    })
                })
                .collect();
            serde_json::to_string(&arr).ok()
        };

        let model = acc
            .model
            .clone()
            .or_else(|| session_model.clone())
            .unwrap_or_else(|| "unknown".to_string());

        result.push(TokenUsageData {
            session_id: session_id.to_string(),
            turn_index: (turn_pos + 1) as i64,
            model,
            input_tokens: acc.input_tokens,
            output_tokens: acc.output_tokens,
            cache_read_tokens: acc.cache_read,
            cache_creation_tokens: acc.cache_creation,
            total_tokens: total,
            // Cost is intentionally left to the backend, which recomputes it
            // from `model_pricing`. OpenCode's own `data.cost` is not sent.
            cost_usd: None,
            repo_url: repo_url.clone(),
            project_name: None, // backend derives this from repo_url
            user_prompts: if acc.user_content.is_empty() {
                None
            } else {
                Some(acc.user_content.clone())
            },
            assistant_responses: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_uses: tool_uses_json,
            turn_started_at: acc.started_ms.and_then(epoch_ms_to_rfc3339),
            turn_completed_at: acc.completed_ms.and_then(epoch_ms_to_rfc3339),
        });
    }

    tracing::debug!(
        "report-token-usage: parsed {} turns from OpenCode session {}",
        result.len(),
        session_id
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse all turns from an OpenCode session.
///
/// `session_id` comes from the plugin's `session.idle` event. When it is `None`
/// (manual invocation) the most recently updated session is used.
///
/// Returns `(state_key, turns)` where `state_key` is the incremental-reporting
/// key used by `reported-turns.json`.
pub fn parse_turns(
    session_id: Option<&str>,
) -> Result<Option<(String, Vec<TokenUsageData>)>, String> {
    let data_dir = OpenCodePreset::opencode_data_path()
        .map_err(|e| format!("cannot resolve OpenCode data path: {}", e))?;

    let Some(db_path) = OpenCodePreset::resolve_sqlite_db_path(&data_dir) else {
        tracing::debug!(
            "report-token-usage: no OpenCode database found under {}",
            data_dir.display()
        );
        return Ok(None);
    };

    let session_id = match session_id {
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => {
            let conn = OpenCodePreset::open_sqlite_readonly(&db_path)
                .map_err(|e| format!("failed to open {}: {}", db_path.display(), e))?;
            match latest_session_id(&conn)? {
                Some(id) => id,
                None => {
                    tracing::debug!("report-token-usage: no OpenCode sessions found");
                    return Ok(None);
                }
            }
        }
    };

    let turns = parse_turns_from_db(&db_path, &session_id)?;
    if turns.is_empty() {
        tracing::debug!(
            "report-token-usage: no turns found for OpenCode session {}",
            session_id
        );
        return Ok(None);
    }

    Ok(Some((format!("opencode:{}", session_id), turns)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a minimal OpenCode-shaped database for testing.
    fn setup_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT,
                model TEXT,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        (dir, db_path)
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        directory: Option<&str>,
        model: Option<&str>,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, directory, model, time_updated) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, directory, model, updated],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, created: i64, data: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, session_id, created, data],
        )
        .unwrap();
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        created: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, message_id, session_id, created, data],
        )
        .unwrap();
    }

    /// One user prompt answered by two assistant messages (a tool step, then the
    /// final answer). Usage must be summed across both; only the last message's
    /// text becomes the assistant response.
    #[test]
    fn test_multi_assistant_message_turn_is_summed() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(&conn, "ses_test", Some("/tmp/does-not-exist"), None, 1000);

        insert_message(
            &conn,
            "msg_u1",
            "ses_test",
            10,
            r#"{"role":"user","time":{"created":10}}"#,
        );
        insert_part(
            &conn,
            "p_u1",
            "msg_u1",
            "ses_test",
            11,
            r#"{"type":"text","text":"hello world"}"#,
        );

        // First assistant step: tool call, with usage.
        insert_message(
            &conn,
            "msg_a1",
            "ses_test",
            20,
            r#"{"role":"assistant","parentID":"msg_u1","modelID":"claude-opus-4-8","tokens":{"total":300,"input":100,"output":50,"reasoning":0,"cache":{"write":10,"read":140}}}"#,
        );
        insert_part(
            &conn,
            "p_a1",
            "msg_a1",
            "ses_test",
            21,
            r#"{"type":"tool","callID":"call_1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file.txt"}}"#,
        );

        // Second assistant step: the user-visible answer.
        insert_message(
            &conn,
            "msg_a2",
            "ses_test",
            30,
            r#"{"role":"assistant","parentID":"msg_u1","modelID":"claude-opus-4-8","time":{"created":29,"completed":40},"tokens":{"total":200,"input":60,"output":90,"reasoning":50,"cache":{"write":0,"read":0}}}"#,
        );
        insert_part(
            &conn,
            "p_a2s",
            "msg_a2",
            "ses_test",
            31,
            r#"{"type":"step-start"}"#,
        );
        insert_part(
            &conn,
            "p_a2t",
            "msg_a2",
            "ses_test",
            32,
            r#"{"type":"text","text":"done!"}"#,
        );

        let turns = parse_turns_from_db(&db_path, "ses_test").unwrap();

        assert_eq!(turns.len(), 1, "expected exactly one turn");
        let t = &turns[0];

        assert_eq!(t.turn_index, 1);
        assert_eq!(t.session_id, "ses_test");
        assert_eq!(t.model, "claude-opus-4-8");

        // input 100+60, output (50+0)+(90+50 reasoning folded in)
        assert_eq!(t.input_tokens, 160);
        assert_eq!(t.output_tokens, 190);
        assert_eq!(t.cache_read_tokens, 140);
        assert_eq!(t.cache_creation_tokens, 10);
        assert_eq!(t.total_tokens, 500);

        assert_eq!(t.user_prompts.as_deref(), Some("hello world"));
        assert_eq!(t.assistant_responses.as_deref(), Some("done!"));
        assert!(t.cost_usd.is_none());

        // Native turn timing surfaces from the user's created and the final
        // assistant's completed epoch-ms (10 -> 40, rendered UTC+8 RFC3339).
        assert_eq!(
            t.turn_started_at.as_deref(),
            Some("1970-01-01T08:00:00.010+08:00")
        );
        assert_eq!(
            t.turn_completed_at.as_deref(),
            Some("1970-01-01T08:00:00.040+08:00")
        );

        // Tool usage captured with name / id / arguments / result.
        let tools: Value = serde_json::from_str(t.tool_uses.as_deref().unwrap()).unwrap();
        let arr = tools.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "bash");
        assert_eq!(arr[0]["id"], "call_1");
        assert_eq!(arr[0]["order"], 1);
        assert_eq!(arr[0]["arguments"]["command"], "ls");
        assert_eq!(arr[0]["result"], "file.txt");
    }

    /// Reasoning parts must never leak into the assistant response.
    #[test]
    fn test_reasoning_parts_are_excluded_from_response() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(&conn, "ses_r", None, None, 1000);
        insert_message(&conn, "u", "ses_r", 10, r#"{"role":"user"}"#);
        insert_part(
            &conn,
            "pu",
            "u",
            "ses_r",
            11,
            r#"{"type":"text","text":"q"}"#,
        );
        insert_message(
            &conn,
            "a",
            "ses_r",
            20,
            r#"{"role":"assistant","parentID":"u","modelID":"m","tokens":{"input":1,"output":2,"reasoning":3,"cache":{"write":0,"read":0}}}"#,
        );
        insert_part(
            &conn,
            "pa1",
            "a",
            "ses_r",
            21,
            r#"{"type":"reasoning","text":"SECRET CHAIN OF THOUGHT"}"#,
        );
        insert_part(
            &conn,
            "pa2",
            "a",
            "ses_r",
            22,
            r#"{"type":"text","text":"the answer"}"#,
        );

        let turns = parse_turns_from_db(&db_path, "ses_r").unwrap();
        assert_eq!(turns.len(), 1);
        let resp = turns[0].assistant_responses.as_deref().unwrap();
        assert_eq!(resp, "the answer");
        assert!(!resp.contains("SECRET"));
    }

    /// Multiple turns are indexed from 1 and kept separate.
    #[test]
    fn test_multiple_turns_are_indexed_from_one() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(&conn, "ses_m", None, None, 1000);
        for (i, (uid, aid, text, inp)) in [("u1", "a1", "first", 10), ("u2", "a2", "second", 20)]
            .iter()
            .enumerate()
        {
            let base = (i as i64 + 1) * 100;
            insert_message(&conn, uid, "ses_m", base, r#"{"role":"user"}"#);
            insert_part(
                &conn,
                &format!("p{}", uid),
                uid,
                "ses_m",
                base + 1,
                &format!(r#"{{"type":"text","text":"{}"}}"#, text),
            );
            insert_message(
                &conn,
                aid,
                "ses_m",
                base + 2,
                &format!(
                    r#"{{"role":"assistant","parentID":"{}","modelID":"m","tokens":{{"input":{},"output":1,"reasoning":0,"cache":{{"write":0,"read":0}}}}}}"#,
                    uid, inp
                ),
            );
            insert_part(
                &conn,
                &format!("p{}", aid),
                aid,
                "ses_m",
                base + 3,
                &format!(r#"{{"type":"text","text":"reply-{}"}}"#, text),
            );
        }

        let turns = parse_turns_from_db(&db_path, "ses_m").unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_index, 1);
        assert_eq!(turns[0].user_prompts.as_deref(), Some("first"));
        assert_eq!(turns[0].assistant_responses.as_deref(), Some("reply-first"));
        assert_eq!(turns[0].input_tokens, 10);
        assert_eq!(turns[1].turn_index, 2);
        assert_eq!(turns[1].user_prompts.as_deref(), Some("second"));
        assert_eq!(turns[1].input_tokens, 20);
    }

    /// An assistant message with no `parentID` (older OpenCode) falls back to
    /// the most recent preceding user message.
    #[test]
    fn test_missing_parent_id_falls_back_to_latest_user_message() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(&conn, "ses_np", None, None, 1000);
        insert_message(&conn, "u1", "ses_np", 10, r#"{"role":"user"}"#);
        insert_part(
            &conn,
            "pu1",
            "u1",
            "ses_np",
            11,
            r#"{"type":"text","text":"prompt"}"#,
        );
        insert_message(
            &conn,
            "a1",
            "ses_np",
            20,
            r#"{"role":"assistant","modelID":"m","tokens":{"input":7,"output":3,"reasoning":0,"cache":{"write":0,"read":0}}}"#,
        );
        insert_part(
            &conn,
            "pa1",
            "a1",
            "ses_np",
            21,
            r#"{"type":"text","text":"ok"}"#,
        );

        let turns = parse_turns_from_db(&db_path, "ses_np").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].input_tokens, 7);
        assert_eq!(turns[0].assistant_responses.as_deref(), Some("ok"));
    }

    /// A prompt with no assistant response yet is skipped, not reported empty.
    #[test]
    fn test_in_flight_turn_with_no_usage_is_skipped() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(&conn, "ses_empty", None, None, 1000);
        insert_message(&conn, "u1", "ses_empty", 10, r#"{"role":"user"}"#);
        insert_part(
            &conn,
            "pu1",
            "u1",
            "ses_empty",
            11,
            r#"{"type":"text","text":"pending"}"#,
        );

        let turns = parse_turns_from_db(&db_path, "ses_empty").unwrap();
        assert!(
            turns.is_empty(),
            "expected no turns for an unanswered prompt"
        );
    }

    /// Session-level model is used when no assistant message carries one.
    #[test]
    fn test_session_model_is_used_as_fallback() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();

        insert_session(
            &conn,
            "ses_sm",
            None,
            Some(r#"{"id":"gpt-5.4-mini","providerID":"openai"}"#),
            1000,
        );
        insert_message(&conn, "u1", "ses_sm", 10, r#"{"role":"user"}"#);
        insert_part(
            &conn,
            "pu1",
            "u1",
            "ses_sm",
            11,
            r#"{"type":"text","text":"hi"}"#,
        );
        insert_message(
            &conn,
            "a1",
            "ses_sm",
            20,
            r#"{"role":"assistant","parentID":"u1","tokens":{"input":5,"output":5,"reasoning":0,"cache":{"write":0,"read":0}}}"#,
        );

        let turns = parse_turns_from_db(&db_path, "ses_sm").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].model, "gpt-5.4-mini");
    }

    /// Unknown session ids yield no turns rather than an error.
    #[test]
    fn test_unknown_session_returns_no_turns() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "ses_present", None, None, 1000);
        drop(conn);

        let turns = parse_turns_from_db(&db_path, "ses_absent").unwrap();
        assert!(turns.is_empty());
    }

    #[test]
    fn test_parse_model_column_handles_json_and_plain() {
        assert_eq!(
            parse_model_column(r#"{"id":"claude-opus-4-8","providerID":"anthropic"}"#).as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            parse_model_column("plain-model").as_deref(),
            Some("plain-model")
        );
        assert_eq!(parse_model_column("   ").as_deref(), None);
    }

    #[test]
    fn test_truncate_str_appends_marker() {
        assert_eq!(truncate_str("short", 10), "short");
        let long = "x".repeat(20);
        let cut = truncate_str(&long, 5);
        assert!(cut.starts_with("xxxxx"));
        assert!(cut.ends_with("...(truncated)"));
    }

    // -- OpenCode 2.x layout -------------------------------------------------

    fn add_v2_tables(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                directory TEXT,
                model TEXT,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    }

    fn insert_v2_message(conn: &Connection, sid: &str, seq: i64, kind: &str, data: &str) {
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?5)",
            rusqlite::params![format!("msg_{}_{}", sid, seq), sid, kind, seq, data],
        )
        .unwrap();
    }

    /// Two turns; the first has two assistant rows (tool step + answer) whose
    /// usage is summed, and an `idle` row that must be ignored.
    #[test]
    fn test_v2_turns_are_split_on_user_rows_and_summed() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        add_v2_tables(&conn);
        conn.execute(
            "INSERT INTO session_v2 (id, directory, model, time_updated) VALUES ('ses_v2', NULL, ?1, 1)",
            [r#"{"id":"session-model","providerID":"anthropic"}"#],
        )
        .unwrap();

        insert_v2_message(
            &conn,
            "ses_v2",
            1,
            "user",
            r#"{"text":"run echo","files":[]}"#,
        );
        insert_v2_message(
            &conn,
            "ses_v2",
            2,
            "assistant",
            r#"{"model":{"id":"claude-opus-4-8"},
                "content":[{"type":"reasoning","text":"thinking"},
                           {"type":"tool","id":"call_1","name":"bash",
                            "state":{"status":"completed","input":{"command":"echo hi"},
                                     "content":[{"type":"text","text":"hi"}]}}],
                "tokens":{"input":10,"output":4,"reasoning":1,"cache":{"read":100,"write":20}}}"#,
        );
        insert_v2_message(
            &conn,
            "ses_v2",
            3,
            "assistant",
            r#"{"model":{"id":"claude-opus-4-8"},
                "content":[{"type":"text","text":"done"}],
                "tokens":{"input":2,"output":6,"reasoning":0,"cache":{"read":5,"write":0}}}"#,
        );
        insert_v2_message(&conn, "ses_v2", 4, "idle", r#"{"outcome":"succeeded"}"#);
        insert_v2_message(
            &conn,
            "ses_v2",
            5,
            "user",
            r#"{"text":"second","files":[]}"#,
        );
        insert_v2_message(
            &conn,
            "ses_v2",
            6,
            "assistant",
            r#"{"content":[{"type":"text","text":"ok"}],
                "tokens":{"input":1,"output":1,"cache":{"read":0,"write":0}}}"#,
        );
        drop(conn);

        let turns = parse_turns_from_db(&db_path, "ses_v2").unwrap();
        assert_eq!(turns.len(), 2);

        let t1 = &turns[0];
        assert_eq!(t1.turn_index, 1);
        assert_eq!(t1.model, "claude-opus-4-8");
        assert_eq!(t1.input_tokens, 12);
        assert_eq!(t1.output_tokens, 11); // 4 + 1 reasoning + 6
        assert_eq!(t1.cache_read_tokens, 105);
        assert_eq!(t1.cache_creation_tokens, 20);
        assert_eq!(t1.total_tokens, 12 + 11 + 105 + 20);
        assert_eq!(t1.user_prompts.as_deref(), Some("run echo"));
        assert_eq!(t1.assistant_responses.as_deref(), Some("done"));

        let tools: Value = serde_json::from_str(t1.tool_uses.as_deref().unwrap()).unwrap();
        assert_eq!(tools[0]["name"], "bash");
        assert_eq!(tools[0]["id"], "call_1");
        assert_eq!(tools[0]["arguments"]["command"], "echo hi");
        assert_eq!(tools[0]["result"], "hi");

        // No per-row model on turn 2: falls back to the session model.
        assert_eq!(turns[1].turn_index, 2);
        assert_eq!(turns[1].model, "session-model");
    }

    /// A V2 prompt with no assistant row yet (in flight / aborted before the
    /// model answered) is skipped, but later turns keep their position.
    #[test]
    fn test_v2_prompt_without_assistant_is_skipped() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        add_v2_tables(&conn);
        conn.execute(
            "INSERT INTO session_v2 (id, directory, model, time_updated) VALUES ('ses_v2', NULL, NULL, 1)",
            [],
        )
        .unwrap();
        insert_v2_message(&conn, "ses_v2", 1, "user", r#"{"text":"lost"}"#);
        insert_v2_message(&conn, "ses_v2", 2, "user", r#"{"text":"kept"}"#);
        insert_v2_message(
            &conn,
            "ses_v2",
            3,
            "assistant",
            r#"{"content":[{"type":"text","text":"hi"}],"tokens":{"input":3,"output":2,"cache":{"read":0,"write":0}}}"#,
        );
        drop(conn);

        let turns = parse_turns_from_db(&db_path, "ses_v2").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_index, 2);
        assert_eq!(turns[0].user_prompts.as_deref(), Some("kept"));
    }

    /// A session only present in the 1.x tables still goes through the 1.x path
    /// when the 2.x tables exist (shared DB after a 2.x launch).
    #[test]
    fn test_v1_session_still_parsed_when_v2_tables_exist() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        add_v2_tables(&conn);
        insert_session(&conn, "ses_v1", None, Some("m"), 1000);
        insert_message(&conn, "msg_u", "ses_v1", 1, r#"{"role":"user"}"#);
        insert_message(
            &conn,
            "msg_a",
            "ses_v1",
            2,
            r#"{"role":"assistant","parentID":"msg_u","modelID":"m","tokens":{"input":7,"output":3,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
        );
        drop(conn);

        let turns = parse_turns_from_db(&db_path, "ses_v1").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].input_tokens, 7);
    }

    /// Manual fallback picks the newest session across both layouts.
    #[test]
    fn test_latest_session_id_spans_v1_and_v2() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        add_v2_tables(&conn);
        insert_session(&conn, "ses_old_v1", None, None, 100);
        conn.execute(
            "INSERT INTO session_v2 (id, directory, model, time_updated) VALUES ('ses_new_v2', NULL, NULL, 200)",
            [],
        )
        .unwrap();
        assert_eq!(
            latest_session_id(&conn).unwrap().as_deref(),
            Some("ses_new_v2")
        );

        insert_session(&conn, "ses_newer_v1", None, None, 300);
        assert_eq!(
            latest_session_id(&conn).unwrap().as_deref(),
            Some("ses_newer_v1")
        );
    }
}
