use super::config::TrackerConfig;
use super::log::ReportOutcome;
use crate::http;
use serde_json::{Value, json};
use std::process::Command;

const API_PATH: &str = "/ai-code-boost/open/report/stats";

pub fn upload_commit(
    repo_path: &str,
    commit_sha: &str,
    diff_gz: Vec<u8>,
    config: &TrackerConfig,
    remote: &str,
    branch: &str,
) -> Result<ReportOutcome, String> {
    let commit_info = gather_commit_info(repo_path, commit_sha)?;
    let repo_url = get_remote_url(repo_path, remote).unwrap_or_else(|| repo_path.to_string());
    let git_ai_raw = get_git_ai_stats(repo_path, commit_sha);
    let ai_line_ranges = get_ai_line_ranges(repo_path, commit_sha);
    let deleted_line_origins = super::origins::deleted_line_origins(repo_path, commit_sha);
    let git_ai_version = get_git_ai_version();
    let pusher_identity = get_pusher_identity(repo_path, commit_sha, Some(config));

    let diff_gz_base64 = if diff_gz.is_empty() {
        Value::Null
    } else {
        Value::String(encode_base64(&diff_gz))
    };

    let team_id: i64 = config
        .team_id
        .parse()
        .map_err(|_| "invalid team_id".to_string())?;

    let pushed_at = chrono::Utc::now().to_rfc3339();

    // Prefer tracker-config username; fall back to git identity name.
    let username = config
        .username
        .as_deref()
        .filter(|u| !u.is_empty())
        .unwrap_or(&pusher_identity.name)
        .to_string();

    let payload = json!({
        "team_id": team_id,
        "team_key": config.team_key,
        "is_doctor": false,
        "repo_url": repo_url,
        "pushed_at": pushed_at,
        "username": username,
        "pusher_name": pusher_identity.name,
        "local_ref": branch,
        "remote_ref": branch,
        "commits": [{
            "commit_sha": commit_sha,
            "commit_message": commit_info.message,
            "commit_timestamp": commit_info.committer_timestamp,
            "git_ai_raw": git_ai_raw,
            "git_ai_version": git_ai_version,
            "diff_gz": diff_gz_base64,
            "ai_line_ranges": ai_line_ranges,
            "deleted_line_origins": deleted_line_origins,
        }]
    });

    let payload_str = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let url = format!("{}{}", config.tracker_url, API_PATH);

    let agent = http::build_agent(None);
    let request = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("X-Team-Key", &config.team_key);

    let response = http::send_with_body(request, &payload_str)?;

    let mut outcome = parse_stats_response(
        response.as_str().unwrap_or_default(),
        ReportOutcome::from_http_status(response.status_code),
    );
    if outcome.message.is_none() {
        outcome.message = Some(format!("HTTP {}，响应未确认入库", response.status_code));
    }
    if outcome.saved {
        println!(
            "[{} tracker] saved {}",
            crate::config::PRODUCT_NAME,
            &commit_sha[..8]
        );
    }
    // Preserve the HTTP status even for a rejection; callers retain failed
    // reports in the retry queue rather than advancing reported markers.
    Ok(outcome)
}

/// Extract the per-commit status the server assigned to our upload.
///
/// Response shape (see `docs/easylife-ai doctor.md`):
/// `{"code":200,"data":{"success":true,"inserted":1,"duplicated":0,"failed":0,
///   "results":[{"commitSha":"...","recordId":612,"status":"inserted"}]}}`
///
/// `status` is `inserted` (new row), `duplicated` (already present) or
/// `failed` (rejected -- `recordId` is null).
fn parse_stats_response(body: &str, mut outcome: ReportOutcome) -> ReportOutcome {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return outcome;
    };
    let Some(data) = value.get("data") else {
        return outcome;
    };

    outcome.server_success = data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    outcome.inserted = data.get("inserted").and_then(Value::as_i64);
    outcome.duplicated = data.get("duplicated").and_then(Value::as_i64);
    outcome.failed = data.get("failed").and_then(Value::as_i64);

    if let Some(result) = data
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
    {
        outcome.record_id = result.get("recordId").and_then(Value::as_i64);
        if let Some(status) = result.get("status").and_then(Value::as_str) {
            outcome.message = Some(status.to_string());
        }
    }

    // `saved` means this request inserted a new row, not merely that the same
    // record already existed. Keep duplicates distinct in the log counters.
    outcome.saved = outcome.message.as_deref() == Some("inserted")
        && outcome.record_id.is_some()
        && outcome.server_success;

    outcome
}

