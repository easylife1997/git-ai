//! Migration away from a legacy `~/.git-ai` installation.
//!
//! Older builds of this client were installed as `~/.git-ai`, and their hook
//! commands were written with the literal `~/.git-ai/bin/git-ai` path. Renaming
//! the app directory to `~/.easylife-ai` did not rewrite those files, and the
//! installers only recognise their own canonical path, so a machine that
//! upgraded in place ends up with:
//!
//! * a still-running daemon from the old directory (the new installer only
//!   stops the daemon belonging to the *current* install), and
//! * hook entries pointing at the old binary, sometimes *alongside* the new
//!   entry for the same event, so one event runs the checkpoint twice.
//!
//! This module detects both problems and repairs them. Every repair is backed
//! up first, and running it twice is a no-op.

use crate::error::GitAiError;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Name of the directory used by pre-rename installations.
pub(crate) const LEGACY_DIR_NAME: &str = ".git-ai";

/// Executable names this client has been installed under.
const CLIENT_BIN_NAMES: [&str; 2] = ["git-ai", "easylife-ai"];

/// Subcommands that identify a hook owned by this client.
const CLIENT_SUBCOMMANDS: [&str; 2] = ["checkpoint", "report-token-usage"];

#[derive(Debug, Default, Clone)]
pub struct MigrationReport {
    /// Legacy daemon that was running and got stopped.
    pub daemon_stopped: Option<u32>,
    /// Human-readable notes about the legacy daemon.
    pub daemon_notes: Vec<String>,
    /// Hook files that were rewritten, with the number of changed commands.
    pub hooks_rewritten: Vec<(PathBuf, usize)>,
    /// Files we could not read or parse; reported rather than silently skipped.
    pub errors: Vec<String>,
}

impl MigrationReport {
    pub fn has_changes(&self) -> bool {
        self.daemon_stopped.is_some() || !self.hooks_rewritten.is_empty()
    }
}

pub fn legacy_dir(home: &Path) -> PathBuf {
    home.join(LEGACY_DIR_NAME)
}

/// A hook command string is owned by us when it invokes one of our binaries
/// with one of our hook subcommands. Checking the shape rather than a fixed
/// path is what lets us find entries written before the rename.
fn our_hook_command(command: &str) -> bool {
    let mut parts = command.split_whitespace();
    let Some(exe) = parts.next() else {
        return false;
    };
    let Some(name) = Path::new(exe).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if !CLIENT_BIN_NAMES.contains(&name) {
        return false;
    }
    parts.any(|arg| CLIENT_SUBCOMMANDS.contains(&arg))
}

/// Rewrite one command to the current binary, keeping its arguments.
fn rebase_command(command: &str, binary: &Path) -> String {
    let mut parts = command.split_whitespace();
    let Some(_exe) = parts.next() else {
        return command.to_string();
    };
    let rest: Vec<&str> = parts.collect();
    let mut out = binary.to_string_lossy().to_string();
    for arg in rest {
        out.push(' ');
        out.push_str(arg);
    }
    out
}

/// Repoint every client hook command at `binary`.
fn rewrite_json_value(value: &mut Value, binary: &Path, changed: &mut usize) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(command)) = map.get("command").cloned().as_ref()
                && our_hook_command(command)
            {
                let rebased = rebase_command(command, binary);
                if rebased != *command {
                    map.insert("command".to_string(), Value::String(rebased));
                    *changed += 1;
                }
            }
            for (_, child) in map.iter_mut() {
                rewrite_json_value(child, binary, changed);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                rewrite_json_value(item, binary, changed);
            }
        }
        _ => {}
    }
}

/// Drop entries that still point into the legacy install directory once the
/// same event array also carries an entry from somewhere else. Scoped to a
/// single array (one event), so an unrelated event that only has legacy
/// entries is repointed rather than deleted.
fn dedupe_event_array(items: &mut Vec<Value>, legacy_root: &Path, removed: &mut usize) {
    let is_legacy = |command: &str| {
        our_hook_command(command)
            && command
                .split_whitespace()
                .next()
                .is_some_and(|exe| Path::new(exe).starts_with(legacy_root))
    };

    let has_newer = items.iter().any(|item| {
        item.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| our_hook_command(c) && !is_legacy(c))
    });
    if !has_newer {
        return;
    }

    let mut kept = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        let stale_duplicate = item
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_legacy);
        if stale_duplicate {
            *removed += 1;
        } else {
            kept.push(item);
        }
    }
    *items = kept;
}

