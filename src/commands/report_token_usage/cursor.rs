//! Cursor token and prompt reporting.
//!
//! Triggered by the Cursor `stop` hook (`~/.cursor/hooks.json`). Token usage
//! comes straight from the hook stdin payload (`input_tokens`, `output_tokens`,
//! `cache_read_tokens`, `cache_write_tokens`), which Cursor fills from the
//! server-side `turnEnded` message of the turn that just finished. Prompt,
//! response and tool calls come from the agent transcript JSONL referenced by
//! `transcript_path`.
//!
//! Cursor does not persist per-turn token counts anywhere else, so a turn whose
//! hook payload has no token data (or only zeros) is NOT reported: token usage
//! is never reported as 0 and never estimated.

use super::TokenUsageData;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Wait between re-reads while the transcript has not yet recorded `turn_ended`.
const TRANSCRIPT_SETTLE_RETRY_MS: u64 = 300;
const TRANSCRIPT_SETTLE_ATTEMPTS: usize = 5;

/// `stop` hook stdin payload from Cursor.
///
/// Unknown fields are ignored. Token fields are optional because Cursor only
/// fills them when the server reported usage for the turn.
#[derive(Debug, Default, Deserialize)]
pub struct CursorStopPayload {
    pub conversation_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub input_tokens: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub output_tokens: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub cache_read_tokens: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub cache_write_tokens: Option<i64>,
}

/// Accept a JSON number, a numeric string (protobuf int64 style) or null.
fn de_opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    Ok(match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

/// One user turn reconstructed from the transcript.
#[derive(Debug, Default)]
struct ParsedTurn {
    prompt: String,
    started_at: Option<String>,
    responses: Vec<String>,
    tools: Vec<Value>,
    ended: bool,
}

/// Parse the transcript into turns. A turn starts at a `role:user` line that
/// carries text and ends at the `{"type":"turn_ended"}` line. Malformed lines
/// are skipped because the file may still be partially written.
fn parse_transcript(content: &str) -> Vec<ParsedTurn> {
    let mut turns: Vec<ParsedTurn> = Vec::new();
    // tool_use id -> index into the current turn's `tools`.
    let mut tool_index: HashMap<String, usize> = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if entry.get("type").and_then(Value::as_str) == Some("turn_ended") {
            if let Some(turn) = turns.last_mut() {
                turn.ended = true;
            }
            continue;
        }

        let items = message_items(&entry);
        match entry.get("role").and_then(Value::as_str) {
            Some("user") => {
                let mut texts: Vec<&str> = Vec::new();
                for item in &items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                texts.push(text);
                            }
                        }
                        Some("tool_result") => {
                            attach_tool_result(&mut turns, &tool_index, item);
                        }
                        _ => {}
                    }
                }
                let joined = texts.join("\n");
                if joined.trim().is_empty() {
                    continue;
                }
                tool_index.clear();
                turns.push(ParsedTurn {
                    prompt: extract_user_query(&joined),
                    started_at: parse_user_timestamp(&joined),
                    ..Default::default()
                });
            }
            Some("assistant") => {
                let Some(turn) = turns.last_mut() else {
                    continue;
                };
                for item in &items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str)
                                && !text.trim().is_empty()
                            {
                                turn.responses.push(text.to_string());
                            }
                        }
                        Some("tool_use") => {
                            let Some(name) = item.get("name").and_then(Value::as_str) else {
                                continue;
                            };
                            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                            let mut tool = serde_json::json!({
                                "name": name,
                                "id": id,
                                "arguments": item.get("input").cloned().unwrap_or(Value::Null),
                            });
                            tool["order"] = Value::from(turn.tools.len() as i64 + 1);
                            if !id.is_empty() {
                                tool_index.insert(id.to_string(), turn.tools.len());
                            }
                            turn.tools.push(tool);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    turns
}

/// `message.content` is normally an array of typed items; tolerate a bare string.
fn message_items(entry: &Value) -> Vec<Value> {
    match entry.pointer("/message/content") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![serde_json::json!({"type": "text", "text": text})],
        _ => Vec::new(),
    }
}