struct CommitMeta {
    message: String,
    committer_timestamp: String,
}

struct Identity {
    email: String,
    name: String,
}

fn gather_commit_info(repo_path: &str, commit_sha: &str) -> Result<CommitMeta, String> {
    let output = Command::new("git")
        .args([
            "-C",
            repo_path,
            "show",
            "-s",
            "--format=%s%n%cI",
            commit_sha,
        ])
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let message = lines.next().unwrap_or("").to_string();
    let committer_timestamp_raw = lines.next().unwrap_or("").to_string();

    let committer_timestamp =
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&committer_timestamp_raw) {
            dt.with_timezone(&chrono::Utc).to_rfc3339()
        } else {
            committer_timestamp_raw
        };

    Ok(CommitMeta {
        message,
        committer_timestamp,
    })
}

fn get_pusher_identity(
    repo_path: &str,
    commit_sha: &str,
    config: Option<&TrackerConfig>,
) -> Identity {
    // 优先级 0: 配置文件显式指定的 username
    if let Some(name) = config.and_then(|c| c.username.as_ref()) {
        // 从 git config 获取 email 作为配套
        let email = git_config_get(repo_path, "--local", "user.email")
            .or_else(|| git_config_get(repo_path, "--global", "user.email"))
            .or_else(|| try_commit_author(repo_path, commit_sha).map(|id| id.email))
            .unwrap_or_else(|| format!("{}@localhost", name));
        return Identity {
            email,
            name: name.clone(),
        };
    }

    // 原有的 4 层兜底逻辑
    if let Some(id) = try_local_git_config(repo_path) {
        return id;
    }
    if let Some(id) = try_global_git_config(repo_path) {
        return id;
    }
    if let Some(id) = try_commit_author(repo_path, commit_sha) {
        return id;
    }
    if let Some(id) = try_hostname_fallback() {
        return id;
    }
    Identity {
        email: String::new(),
        name: String::new(),
    }
}

fn try_local_git_config(repo_path: &str) -> Option<Identity> {
    let email = git_config_get(repo_path, "--local", "user.email")?;
    let name = git_config_get(repo_path, "--local", "user.name").unwrap_or_default();
    Some(Identity { email, name })
}

fn try_global_git_config(repo_path: &str) -> Option<Identity> {
    let email = git_config_get(repo_path, "--global", "user.email")?;
    let name = git_config_get(repo_path, "--global", "user.name").unwrap_or_default();
    Some(Identity { email, name })
}

fn try_commit_author(repo_path: &str, commit_sha: &str) -> Option<Identity> {
    let email_output = Command::new("git")
        .args(["-C", repo_path, "log", "-1", "--format=%ae", commit_sha])
        .output()
        .ok()?;
    let name_output = Command::new("git")
        .args(["-C", repo_path, "log", "-1", "--format=%an", commit_sha])
        .output()
        .ok()?;

    if !email_output.status.success() {
        return None;
    }

    let email = String::from_utf8_lossy(&email_output.stdout)
        .trim()
        .to_string();
    let name = if name_output.status.success() {
        String::from_utf8_lossy(&name_output.stdout)
            .trim()
            .to_string()
    } else {
        String::new()
    };

    if email.is_empty() {
        None
    } else {
        Some(Identity { email, name })
    }
}

fn try_hostname_fallback() -> Option<Identity> {
    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let hostname_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hostname_str.is_empty() {
        return None;
    }
    Some(Identity {
        email: format!("{}@localhost", hostname_str),
        name: hostname_str,
    })
}

fn git_config_get(repo_path: &str, scope: &str, key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", repo_path, "config", scope, key])
        .output()
        .ok()?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn get_remote_url(repo_path: &str, remote: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", repo_path, "remote", "get-url", remote])
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

fn get_git_ai_stats(repo_path: &str, commit_sha: &str) -> serde_json::Value {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("easylife-ai")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "easylife-ai".to_string());

    let work_tree = super::resolve_work_tree(repo_path);

    let output = Command::new(&binary)
        .args(["stats", "--json", commit_sha])
        .current_dir(&work_tree)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}))
        }
        _ => serde_json::json!({}),
    }
}