/// Cursor (and Codex's JSON form) wrap each tool block as
/// `{"hooks":[{"command": ...}]}`. A whole block is redundant when every
/// command it holds is a legacy-path client command and a sibling block for
/// the same event is not. Without this, the event fires twice.
fn dedupe_tool_blocks(items: &mut Vec<Value>, legacy_root: &Path, removed: &mut usize) {
    let block_commands = |block: &Value| -> Vec<String> {
        block
            .get("hooks")
            .and_then(Value::as_array)
            .map(|hooks| {
                hooks
                    .iter()
                    .filter_map(|h| h.get("command").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let legacy_exe = |command: &str| {
        command
            .split_whitespace()
            .next()
            .is_some_and(|exe| Path::new(exe).starts_with(legacy_root))
    };

    let has_newer_block = items.iter().any(|block| {
        let commands = block_commands(block);
        !commands.is_empty()
            && commands
                .iter()
                .any(|c| our_hook_command(c) && !legacy_exe(c))
    });
    if !has_newer_block {
        return;
    }

    let mut kept = Vec::with_capacity(items.len());
    for block in items.drain(..) {
        let commands = block_commands(&block);
        let all_legacy = !commands.is_empty()
            && commands
                .iter()
                .all(|c| our_hook_command(c) && legacy_exe(c));
        if all_legacy {
            *removed += 1;
        } else {
            kept.push(block);
        }
    }
    *items = kept;
}

fn dedupe_json(value: &mut Value, legacy_root: &Path, removed: &mut usize) {
    match value {
        Value::Array(items) => {
            // `hooks.<event>` is an array of entries; nested `hooks` arrays
            // inside an entry hold the commands themselves.
            dedupe_event_array(items, legacy_root, removed);
            dedupe_tool_blocks(items, legacy_root, removed);
            for item in items.iter_mut() {
                dedupe_json(item, legacy_root, removed);
            }
        }
        Value::Object(map) => {
            for (_, child) in map.iter_mut() {
                dedupe_json(child, legacy_root, removed);
            }
        }
        _ => {}
    }
}

/// Candidate hook/config files that may embed an absolute client path.
fn candidate_files(home: &Path) -> Vec<PathBuf> {
    let mut files = vec![
        home.join(".claude/settings.json"),
        home.join(".codex/config.toml"),
        home.join(".codex/hooks.json"),
        home.join(".cursor/hooks.json"),
        home.join(".gemini/settings.json"),
        home.join(".github/hooks/git-ai.json"),
    ];
    files.extend([
        home.join(".config/opencode/opencode.json"),
        home.join(".config/opencode/plugins/git-ai.js"),
        home.join(".config/opencode/plugin/git-ai.js"),
    ]);
    files
}

/// Back up a file next to the app's backups directory before modifying it.
fn backup_file(path: &Path, backups_root: &Path) -> Result<PathBuf, GitAiError> {
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    fs::create_dir_all(backups_root)?;
    let target = backups_root.join(format!("legacy-migration-{}-{}", stamp, name));
    fs::copy(path, &target)?;
    Ok(target)
}

/// Fix a JSON hook file. Returns the number of rewritten or removed commands.
fn fix_json_file(
    path: &Path,
    binary: &Path,
    legacy_root: &Path,
    backups_root: &Path,
) -> Result<usize, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("读取失败：{}", e))?;
    let mut value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("JSON 解析失败：{}", e))?;

    let mut changed = 0usize;
    let mut removed = 0usize;
    // Dedupe first, while a stale path is still distinguishable from the
    // current one, then repoint whatever legitimate legacy entries remain.
    dedupe_json(&mut value, legacy_root, &mut removed);
    rewrite_json_value(&mut value, binary, &mut changed);

    if changed == 0 && removed == 0 {
        return Ok(0);
    }

    let rendered =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化失败：{}", e))?;
    backup_file(path, backups_root).map_err(|e| format!("备份失败：{}", e))?;
    fs::write(path, format!("{}\n", rendered)).map_err(|e| format!("写入失败：{}", e))?;
    Ok(changed + removed)
}

/// Fix a TOML config (Codex). Only `command = "..."` lines are touched so the
/// rest of the user's file stays byte-identical. Dedupe is scoped to a TOML
/// table (`[[hooks.Stop]]` etc.), because separate tables are separate events
/// that must each keep their own hook.
fn fix_toml_file(
    path: &Path,
    binary: &Path,
    legacy_root: &Path,
    backups_root: &Path,
) -> Result<usize, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("读取失败：{}", e))?;
    let current = binary.to_string_lossy().to_string();
    let is_legacy = |command: &str| {
        command
            .split_whitespace()
            .next()
            .is_some_and(|exe| Path::new(exe).starts_with(legacy_root))
    };
    let mut changed = 0usize;
    let mut section_has_current: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut section = String::new();

    // First pass: which tables already carry a non-legacy client hook?
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            section = trimmed.to_string();
            continue;
        }
        if let Some(command) = toml_command_value(trimmed)
            && our_hook_command(command)
            && !is_legacy(command)
        {
            section_has_current.insert(section.clone());
        }
    }

    let mut out = String::with_capacity(raw.len());
    section.clear();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            section = trimmed.to_string();
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(command) = toml_command_value(trimmed)
            && our_hook_command(command)
        {
            if command.starts_with(&current) {
                // Already correct.
            } else if is_legacy(command) && section_has_current.contains(&section) {
                changed += 1;
                continue; // Stale duplicate for an event that is already covered.
            } else {
                changed += 1;
                let indent = &line[..line.len() - trimmed.len()];
                out.push_str(&format!(
                    "{}command = \"{}\"\n",
                    indent,
                    rebase_command(command, binary)
                ));
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if changed == 0 {
        return Ok(0);
    }
    backup_file(path, backups_root).map_err(|e| format!("备份失败：{}", e))?;
    fs::write(path, out).map_err(|e| format!("写入失败：{}", e))?;
    Ok(changed)
}

/// Extract the string value of a `command = "..."` line.
fn toml_command_value(trimmed_line: &str) -> Option<&str> {
    let rest = trimmed_line.strip_prefix("command")?;
    let eq = rest.find('=')?;
    rest[eq + 1..]
        .trim()
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
}

/// Stop the daemon belonging to a legacy install, verifying the PID really is
/// that binary before signalling it (PIDs get recycled).
pub fn stop_legacy_daemon(home: &Path) -> (Option<u32>, Vec<String>) {
    let mut notes = Vec::new();
    let legacy = legacy_dir(home);
    if !legacy.exists() {
        return (None, notes);
    }

    let meta = legacy.join("internal/daemon/daemon.pid.json");
    let Some(pid) = read_pid(&meta) else {
        notes.push(format!("未找到旧 daemon PID 文件（{}）", meta.display()));
        return (None, notes);
    };

    let expected = legacy.join("bin").join("git-ai");
    match process_command(pid) {
        Some(command) => {
            let expected_str = expected.to_string_lossy().to_string();
            if !command.contains(&expected_str) {
                notes.push(format!(
                    "PID {} 不是旧客户端进程，已跳过（实际：{}）",
                    pid, command
                ));
                return (None, notes);
            }
        }
        None => {
            notes.push(format!("PID {} 已不存在，无需停止", pid));
            return (None, notes);
        }
    }

    // Soft shutdown via the legacy control socket, then escalate.
    let control = legacy.join("internal/daemon/control.sock");
    if crate::daemon::local_socket_connects_with_timeout(&control, Duration::from_millis(100))
        .is_ok()
    {
        let _ =
            crate::daemon::send_control_request(&control, &crate::daemon::ControlRequest::Shutdown);
    }
    if wait_for_exit(pid, Duration::from_secs(5)) {
        notes.push(format!("旧 daemon（PID {}）已优雅退出", pid));
        return (Some(pid), notes);
    }

    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    if wait_for_exit(pid, Duration::from_secs(5)) {
        notes.push(format!("旧 daemon（PID {}）已终止", pid));
        return (Some(pid), notes);
    }

    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
    if wait_for_exit(pid, Duration::from_secs(5)) {
        notes.push(format!("旧 daemon（PID {}）已被强制结束", pid));
        return (Some(pid), notes);
    }

    notes.push(format!("无法停止旧 daemon（PID {}），请手动处理", pid));
    (None, notes)
}

fn read_pid(meta: &Path) -> Option<u32> {
    let raw = fs::read_to_string(meta).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("pid")?.as_u64().map(|p| p as u32)
}

/// Full command line of a process, or `None` when it no longer exists.
fn process_command(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if process_command(pid).is_none() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop the legacy daemon and repoint every hook file at `binary`.
pub fn migrate(home: &Path, binary: &Path, dry_run: bool) -> MigrationReport {
    let mut report = MigrationReport::default();
    let legacy = legacy_dir(home);

    if !legacy.exists() {
        return report;
    }

    if dry_run {
        report.daemon_notes.push(format!(
            "发现旧安装目录 {}（dry-run，不修改）",
            legacy.display()
        ));
        return report;
    }

    let (stopped, notes) = stop_legacy_daemon(home);
    report.daemon_stopped = stopped;
    report.daemon_notes = notes;

    let backups_root = home.join(crate::config::APP_DIR_NAME).join("backups");

    for path in candidate_files(home) {
        if !path.is_file() {
            continue;
        }
        let result = if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            fix_toml_file(&path, binary, &legacy, &backups_root)
        } else {
            fix_json_file(&path, binary, &legacy, &backups_root)
        };
        match result {
            Ok(0) => {}
            Ok(count) => report.hooks_rewritten.push((path, count)),
            Err(e) => report.errors.push(format!("{}：{}", path.display(), e)),
        }
    }

    report
}

/// Human-readable summary for the installer output.
pub fn print_report(report: &MigrationReport) {
    if let Some(pid) = report.daemon_stopped {
        println!("\x1b[1;32m✓ 已停止旧版后台服务\x1b[0m (PID {})", pid);
    }
    for note in &report.daemon_notes {
        println!("\x1b[1;33m⚠ {}\x1b[0m", note);
    }
    for (path, count) in &report.hooks_rewritten {
        println!(
            "\x1b[1;32m✓ 已统一 hook 指向\x1b[0m {}（{} 处）",
            path.display(),
            count
        );
    }
    for error in &report.errors {
        println!("\x1b[1;33m⚠ 迁移跳过：{}\x1b[0m", error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// A machine with a leftover install directory is the only case that gets
    /// migrated at all, so every fixture needs one.
    fn with_legacy_dir(home: &Path) {
        fs::create_dir_all(home.join(".git-ai")).unwrap();
    }

    #[test]
    fn our_hook_command_matches_both_names_only() {
        assert!(our_hook_command(
            "/Users/x/.git-ai/bin/git-ai checkpoint cursor"
        ));
        assert!(our_hook_command(
            "/Users/x/.easylife-ai/bin/easylife-ai report-token-usage codex"
        ));
        assert!(!our_hook_command("/usr/bin/notify-send Codex"));
        assert!(!our_hook_command("/Users/x/.git-ai/bin/git-ai blame"));
        assert!(!our_hook_command(""));
    }

    #[test]
    fn cursor_duplicate_is_collapsed_to_current_binary() {
        let home = tempfile::tempdir().unwrap();
        with_legacy_dir(home.path());
        let path = home.path().join(".cursor/hooks.json");
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");
        let old_exe = home.path().join(".git-ai/bin/git-ai");
        write(
            &path,
            &format!(
                r#"{{"version":1,"hooks":{{"postToolUse":[
                 {{"command":"{new} checkpoint cursor --hook-input stdin"}},
                 {{"command":"{old} checkpoint cursor --hook-input stdin"}}],
               "afterFileEdit":[
                 {{"command":"{old} checkpoint cursor --hook-input stdin"}}]}}}}"#,
                new = binary.display(),
                old = old_exe.display()
            ),
        );

        let report = migrate(home.path(), &binary, false);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.hooks_rewritten.len(), 1);

        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let post = parsed["hooks"]["postToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1, "duplicate must be collapsed");
        assert!(
            post[0]["command"]
                .as_str()
                .unwrap()
                .starts_with(&binary.to_string_lossy().to_string())
        );
        let after = parsed["hooks"]["afterFileEdit"].as_array().unwrap();
        assert_eq!(after.len(), 1);
        assert!(
            after[0]["command"]
                .as_str()
                .unwrap()
                .contains(".easylife-ai/bin/easylife-ai"),
            "legacy entry must be repointed"
        );
        assert!(!fs::read_to_string(&path).unwrap().contains(".git-ai/bin"));
    }

    #[test]
    fn codex_toml_is_repointed_and_user_keys_preserved() {
        let home = tempfile::tempdir().unwrap();
        with_legacy_dir(home.path());
        let path = home.path().join(".codex/config.toml");
        let old_exe = home.path().join(".git-ai/bin/git-ai");
        write(
            &path,
            &format!(
                "model = \"gpt-5\"\nnotify = [\"notify-send\"]\n\n[[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ncommand = \"{} checkpoint codex --hook-input stdin\"\ntype = \"command\"\n",
                old_exe.display()
            ),
        );
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");
        let report = migrate(home.path(), &binary, false);
        assert_eq!(report.hooks_rewritten.len(), 1);

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("model = \"gpt-5\""));
        assert!(updated.contains("notify = [\"notify-send\"]"));
        assert!(updated.contains("type = \"command\""));
        assert!(!updated.contains(".git-ai/bin/git-ai"));
        assert!(updated.contains(".easylife-ai/bin/easylife-ai checkpoint codex"));
    }

    #[test]
    fn migration_is_idempotent_and_backs_up_before_writing() {
        let home = tempfile::tempdir().unwrap();
        with_legacy_dir(home.path());
        let path = home.path().join(".cursor/hooks.json");
        let old_exe = home.path().join(".git-ai/bin/git-ai");
        write(
            &path,
            &format!(
                r#"{{"hooks":{{"preToolUse":[{{"command":"{} checkpoint cursor"}}]}}}}"#,
                old_exe.display()
            ),
        );
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");

        let first = migrate(home.path(), &binary, false);
        assert_eq!(first.hooks_rewritten.len(), 1);
        let backups: Vec<_> = fs::read_dir(home.path().join(".easylife-ai/backups"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].starts_with("legacy-migration-"));

        let second = migrate(home.path(), &binary, false);
        assert!(
            second.hooks_rewritten.is_empty(),
            "second run must be a no-op"
        );
        assert!(!second.has_changes());
    }

    #[test]
    fn migration_without_legacy_dir_does_nothing() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".cursor/hooks.json");
        write(&path, r#"{"hooks":{}}"#);
        let before = fs::read_to_string(&path).unwrap();
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");

        let report = migrate(home.path(), &binary, false);
        assert!(!report.has_changes());
        assert!(report.daemon_notes.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn dry_run_reports_without_modifying() {
        let home = tempfile::tempdir().unwrap();
        with_legacy_dir(home.path());
        let path = home.path().join(".cursor/hooks.json");
        let original = format!(
            r#"{{"hooks":{{"preToolUse":[{{"command":"{} checkpoint cursor"}}]}}}}"#,
            home.path().join(".git-ai/bin/git-ai").display()
        );
        write(&path, &original);
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");

        let report = migrate(home.path(), &binary, true);
        assert!(report.hooks_rewritten.is_empty());
        assert!(
            !report.daemon_notes.is_empty(),
            "dry-run should say what it found"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn unparseable_file_is_reported_not_ignored() {
        let home = tempfile::tempdir().unwrap();
        with_legacy_dir(home.path());
        let path = home.path().join(".cursor/hooks.json");
        write(&path, "{ not json");
        let binary = home.path().join(".easylife-ai/bin/easylife-ai");

        let report = migrate(home.path(), &binary, false);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