fn attach_tool_result(turns: &mut [ParsedTurn], index: &HashMap<String, usize>, item: &Value) {
    let Some(id) = item.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    if let (Some(turn), Some(&i)) = (turns.last_mut(), index.get(id))
        && let Some(tool) = turn.tools.get_mut(i)
    {
        tool["result"] = item.get("content").cloned().unwrap_or(Value::Null);
    }
}

/// Remove the `<timestamp>` and `<user_query>` wrappers Cursor puts around the
/// user's text.
fn extract_user_query(text: &str) -> String {
    let without_ts = match (text.find("<timestamp>"), text.find("</timestamp>")) {
        (Some(start), Some(end)) if end > start => {
            format!("{}{}", &text[..start], &text[end + "</timestamp>".len()..])
        }
        _ => text.to_string(),
    };
    let trimmed = without_ts.trim();
    if let Some(start) = trimmed.find("<user_query>") {
        let inner = &trimmed[start + "<user_query>".len()..];
        let inner = inner
            .rfind("</user_query>")
            .map_or(inner, |end| &inner[..end]);
        return inner.trim().to_string();
    }
    trimmed.to_string()
}

/// Parse `<timestamp>Monday, Jul 13, 2026, 6:56 PM (UTC+8)</timestamp>` into an
/// RFC3339 string in UTC+8, the zone all reported timestamps use.
fn parse_user_timestamp(text: &str) -> Option<String> {
    let start = text.find("<timestamp>")? + "<timestamp>".len();
    let end = text.find("</timestamp>")?;
    let raw = text.get(start..end)?.trim();

    // "<weekday>, <Mon> <D>, <YYYY>, <h:mm> <AM|PM> (UTC<+/-h[:mm]>)"
    let (_, rest) = raw.split_once(", ")?;
    let (datetime_part, offset_part) = match rest.rfind(" (UTC") {
        Some(i) => (&rest[..i], Some(&rest[i + " (UTC".len()..])),
        None => (rest, None),
    };
    let naive = NaiveDateTime::parse_from_str(datetime_part.trim(), "%b %d, %Y, %I:%M %p").ok()?;
    let offset = offset_part
        .and_then(|s| s.strip_suffix(')'))
        .and_then(parse_utc_offset)
        .unwrap_or_else(report_offset);
    let local = offset.from_local_datetime(&naive).single()?;
    Some(local.with_timezone(&report_offset()).to_rfc3339())
}

/// Parse "+8", "-5", "+5:30" into a fixed offset.
fn parse_utc_offset(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    let sign = match s.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let body = &s[1..];
    let (hours, minutes) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (body.parse::<i32>().ok()?, 0),
    };
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn report_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("valid offset")
}

fn now_rfc3339(now: DateTime<Utc>) -> String {
    now.with_timezone(&report_offset()).to_rfc3339()
}

/// Build the usage record for the turn that just ended.
///
/// Token counts exist only for the latest turn (the hook payload describes the
/// turn that just stopped), so only the last transcript turn is reported.
/// Returns `None` when there are no token counts to report.
fn build_current_turn(
    payload: &CursorStopPayload,
    turns: &[ParsedTurn],
    now: DateTime<Utc>,
) -> Option<TokenUsageData> {
    let turn = turns.last()?;

    let input = payload.input_tokens.unwrap_or(0).max(0);
    let output = payload.output_tokens.unwrap_or(0).max(0);
    let cache_read = payload.cache_read_tokens.unwrap_or(0).max(0);
    let cache_creation = payload.cache_write_tokens.unwrap_or(0).max(0);
    if input + output == 0 {
        return None;
    }

    let model = [payload.model_id.as_deref(), payload.model.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|m| !m.is_empty())
        .unwrap_or("unknown")
        .to_string();

    let tool_uses = if turn.tools.is_empty() {
        None
    } else {
        serde_json::to_string(&turn.tools).ok()
    };
    let responses = turn.responses.join("\n\n");

    Some(TokenUsageData {
        session_id: payload.conversation_id.clone(),
        turn_index: turns.len() as i64,
        model,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        total_tokens: input + output + cache_read + cache_creation,
        // Cost is computed by the backend from `model_pricing`.
        cost_usd: None,
        repo_url: None,
        project_name: payload
            .workspace_roots
            .first()
            .and_then(|root| std::path::Path::new(root).file_name())
            .map(|name| name.to_string_lossy().to_string()),
        user_prompts: Some(turn.prompt.clone()).filter(|p| !p.is_empty()),
        assistant_responses: Some(responses).filter(|r| !r.is_empty()),
        tool_uses,
        turn_started_at: turn.started_at.clone(),
        turn_completed_at: Some(now_rfc3339(now)),
    })
}

