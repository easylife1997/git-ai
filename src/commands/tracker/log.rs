use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub enum LogStatus {
    Uploaded,
    Skipped,
    Failed,
    RetryOk,
    /// The POST reached the server (HTTP 2xx) but the server did not persist
    /// the record.  Distinguished from `Uploaded` because a 2xx response alone
    /// does not mean the data landed -- see `docs/easylife-ai doctor.md`.
    NotSaved,
}

impl LogStatus {
    fn as_str(&self) -> &str {
        match self {
            LogStatus::Uploaded => "uploaded",
            LogStatus::Skipped => "skipped",
            LogStatus::Failed => "failed",
            LogStatus::RetryOk => "retry_ok",
            LogStatus::NotSaved => "not_saved",
        }
    }

    fn display_name(&self) -> &str {
        match self {
            LogStatus::Uploaded => "✓ 上报成功",
            LogStatus::Skipped => "⊘ 已跳过",
            LogStatus::Failed => "✗ 上报失败",
            LogStatus::RetryOk => "↻ 重试成功",
            LogStatus::NotSaved => "⚠ 未入库",
        }
    }
}

/// Which reporting pipeline a log line came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Code,
    Token,
}

impl LogKind {
    fn as_str(&self) -> &str {
        match self {
            LogKind::Code => "code",
            LogKind::Token => "token",
        }
    }
}

/// Server-side outcome of a report POST.
///
/// The tracker server returns HTTP 200 for every non-transport outcome --
/// including validation failures and "nothing was inserted" -- so the HTTP
/// status alone cannot tell whether the data landed.  These fields capture the
/// response body so the log records what actually happened.
#[derive(Default, Clone)]
pub struct ReportOutcome {
    pub http_status: Option<u16>,
    pub server_success: bool,
    pub saved: bool,
    pub session_id: Option<String>,
    pub inserted: Option<i64>,
    pub duplicated: Option<i64>,
    pub failed: Option<i64>,
    pub record_id: Option<i64>,
    pub message: Option<String>,
    pub turns: Option<usize>,
    pub turn: Option<i64>,
}

impl ReportOutcome {
    /// Accepted existing data may advance incremental state without claiming a
    /// new insert. A bare HTTP 2xx or success flag is not sufficient.
    pub fn accepted(&self) -> bool {
        self.saved
            || (self.server_success
                && self.record_id.is_some()
                && matches!(
                    self.message.as_deref(),
                    Some("duplicated") | Some("数据未增长，已跳过")
                ))
    }

    pub fn from_http_status(status: u16) -> Self {
        Self {
            http_status: Some(status),
            ..Default::default()
        }
    }

    /// Render as tab-separated `key=value` columns.
    ///
    /// Tabs matter: readers split the line on `\t` first, so a single
    /// space-joined blob would collapse into one unparseable value.
    fn to_fields(&self) -> Vec<String> {
        let mut fields = Vec::new();
        if let Some(status) = self.http_status {
            fields.push(format!("http={}", status));
        }
        fields.push(format!("server_success={}", self.server_success));
        fields.push(format!("saved={}", self.saved));
        if let Some(session_id) = self.session_id.as_deref() {
            fields.push(format!("session_id={}", sanitize_field(session_id)));
        }
        if let Some(v) = self.inserted {
            fields.push(format!("inserted={}", v));
        }
        if let Some(v) = self.duplicated {
            fields.push(format!("duplicated={}", v));
        }
        if let Some(v) = self.failed {
            fields.push(format!("failed={}", v));
        }
        if let Some(v) = self.record_id {
            fields.push(format!("record_id={}", v));
        }
        if let Some(v) = self.turns {
            fields.push(format!("turns={}", v));
        }
        if let Some(v) = self.turn {
            fields.push(format!("turn={}", v));
        }
        if let Some(msg) = self.message.as_deref().filter(|m| !m.is_empty()) {
            fields.push(format!("msg={}", sanitize_field(msg)));
        }
        fields
    }
}

/// Keep values on one TSV line and out of the `key=value` grammar.
#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn fields_round_trip_with_tabs_and_session_identity() {
        let outcome = ReportOutcome {
            http_status: Some(200),
            saved: true,
            server_success: true,
            session_id: Some("doctor-long-session-id".to_string()),
            message: Some("ok\tline\nnext".to_string()),
            ..Default::default()
        };
        let line = format!(
            "t\tuploaded\tdoctor-short\tclaude/model\t/tmp/x\tkind=token\t{}",
            outcome.to_fields().join("\t")
        );
        let parts: Vec<_> = line.split('\t').collect();
        let (fields, _) = parse_trailing_fields(&parts);
        assert_eq!(field(&fields, "saved"), Some("true"));
        assert_eq!(field(&fields, "session_id"), Some("doctor-long-session-id"));
        assert_eq!(field(&fields, "msg"), Some("ok line next"));
        assert_eq!(truncate_ident("abcdef123456", 7), "abcdef1");
        assert!("中文测试会话".starts_with(truncate_ident("中文测试会话", 7)));
    }
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Shorten an identifier for display without splitting a UTF-8 character
/// boundary (session ids from other platforms are not always ASCII).
///
/// Truncation happens at *display* time only -- the log stores the full value
/// so readers can match a commit SHA or session id exactly.
fn truncate_ident(ident: &str, max_bytes: usize) -> &str {
    if ident.len() <= max_bytes {
        return ident;
    }
    let mut end = max_bytes;
    while end > 0 && !ident.is_char_boundary(end) {
        end -= 1;
    }
    &ident[..end]
}