/// 读取本提交 `refs/notes/ai` 里的逐行归属，只保留 AI 写的行。
///
/// 返回 `{ "文件路径": [[起始行, 结束行], ...] }`，行号是提交后文件里的行号（1 起，闭区间），
/// 每个文件的区间已排序并合并。
/// - 没有 notes（或读取 / 解析失败）返回 `null`：服务端据此区分「无归属信息」和「有归属但没有 AI 行」。
/// - 有 notes 但没有 AI 行时返回 `{}`。
///
/// 只上报文件路径和行号，不上报 notes 里的 prompts / 对话内容。
fn get_ai_line_ranges(repo_path: &str, commit_sha: &str) -> Value {
    let work_tree = super::resolve_work_tree(repo_path);
    let Ok(repo) = crate::git::repository::find_repository_in_path(&work_tree) else {
        return Value::Null;
    };
    match crate::git::refs::get_authorship(&repo, commit_sha) {
        Some(log) => ai_line_ranges_from_log(&log),
        None => Value::Null,
    }
}

fn ai_line_ranges_from_log(
    log: &crate::authorship::authorship_log_serialization::AuthorshipLog,
) -> Value {
    use crate::authorship::authorship_log::LineRange;

    let mut files = serde_json::Map::new();
    for file in &log.attestations {
        let mut ranges: Vec<(u32, u32)> = file
            .entries
            .iter()
            // `h_` 前缀是已知的人工作者，其余哈希对应 metadata.prompts 里的 AI 会话
            .filter(|entry| !entry.hash.starts_with("h_"))
            .flat_map(|entry| entry.line_ranges.iter())
            .map(|range| match range {
                LineRange::Single(line) => (*line, *line),
                LineRange::Range(start, end) => (*start.min(end), *start.max(end)),
            })
            .collect();
        if ranges.is_empty() {
            continue;
        }

        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                // 重叠或首尾相接的区间合并成一个
                Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }

        let value = merged
            .into_iter()
            .map(|(start, end)| json!([start, end]))
            .collect::<Vec<_>>();
        // 同一路径出现多次时追加，服务端按区间求交集，重复无影响
        match files.get_mut(&file.file_path) {
            Some(Value::Array(existing)) => existing.extend(value),
            _ => {
                files.insert(file.file_path.clone(), Value::Array(value));
            }
        }
    }
    Value::Object(files)
}

fn get_git_ai_version() -> String {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("easylife-ai")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "easylife-ai".to_string());

    let output = Command::new(&binary).arg("--version").output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn inserted_duplicate_and_failure_are_distinct() {
        for (status, saved, accepted) in [
            ("inserted", true, true),
            ("duplicated", false, true),
            ("failed", false, false),
        ] {
            let body = serde_json::json!({"data":{"success":true,"results":[{"status":status,"recordId":42}]}});
            let result =
                parse_stats_response(&body.to_string(), ReportOutcome::from_http_status(200));
            assert_eq!(result.saved, saved);
            assert_eq!(result.accepted(), accepted);
        }
        let malformed = parse_stats_response("not json", ReportOutcome::from_http_status(200));
        assert!(!malformed.accepted());
    }

    #[test]
    fn ai_line_ranges_keep_only_ai_entries_sorted_and_merged() {
        use crate::authorship::authorship_log::LineRange;
        use crate::authorship::authorship_log_serialization::{AttestationEntry, AuthorshipLog};

        let mut log = AuthorshipLog::new();
        let file = log.get_or_create_file("src/a.rs");
        file.add_entry(AttestationEntry::new(
            "ef272a2aa4e23b7d".to_string(),
            vec![LineRange::Range(10, 12), LineRange::Single(5)],
        ));
        // 另一个 AI 会话：与 10-12 首尾相接，应合并成 10-14
        file.add_entry(AttestationEntry::new(
            "0123456789abcdef".to_string(),
            vec![LineRange::Range(13, 14)],
        ));
        // 已知人工作者：不能计入 AI 行
        file.add_entry(AttestationEntry::new(
            "h_alice".to_string(),
            vec![LineRange::Range(20, 30)],
        ));
        // 只有人工行的文件不出现在结果里
        log.get_or_create_file("src/human.rs")
            .add_entry(AttestationEntry::new(
                "h_bob".to_string(),
                vec![LineRange::Single(1)],
            ));

        let got = ai_line_ranges_from_log(&log);
        assert_eq!(got, serde_json::json!({ "src/a.rs": [[5, 5], [10, 14]] }));

        // 有 notes 但没有 AI 行：空对象，而不是 null
        assert_eq!(
            ai_line_ranges_from_log(&AuthorshipLog::new()),
            serde_json::json!({})
        );
    }
}

fn encode_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        result.push(ALPHABET[b0 >> 2] as char);
        result.push(ALPHABET[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }
    result
}