/// Read the transcript, waiting briefly for Cursor to record `turn_ended`.
fn read_transcript_settled(path: &str) -> Result<String, String> {
    let mut content = String::new();
    for attempt in 0..TRANSCRIPT_SETTLE_ATTEMPTS {
        content = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        if parse_transcript(&content).last().is_some_and(|t| t.ended) {
            break;
        }
        if attempt + 1 < TRANSCRIPT_SETTLE_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(TRANSCRIPT_SETTLE_RETRY_MS));
        }
    }
    Ok(content)
}

/// Parse the just-finished Cursor turn.
///
/// Returns the incremental-state key (`cursor:<conversation_id>`) and the turn,
/// or `Ok(None)` when there is nothing reportable (no transcript, no tokens).
pub fn parse_turns(
    payload: &CursorStopPayload,
) -> Result<Option<(String, Vec<TokenUsageData>)>, String> {
    if payload.conversation_id.trim().is_empty() {
        return Ok(None);
    }
    let Some(path) = payload
        .transcript_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        tracing::debug!("report-token-usage: Cursor transcript_path is empty, skipping");
        return Ok(None);
    };

    let content = read_transcript_settled(path)?;
    let turns = parse_transcript(&content);
    let Some(usage) = build_current_turn(payload, &turns, Utc::now()) else {
        tracing::debug!(
            "report-token-usage: Cursor turn has no token usage (or no transcript turn), skipping"
        );
        return Ok(None);
    };

    Ok(Some((
        format!("cursor:{}", payload.conversation_id),
        vec![usage],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_line(text: &str) -> String {
        json!({"role":"user","message":{"content":[{"type":"text","text":text}]}}).to_string()
    }

    fn assistant_line(text: &str) -> String {
        json!({"role":"assistant","message":{"content":[{"type":"text","text":text}]}}).to_string()
    }

    fn ended_line() -> String {
        json!({"type":"turn_ended","status":"success"}).to_string()
    }

    fn payload(input: Option<i64>, output: Option<i64>) -> CursorStopPayload {
        CursorStopPayload {
            conversation_id: "conv-1".to_string(),
            model: Some("default".to_string()),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: Some(30),
            cache_write_tokens: Some(5),
            workspace_roots: vec!["/work/demo-proj".to_string()],
            ..Default::default()
        }
    }

    fn two_turn_transcript() -> String {
        [
            user_line(
                "<timestamp>Monday, Jul 13, 2026, 6:56 PM (UTC+8)</timestamp>\n<user_query>\nhi\n</user_query>",
            ),
            assistant_line("hello"),
            ended_line(),
            user_line("<user_query>\nsecond\n</user_query>"),
            assistant_line("part one"),
            assistant_line("part two"),
            ended_line(),
        ]
        .join("\n")
    }

    #[test]
    fn splits_turns_and_strips_wrappers() {
        let turns = parse_transcript(&two_turn_transcript());
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].prompt, "hi");
        assert_eq!(turns[0].responses, vec!["hello"]);
        assert!(turns[0].ended && turns[1].ended);
        assert_eq!(turns[1].prompt, "second");
        assert_eq!(turns[1].responses, vec!["part one", "part two"]);
    }

    #[test]
    fn timestamp_is_converted_to_utc_plus_8() {
        let turns = parse_transcript(&two_turn_transcript());
        assert_eq!(
            turns[0].started_at.as_deref(),
            Some("2026-07-13T18:56:00+08:00")
        );
        assert!(turns[1].started_at.is_none());
        assert_eq!(
            parse_user_timestamp("<timestamp>Monday, Jul 13, 2026, 6:56 PM (UTC-5)</timestamp>")
                .as_deref(),
            Some("2026-07-14T07:56:00+08:00")
        );
    }

    #[test]
    fn reports_only_latest_turn_with_all_token_fields() {
        let turns = parse_transcript(&two_turn_transcript());
        let usage =
            build_current_turn(&payload(Some(100), Some(20)), &turns, Utc::now()).expect("usage");
        assert_eq!(usage.session_id, "conv-1");
        assert_eq!(usage.turn_index, 2);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_creation_tokens, 5);
        assert_eq!(usage.total_tokens, 155);
        assert_eq!(usage.model, "default");
        assert_eq!(usage.user_prompts.as_deref(), Some("second"));
        assert_eq!(
            usage.assistant_responses.as_deref(),
            Some("part one\n\npart two")
        );
        assert_eq!(usage.project_name.as_deref(), Some("demo-proj"));
        assert!(usage.cost_usd.is_none());
        assert!(usage.turn_completed_at.is_some());
    }

    #[test]
    fn missing_or_zero_tokens_are_not_reported() {
        let turns = parse_transcript(&two_turn_transcript());
        assert!(build_current_turn(&payload(None, None), &turns, Utc::now()).is_none());
        assert!(build_current_turn(&payload(Some(0), Some(0)), &turns, Utc::now()).is_none());
        // Cache counters alone do not make a reportable turn.
        assert!(build_current_turn(&payload(None, Some(0)), &turns, Utc::now()).is_none());
    }

    #[test]
    fn model_id_wins_over_model() {
        let turns = parse_transcript(&two_turn_transcript());
        let mut p = payload(Some(1), Some(1));
        p.model_id = Some("claude-4-sonnet".to_string());
        let usage = build_current_turn(&p, &turns, Utc::now()).unwrap();
        assert_eq!(usage.model, "claude-4-sonnet");
        p.model_id = None;
        p.model = None;
        let usage = build_current_turn(&p, &turns, Utc::now()).unwrap();
        assert_eq!(usage.model, "unknown");
    }

    #[test]
    fn tool_calls_are_collected_with_results() {
        let transcript = [
            user_line("<user_query>\nedit it\n</user_query>"),
            json!({"role":"assistant","message":{"content":[
                {"type":"text","text":"on it"},
                {"type":"tool_use","id":"t1","name":"Write","input":{"path":"a.txt"}}
            ]}})
            .to_string(),
            json!({"role":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"ok"}
            ]}})
            .to_string(),
            assistant_line("done"),
            ended_line(),
        ]
        .join("\n");
        let turns = parse_transcript(&transcript);
        // The tool_result line must not start a new turn.
        assert_eq!(turns.len(), 1);
        let usage = build_current_turn(&payload(Some(1), Some(1)), &turns, Utc::now()).unwrap();
        let tools: Value = serde_json::from_str(usage.tool_uses.as_ref().unwrap()).unwrap();
        assert_eq!(tools[0]["name"], "Write");
        assert_eq!(tools[0]["id"], "t1");
        assert_eq!(tools[0]["arguments"]["path"], "a.txt");
        assert_eq!(tools[0]["result"], "ok");
        assert_eq!(tools[0]["order"], 1);
    }

    #[test]
    fn malformed_and_unfinished_transcripts_do_not_panic() {
        let content = format!(
            "not json\n{}\n{{\"role\":",
            user_line("<user_query>x</user_query>")
        );
        let turns = parse_transcript(&content);
        assert_eq!(turns.len(), 1);
        assert!(!turns[0].ended);
        assert!(parse_transcript("").is_empty());
    }

    #[test]
    fn payload_accepts_numbers_strings_and_nulls() {
        let p: CursorStopPayload = serde_json::from_value(json!({
            "conversation_id": "c",
            "input_tokens": 10,
            "output_tokens": "20",
            "cache_read_tokens": null,
            "status": "completed",
            "unknown_field": true
        }))
        .unwrap();
        assert_eq!(p.input_tokens, Some(10));
        assert_eq!(p.output_tokens, Some(20));
        assert_eq!(p.cache_read_tokens, None);
        assert_eq!(p.cache_write_tokens, None);
    }

    #[test]
    fn parse_turns_reads_transcript_and_uses_state_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conv-1.jsonl");
        std::fs::write(&path, two_turn_transcript()).unwrap();
        let mut p = payload(Some(100), Some(20));
        p.transcript_path = Some(path.to_string_lossy().to_string());

        let (key, turns) = parse_turns(&p).unwrap().expect("turns");
        assert_eq!(key, "cursor:conv-1");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_index, 2);

        // No transcript path -> nothing to report, but not an error.
        p.transcript_path = None;
        assert!(parse_turns(&p).unwrap().is_none());
    }
}