fn truncate_for_display(ident: &str) -> &str {
    truncate_ident(ident, 12)
}

fn reason_display(reason: &str) -> String {
    match reason {
        "already_reported" => "已上报过".to_string(),
        "blacklisted" => "黑名单过滤".to_string(),
        "merge_commit" => "合并提交".to_string(),
        "synthetic_message" => "自动生成的提交信息".to_string(),
        "copy_paste_threshold" => "手动添加代码超过阈值（>1500行）".to_string(),
        other => other.to_string(),
    }
}

pub fn log_path() -> PathBuf {
    crate::mdm::utils::home_dir()
        .join(crate::config::APP_DIR_NAME)
        .join("tracker-upload.log")
}

/// Append one line to the tracker upload log.
///
/// The first five tab-separated columns are stable and positionally parsed by
/// [`print_log`]; everything after them is `key=value` so new fields can be
/// added without breaking older readers or older log files.
///
/// Layout: `timestamp  status  ident  source  repo_path  [reason|key=value...]`
#[allow(clippy::too_many_arguments)]
pub fn append_log(
    status: LogStatus,
    kind: LogKind,
    ident: &str,
    remote: &str,
    branch: &str,
    repo_path: &str,
    reason: Option<&str>,
    outcome: Option<&ReportOutcome>,
) {
    let timestamp = chrono::Utc::now().to_rfc3339();
    // Preserve the legacy SHA7 column for code. Token outcomes carry the full
    // session id in a separate session_id= field for exact correlation.
    let ident = match kind {
        LogKind::Code => truncate_ident(ident, 7),
        LogKind::Token => truncate_ident(ident, 12),
    };
    let ident = sanitize_field(ident);
    let remote_branch = sanitize_field(&format!("{}/{}", remote, branch));
    let repo_path = sanitize_field(repo_path);

    let mut line = format!(
        "{}\t{}\t{}\t{}\t{}\tkind={}",
        timestamp,
        status.as_str(),
        ident,
        remote_branch,
        repo_path,
        kind.as_str()
    );

    if let Some(outcome) = outcome {
        for field in outcome.to_fields() {
            line.push('\t');
            line.push_str(&field);
        }
    }
    if let Some(r) = reason {
        line.push_str(&format!("\treason={}", sanitize_field(r)));
    }
    line.push('\n');

    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn print_log(lines: usize) {
    let path = log_path();
    if !path.exists() {
        println!("暂无上报日志");
        return;
    }

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("无法打开日志文件: {}", e);
            return;
        }
    };

    let reader = BufReader::new(file);
    let all_lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    let start = if all_lines.len() > lines {
        all_lines.len() - lines
    } else {
        0
    };

    for line in &all_lines[start..] {
        print_formatted_line(line);
    }
}

/// Parse the trailing `key=value` columns of a log line.
///
/// Legacy lines (written before the `key=value` format landed) put a bare
/// reason string in column 6; those are returned via the `legacy_reason` slot
/// so old logs keep rendering the way they always did.
fn parse_trailing_fields<'a>(parts: &[&'a str]) -> (Vec<(&'a str, &'a str)>, Option<&'a str>) {
    let mut fields = Vec::new();
    let mut legacy_reason = None;

    for part in parts.iter().skip(5) {
        match part.split_once('=') {
            Some((key, value)) => fields.push((key, value)),
            // A column with no `=` can only be a legacy reason string.
            None => {
                if legacy_reason.is_none() {
                    legacy_reason = Some(*part);
                }
            }
        }
    }

    (fields, legacy_reason)
}

fn field<'a>(fields: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, value)| *value)
}

fn print_formatted_line(line: &str) {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 5 {
        println!("{}", line);
        return;
    }

    let timestamp = parts[0];
    let status_str = parts[1];
    let ident = parts[2];
    let remote_branch = parts[3];
    let repo_path = parts[4];

    let (fields, legacy_reason) = parse_trailing_fields(&parts);
    let reason = legacy_reason.or_else(|| field(&fields, "reason"));

    let time_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        let local: chrono::DateTime<chrono::Local> = dt.into();
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        timestamp.to_string()
    };

    let status_display = match status_str {
        "uploaded" => LogStatus::Uploaded.display_name(),
        "skipped" => LogStatus::Skipped.display_name(),
        "failed" => LogStatus::Failed.display_name(),
        "retry_ok" => LogStatus::RetryOk.display_name(),
        "not_saved" => LogStatus::NotSaved.display_name(),
        _ => status_str,
    };

    let repo_name = repo_path
        .trim_end_matches("/.git")
        .rsplit('/')
        .next()
        .unwrap_or(repo_path);

    // Code lines read best as `<repo>/<remote>/<branch>`; token lines have no
    // repo or branch, so they show the platform/model instead.
    match field(&fields, "kind") {
        Some("token") => {
            print!(
                "{} {} [token] {} platform/model={}",
                time_display,
                status_display,
                truncate_for_display(ident),
                remote_branch
            );
        }
        _ => {
            print!(
                "{} {} [code] {} {}/{}",
                time_display,
                status_display,
                truncate_for_display(ident),
                repo_name,
                remote_branch
            );
        }
    }

    if let Some(record_id) = field(&fields, "record_id") {
        print!(" recordId={}", record_id);
    }

    if let Some(turn) = field(&fields, "turn") {
        print!(" turn={}", turn);
    }

    if let Some(r) = reason {
        let reason_text = reason_display(r);
        print!(" - {}", reason_text);
    }

    if let Some(msg) = field(&fields, "msg") {
        print!(" - {}", msg);
    }

    println!();
}
