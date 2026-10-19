//! `easylife-ai doctor` -- end-to-end self check for the reporting pipelines.
//!
//! Verifies that both upload paths work from *this* machine, the same way a
//! real user triggers them: a real `git push` through the PATH shim for code
//! stats, and a real Stop-hook invocation for token usage.  Nothing here is a
//! dry run -- the data is really sent and really stored on the server.
//!
//! See `docs/easylife-ai doctor.md` for the full design.

use crate::commands::tracker::config as tracker_config;
use crate::commands::tracker::log;
use crate::config::APP_DIR_NAME;
use crate::mdm::agents::get_all_installers;
use crate::mdm::hook_installer::HookInstallerParams;
use crate::mdm::utils::{get_current_binary_path, home_dir};
use serde_json::json;
use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const TOTAL_STEPS: usize = 7;
static JSON_MODE: AtomicBool = AtomicBool::new(false);

/// File holding the last run's plain-text transcript, read by `ops-check`.
const REPORT_FILE_NAME: &str = "doctor-last-report.txt";

/// Everything this run printed, minus escape codes.
///
/// `ops-check` embeds this verbatim in the prompt it hands to an external
/// agent, so a user who just ran `doctor` does not have to paste output by hand.
static REPORT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The run's progress bar, reachable from the free-standing output helpers.
///
/// The bar draws itself on a timer, so printing to the terminal directly would
/// interleave with it and leave step text glued to a redraw on the same line.
/// Routing every line through `ProgressBar::println` keeps the bar intact and
/// puts text above it, in order.
static PROGRESS_BAR: std::sync::OnceLock<indicatif::ProgressBar> = std::sync::OnceLock::new();

/// How long to wait for the daemon to flush a report into the upload log.
const REPORT_WAIT: Duration = Duration::from_secs(30);
const REPORT_POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Options {
    repo_url: Option<String>,
    no_fix: bool,
    keep: bool,
    json: bool,
    /// Skip writing the transcript that `ops-check` reads back.
    no_save: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Options::default();
        let mut iter = args.iter();

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--repo-url" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--repo-url requires a value".to_string())?;
                    opts.repo_url = Some(value.clone());
                }
                "--no-fix" => opts.no_fix = true,
                "--keep" => opts.keep = true,
                "--json" => opts.json = true,
                "--no-save" => opts.no_save = true,
                other => {
                    if let Some(value) = other.strip_prefix("--repo-url=") {
                        opts.repo_url = Some(value.to_string());
                    } else {
                        return Err(format!("unknown argument: {}", other));
                    }
                }
            }
        }

        Ok(opts)
    }
}

/// Outcome of one reporting attempt, fed into the final summary.
#[derive(Clone)]
struct CheckResult {
    name: String,
    ok: bool,
    detail: String,
    /// A failure the user cannot fix by changing their usage (drives exit 1).
    blocking: bool,
    /// The check could not run at all, so nothing was verified either way.
    skipped: bool,
}

struct Doctor {
    opts: Options,
    checks: Vec<CheckResult>,
    /// Lines already present in the upload log when the run started.
    log_offset: u64,
    temp_dir: Option<PathBuf>,
    /// Progress bar, driven alongside the textual step headers.
    bar: Option<indicatif::ProgressBar>,
}

impl Doctor {
    fn new(opts: Options) -> Self {
        Self {
            opts,
            checks: Vec::new(),
            log_offset: current_log_len(),
            temp_dir: None,
            bar: None,
        }
    }

    fn record(&mut self, name: &str, ok: bool, blocking: bool, detail: impl Into<String>) {
        self.checks.push(CheckResult {
            name: name.to_string(),
            ok,
            blocking,
            detail: detail.into(),
            skipped: false,
        });
    }

    /// Record a check that never ran. Kept distinct from a pass so the summary
    /// cannot claim a pipeline "works" when it was never exercised.
    fn record_skipped(&mut self, name: &str, detail: impl Into<String>) {
        self.checks.push(CheckResult {
            name: name.to_string(),
            ok: true,
            blocking: false,
            detail: detail.into(),
            skipped: true,
        });
    }

    fn has_failure(&self) -> bool {
        self.checks.iter().any(|c| !c.ok && c.blocking)
    }

    /// Advance the progress bar and update the label shown next to it.
    fn advance(&mut self, position: usize, message: &str) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.to_string());
            bar.set_position(position as u64);
        }
    }
}

fn progress_line(message: &str) {
    // Record the plain-text form for the report file: the transcript is read by
    // an external agent, which should not have to parse ANSI escapes.
    if let Ok(mut report) = REPORT.lock() {
        report.push(strip_ansi(message));
    }
    if JSON_MODE.load(Ordering::Relaxed) {
        eprintln!("{}", message);
        return;
    }
    // Print *through* the bar: `suspend` clears it, runs our print, then
    // redraws. Printing directly would let the ticker redraw over our text and
    // leave a step header glued to a progress line.
    match PROGRESS_BAR.get() {
        Some(bar) => bar.suspend(|| println!("{}", message)),
        None => println!("{}", message),
    }
}

/// Drop SGR sequences (`ESC[...m`) so the saved report is readable as text.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // Consume up to and including the final byte of the escape sequence.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// Where the transcript of the last run is kept.
pub fn report_path() -> PathBuf {
    home_dir().join(APP_DIR_NAME).join(REPORT_FILE_NAME)
}

/// Read the last saved transcript, if any.
pub fn read_last_report() -> Option<String> {
    let path = report_path();
    let text = fs::read_to_string(&path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// Persist this run's transcript for `ops-check` to pick up later.
///
/// A failed write is reported but never fatal: the diagnosis itself already
/// happened, and losing the transcript only costs the user a manual copy.
fn save_report() -> Result<PathBuf, String> {
    let path = report_path();
    let body = {
        let report = REPORT.lock().map_err(|_| "报告缓冲区不可用".to_string())?;
        report.join("\n")
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{}", e))?;
    }
    fs::write(&path, format!("{}\n", body.trim_end())).map_err(|e| format!("写入失败：{}", e))?;
    Ok(path)
}

fn step_header(index: usize, title: &str) {
    progress_line(&format!(
        "\n\x1b[1m[{}/{}] {}\x1b[0m",
        index, TOTAL_STEPS, title
    ));
}

fn info(message: &str) {
    progress_line(&format!("      {}", message));
}

fn ok(message: &str) {
    progress_line(&format!("      \x1b[1;32m✓\x1b[0m {}", message));
}

fn warn(message: &str) {
    progress_line(&format!("      \x1b[1;33m⚠\x1b[0m {}", message));
}

fn fail(message: &str) {
    progress_line(&format!("      \x1b[1;31m✗\x1b[0m {}", message));
}

fn skipped(message: &str) {
    progress_line(&format!("      \x1b[90m⊘\x1b[0m {}", message));
}

/// Same marker as `skipped`, named for the summary where the word "skipped"
/// reads as a verb rather than a label.
fn skipped_line(message: &str) {
    skipped(message);
}

pub fn handle_doctor(args: &[String]) {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: easylife-ai doctor [--repo-url <url>] [--no-fix] [--keep] [--json] [--no-save]"
        );
        println!("真实验证代码与 3 轮 Claude Code token 上报，并自动检查/修复 hook 与插件。");
        println!(
            "--repo-url 使用独立 doctor/<id> 分支；--json 将进度写入 stderr、JSON 写入 stdout。"
        );
        println!(
            "诊断结果默认保存到 ~/{APP_DIR_NAME}/{REPORT_FILE_NAME}，供 `easylife-ai ops-check` 引用；--no-save 关闭。",
        );
        return;
    }
    let opts = match Options::parse(args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("easylife-ai doctor: {}", e);
            eprintln!("Usage: easylife-ai doctor [--repo-url <url>] [--no-fix] [--keep] [--json]");
            std::process::exit(2);
        }
    };

    let json_output = opts.json;
    JSON_MODE.store(json_output, Ordering::Relaxed);

    // Validate arguments before anything is sent: a bad `--repo-url` must not
    // produce a partial run that already uploaded real data to the server.
    if let Some(url) = opts.repo_url.as_deref()
        && let Err(e) = reject_credentials(url)
    {
        eprintln!("easylife-ai doctor: {}", e);
        std::process::exit(2);
    }

    // Only drive a real bar on an interactive terminal. indicatif discards all
    // drawing when its target is not a TTY, and routing our text through it
    // would discard that text too.
    if !json_output && std::io::stdout().is_terminal() {
        // Draw on stdout, the same stream our text uses: the bar and the step
        // output must share one target or the redraws cannot be coordinated.
        let bar = indicatif::ProgressBar::with_draw_target(
            Some(TOTAL_STEPS as u64),
            indicatif::ProgressDrawTarget::stdout(),
        );
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.green} [{bar:28.cyan/blue}] {pos}/{len} {msg}",
            )
            .expect("valid progress template"),
        );
        bar.enable_steady_tick(Duration::from_millis(120));
        let _ = PROGRESS_BAR.set(bar);
    }
    let mut doctor = Doctor::new(opts);
    if let Some(bar) = PROGRESS_BAR.get() {
        doctor.bar = Some(bar.clone());
    }

    progress_line("\x1b[1measylife-ai doctor\x1b[0m —— 上报链路自检");
    info("说明：本命令会真实上报数据到服务端，并自动修复缺失的 hook/插件。");

    // The bar position is the step being worked on (1-based), so the label next
    // to it always matches the `[n/7]` header that step prints.
    doctor.advance(1, "环境自检");
    let env_ok = doctor.step_environment();
    doctor.advance(2, "配置检查");
    let config = doctor.step_config();
    doctor.advance(3, "hook 与插件检查");
    doctor.step_hooks();
    doctor.advance(4, "网络连通性");

    let Some(config) = config else {
        doctor.advance(7, "结论");
        doctor.finish(json_output);
        return;
    };

    doctor.step_connectivity(&config);
    doctor.advance(5, "代码上报");
    doctor.step_code_report();
    doctor.advance(6, "token 上报（3 轮）");
    doctor.step_token_report(config.token_report);
    doctor.advance(7, "结论");

    if !env_ok {
        warn("环境自检存在问题，上报结果可能受影响");
    }

    doctor.finish(json_output);
}

impl Doctor {
    /// Step 1: client version, git availability, PATH shim.
    fn step_environment(&mut self) -> bool {
        step_header(1, "环境自检");

        ok(&format!("客户端版本 {}", env!("CARGO_PKG_VERSION")));

        let git_ok = Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if git_ok {
            ok("git 可用");
        } else {
            fail("git 不可用：找不到可执行的 git");
            self.record("environment", false, true, "git 不可用");
            return false;
        }

        // The shim must win on PATH, otherwise `git push` never reaches us.
        let resolved_git = resolve_git_on_path();
        let shim = home_dir()
            .join(crate::config::APP_DIR_NAME)
            .join("bin")
            .join("git");
        match &resolved_git {
            Some(path) if is_our_shim(path) => {
                ok(&format!("PATH shim 就位（{}）", path.display()));
                true
            }
            Some(path) => {
                fail(&format!(
                    "PATH 中的 git 不是 easylife-ai shim：{}",
                    path.display()
                ));
                info(&format!("  期望：{}", shim.display()));
                info(
                    "  修复：把 easylife-ai 的 bin 目录放到 PATH 最前面，或重新运行 easylife-ai install-hooks",
                );
                self.record("environment", false, true, "PATH shim 未就位");
                false
            }
            None => {
                fail("PATH 中找不到 git");
                self.record("environment", false, true, "PATH 中找不到 git");
                false
            }
        }
    }

    /// Step 2: tracker-config.json completeness.
    fn step_config(&mut self) -> Option<tracker_config::TrackerConfig> {
        step_header(2, "配置校验");

        let path = tracker_config::config_path();
        let config = match tracker_config::load_config() {
            Some(c) => c,
            None => {
                fail(&format!("未找到配置文件：{}", path.display()));
                info("  修复：先完成登录/初始化，使配置文件生成");
                self.record("config", false, true, "缺少 tracker-config.json");
                return None;
            }
        };

        let mut missing = Vec::new();
        if config.tracker_url.trim().is_empty() {
            missing.push("tracker_url");
        }
        if config.team_id.trim().is_empty() {
            missing.push("team_id");
        } else if config.team_id.parse::<i64>().is_err() {
            fail(&format!("team_id 不是合法整数：{}", config.team_id));
            info("  team_id 解析失败时 token 上报会静默退出");
            missing.push("team_id");
        }
        if config.team_key.trim().is_empty() {
            missing.push("team_key");
        }
        let username = config.username.as_deref().unwrap_or("");
        if username.trim().is_empty() {
            missing.push("username");
        }

        if missing.is_empty() {
            ok(&format!("{} 齐全", path.display()));
            ok(&format!("tracker_url = {}", config.tracker_url));
            ok(&format!("team_id = {}", config.team_id));
            ok(&format!("username = {}", username));
            Some(config)
        } else {
            fail(&format!("配置缺少字段：{}", missing.join(", ")));
            self.record(
                "config",
                false,
                true,
                format!("缺少 {}", missing.join(", ")),
            );
            None
        }
    }

    /// Step 3: hook / plugin status for every supported agent, plus repair.
    fn step_hooks(&mut self) {
        step_header(3, "hook 与插件检查");

        let binary_path = match get_current_binary_path() {
            Ok(p) => p,
            Err(e) => {
                fail(&format!("无法定位当前二进制：{}", e));
                self.record("hooks", false, true, "无法定位当前二进制");
                return;
            }
        };
        let params = HookInstallerParams {
            binary_path: binary_path.clone(),
        };

        let installers = get_all_installers();
        info(&format!(
            "正在检查 {} 个 AI 工具的 hook/插件配置 ...",
            installers.len()
        ));

        let mut needing_fix: Vec<(String, String)> = Vec::new();
        let mut detected = 0usize;
        let mut healthy = 0usize;

        for installer in &installers {
            let name = installer.name().to_string();
            match installer.check_hooks(&params) {
                Ok(result) => {
                    if !result.tool_installed {
                        skipped(&format!("{:<20}未安装该工具", name));
                        continue;
                    }
                    detected += 1;

                    // Plugin-only agents (no config-file hooks) always report
                    // `hooks_installed: false` by design -- their state lives in
                    // a plugin, not a hook file, so it cannot be judged here.
                    if !installer.uses_config_hooks() {
                        if installer.check_extras(&params).unwrap_or(false) {
                            ok(&format!("{:<20}插件已安装", name));
                            healthy += 1;
                        } else {
                            fail(&format!("{:<20}插件缺失或 IDE 不兼容", name));
                            needing_fix.push((installer.id().to_string(), name));
                        }
                        continue;
                    }

                    if result.hooks_installed && result.hooks_up_to_date {
                        ok(&format!("{:<20}已配置且最新", name));
                        healthy += 1;
                    } else if result.hooks_installed {
                        warn(&format!("{:<20}已配置但版本过旧（二进制路径已变更）", name));
                        needing_fix.push((installer.id().to_string(), name));
                    } else {
                        fail(&format!("{:<20}hook 缺失", name));
                        needing_fix.push((installer.id().to_string(), name));
                    }
                }
                Err(e) => {
                    warn(&format!("{:<20}检查失败：{}", name, e));
                }
            }
        }

        if detected == 0 {
            warn("没有检测到任何已安装的 AI 工具；跳过 hook 修复，仍验证上报接口");
            self.record("hooks", true, false, "未检测到 AI 工具");
            return;
        }

        if needing_fix.is_empty() {
            ok(&format!("{} 个工具配置正常", healthy));
            self.record("hooks", true, false, format!("{} 个工具正常", healthy));
            return;
        }

        if self.opts.no_fix {
            warn(&format!(
                "{} 个工具需要修复（已指定 --no-fix，跳过自动修复）",
                needing_fix.len()
            ));
            self.record(
                "hooks",
                false,
                false,
                format!("{} 个工具待修复", needing_fix.len()),
            );
            return;
        }

        info("正在自动修复 ...");
        // Invoke the normal CLI entry point, but capture its verbose installer
        // output so `--json` remains valid JSON on stdout.
        if let Err(e) = run_install_hooks_captured() {
            fail(&format!("自动修复失败：{}", e));
            self.record("hooks", false, true, format!("自动修复失败：{}", e));
            return;
        }

        // Verify by re-checking the real state rather than trusting the
        // returned status map: the map omits plugin-only agents entirely.
        let mut still_broken = Vec::new();
        for (id, name) in &needing_fix {
            let installer = installers.iter().find(|i| i.id() == id);
            let fixed = installer
                .map(|i| {
                    if i.uses_config_hooks() {
                        i.check_hooks(&params)
                            .map(|r| r.hooks_installed && r.hooks_up_to_date)
                            .unwrap_or(false)
                    } else {
                        i.check_extras(&params).unwrap_or(false)
                    }
                })
                .unwrap_or(false);

            if fixed {
                ok(&format!("{:<20}已修复", name));
            } else {
                fail(&format!("{:<20}修复后仍未生效", name));
                still_broken.push(name.clone());
            }
        }

        if still_broken.is_empty() {
            self.record(
                "hooks",
                true,
                false,
                format!("已修复 {} 个工具", needing_fix.len()),
            );
        } else {
            self.record(
                "hooks",
                false,
                true,
                format!("修复失败：{}", still_broken.join(", ")),
            );
        }
    }

    /// Step 4: TCP reachability of the tracker host. No HTTP request is sent --
    /// any POST would create data, and a reachability probe should not.
    fn step_connectivity(&mut self, config: &tracker_config::TrackerConfig) {
        step_header(4, "连通性");
        info(&format!("正在探测 {} ...", config.tracker_url));

        match probe_tcp(&config.tracker_url) {
            Ok(addr) => {
                ok(&format!("TCP 可达（{}）", addr));
                self.record("connectivity", true, false, format!("TCP 可达 {}", addr));
            }
            Err(e) => {
                fail(&format!("TCP 不可达：{}", e));
                info("  修复：确认网络、代理、防火墙，以及 tracker_url 是否正确");
                self.record("connectivity", false, true, format!("TCP 不可达：{}", e));
            }
        }
    }

    /// Step 5: real commit + real push, through the PATH shim.
    fn step_code_report(&mut self) {
        step_header(5, "代码上报");

        match self.run_code_report() {
            Ok(detail) => {
                self.record("code_report", true, false, detail);
            }
            Err(e) => {
                fail(&e);
                self.record("code_report", false, true, e);
            }
        }
    }

    fn run_code_report(&mut self) -> Result<String, String> {
        let id = random_id();
        let root = std::env::temp_dir().join(format!("easylife-ai-doctor-{}", id));
        fs::create_dir_all(&root).map_err(|e| format!("创建临时目录失败：{}", e))?;
        self.temp_dir = Some(root.clone());

        let work = root.join("work");
        let custom_remote = self.opts.repo_url.is_some();
        let remote_name = if custom_remote {
            "__doctor".to_string()
        } else {
            "origin".to_string()
        };
        let branch_name = if custom_remote {
            format!("doctor/{}", id)
        } else {
            "main".to_string()
        };
        let remote_desc;
        let mut remote_cleanup = None;
        let mut needs_baseline_commit = false;

        if let Some(url) = self.opts.repo_url.as_deref() {
            remote_desc = url.to_string();
            info(&format!("正在\"{}\"仓库模仿用户上报代码", remote_desc));
            let output = Command::new("git")
                .args(["clone", "--quiet", "--depth", "1", url])
                .arg(&work)
                .output()
                .map_err(|e| format!("git clone 失败：{}", e))?;
            if !output.status.success() {
                return Err(format!(
                    "git clone 失败：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }

            git_in(&work, &["remote", "rename", "origin", &remote_name])?;
            git_in(&work, &["config", "user.name", "easylife-ai doctor"])?;
            git_in(&work, &["config", "user.email", "doctor@easylife-ai.local"])?;
            let has_head = Command::new("git")
                .current_dir(&work)
                .args(["rev-parse", "--verify", "HEAD"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if has_head {
                git_in(&work, &["checkout", "--quiet", "-b", &branch_name])?;
            } else {
                git_in(&work, &["checkout", "--quiet", "--orphan", &branch_name])?;
                needs_baseline_commit = true;
            }

            remote_cleanup = Some(RemoteBranchCleanup::new(
                work.clone(),
                remote_name.clone(),
                branch_name.clone(),
            ));
        } else {
            let bare = root.join("remote.git");
            let output = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&bare)
                .output()
                .map_err(|e| format!("git init --bare 失败：{}", e))?;
            if !output.status.success() {
                return Err(format!(
                    "git init --bare 失败：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            remote_desc = bare.to_string_lossy().to_string();
            info(&format!("正在\"{}\"仓库模仿用户上报代码", remote_desc));
            fs::create_dir_all(&work).map_err(|e| format!("创建工作仓库失败：{}", e))?;
            git_in(&work, &["init", "--quiet"])?;
            git_in(&work, &["checkout", "--quiet", "-b", &branch_name])?;
            git_in(&work, &["remote", "add", &remote_name, &remote_desc])?;
            git_in(&work, &["config", "user.name", "easylife-ai doctor"])?;
            git_in(&work, &["config", "user.email", "doctor@easylife-ai.local"])?;
            needs_baseline_commit = true;
        }

        // Seed a baseline ref before the test commit. This makes the next push
        // an update to an existing branch, which is what the tracker compares.
        if needs_baseline_commit {
            let baseline = format!("基线文件，由 easylife-ai doctor 生成（{}）\n", id);
            fs::write(work.join("README.md"), baseline)
                .map_err(|e| format!("写入基线文件失败：{}", e))?;
            git_in(&work, &["add", "-A"])?;
            git_in(&work, &["commit", "--quiet", "-m", "doctor: 基线提交"])?;
        }
        if custom_remote {
            // An empty lease requires the remote ref not to exist. Never
            // update a coincidentally pre-existing doctor branch.
            git_in(
                &work,
                &[
                    "push",
                    "--quiet",
                    "-u",
                    &format!("--force-with-lease=refs/heads/{}:", branch_name),
                    &remote_name,
                    &branch_name,
                ],
            )?;
        } else {
            git_in(
                &work,
                &["push", "--quiet", "-u", &remote_name, &branch_name],
            )?;
        }

        if let Some(cleanup) = remote_cleanup.as_mut() {
            cleanup.mark_branch_created();
        }

        // The real payload: a randomly generated .py file.  `.py` is in the
        // diff allowlist, so the uploaded diff is non-empty.
        let file_name = format!("doctor-{}.py", id);
        let body = random_python(&id);
        info(&format!(
            "自动化新建文件{}，内容随机生成，{} 字",
            file_name,
            body.chars().count()
        ));
        fs::write(work.join(&file_name), &body).map_err(|e| format!("写入文件失败：{}", e))?;

        info("正在生成可提交内容");
        git_in(&work, &["add", "-A"])?;

        info("正在提交");
        git_in(
            &work,
            &[
                "commit",
                "--quiet",
                "-m",
                &format!("doctor: 上报链路自检 {}", id),
            ],
        )?;

        let head = git_capture(&work, &["rev-parse", "HEAD"])?;
        let head = head.trim().to_string();

        info("正在推送");
        // Deliberately spawn plain `git push`: PATH resolution takes it through
        // the easylife-ai shim, exactly like a real user's push.
        let push = Command::new("git")
            .current_dir(&work)
            .args(["push", "--quiet", &remote_name, &branch_name])
            .output()
            .map_err(|e| format!("git push 失败：{}", e))?;
        if !push.status.success() {
            return Err(format!(
                "git push 失败：{}",
                String::from_utf8_lossy(&push.stderr).trim()
            ));
        }

        info("检查是否正确上报 ...");
        let entry = wait_for_code_log_entry(&self.log_offset, &head, REPORT_WAIT);
        let cleanup_error = remote_cleanup.and_then(|mut cleanup| cleanup.cleanup().err());
        if let Some(error) = &cleanup_error {
            warn(&format!(
                "远端 doctor 分支 {} 清理失败：{}",
                branch_name, error
            ));
        }

        match entry {
            Some(line) => {
                let fields = parse_log_fields(&line);
                if fields.get("saved").map(String::as_str) == Some("true") {
                    let record = fields
                        .get("record_id")
                        .map(|r| format!("（recordId={}）", r))
                        .unwrap_or_default();
                    ok(&format!("上报成功{}", record));
                    let cleanup_note = cleanup_error
                        .map(|e| format!("；远端 doctor 分支清理失败：{}", e))
                        .unwrap_or_default();
                    if !cleanup_note.is_empty() {
                        warn(&cleanup_note);
                    }
                    Ok(format!("代码上报成功{}{}", record, cleanup_note))
                } else {
                    let msg = fields
                        .get("msg")
                        .or_else(|| fields.get("reason"))
                        .cloned()
                        .unwrap_or_else(|| "服务端未入库".to_string());
                    warn(&format!("上报未入库：{}", msg));
                    Err(format!("代码上报未入库：{}", msg))
                }
            }
            None => {
                fail("未在日志中发现本次代码上报记录");
                info("  可能原因：push 未经过 easylife-ai shim、hook 未生效、daemon 未运行");
                let cleanup_note = cleanup_error
                    .map(|e| format!("；远端 doctor 分支清理失败：{}", e))
                    .unwrap_or_default();
                Err(format!("未观察到代码上报{}", cleanup_note))
            }
        }
    }

    /// Step 6: three simulated Claude Code turns through the real Stop hook.
    fn step_token_report(&mut self, enabled: bool) {
        step_header(6, "token 上报（3 轮）");
        if !enabled {
            let detail = "已禁用：token_report=false，跳过 token 上报验证（未验证）";
            skipped(detail);
            self.record_skipped("token_report", detail);
            return;
        }

        match self.run_token_report() {
            Ok(detail) => self.record("token_report", true, false, detail),
            Err(e) => {
                fail(&e);
                self.record("token_report", false, true, e);
            }
        }
    }

    fn run_token_report(&mut self) -> Result<String, String> {
        const TURNS: usize = 3;
        info("正在模仿 Claude Code 使用");

        let id = random_id();
        let root = self
            .temp_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join(format!("easylife-ai-doctor-{}", id)));
        let transcripts = root.join("transcripts");
        fs::create_dir_all(&transcripts).map_err(|e| format!("创建 transcript 目录失败：{}", e))?;

        let session_id = format!("doctor-{}", id);
        let model = "claude-sonnet-4-5-20250929";
        let transcript = transcripts.join(format!("{}.jsonl", session_id));
        info(&format!("生成 {} 轮模拟会话", TURNS));

        let mut lines = String::new();
        for turn in 1..=TURNS {
            let prompt = format!(
                "doctor 自检第 {} 轮：请确认本机上报链路是否正常（{}）",
                turn, id
            );
            let user_line = json!({
                "type": "user",
                "promptSource": "typed",
                "sessionId": session_id,
                "message": { "role": "user", "content": prompt },
                "timestamp": "2026-01-01T00:00:00.000Z",
            });
            let assistant_line = json!({
                "type": "assistant",
                "sessionId": session_id,
                "message": {
                    "id": format!("msg_doctor_{}_{}", id, turn),
                    "role": "assistant",
                    "model": model,
                    "stop_reason": "end_turn",
                    "content": [{ "type": "text", "text": format!("doctor 自检回复第 {} 轮", turn) }],
                    "usage": {
                        "input_tokens": 1000 + turn,
                        "output_tokens": 200 + turn,
                        "cache_read_input_tokens": 50 + turn,
                        "cache_creation_input_tokens": 10 + turn,
                    },
                },
                "timestamp": "2026-01-01T00:00:01.000Z",
            });
            lines.push_str(&user_line.to_string());
            lines.push('\n');
            lines.push_str(&assistant_line.to_string());
            lines.push('\n');
        }
        fs::write(&transcript, lines).map_err(|e| format!("写入 transcript 失败：{}", e))?;

        info("正在上报 token 用量");
        // Drive the real Stop-hook entry point: transcript_path arrives on
        // stdin, so the fixture never has to live under ~/.claude/projects.
        let payload = json!({
            "session_id": session_id,
            "transcript_path": transcript.to_string_lossy(),
            "cwd": root.to_string_lossy(),
            "hook_event_name": "Stop",
        });

        let exe = get_current_binary_path().map_err(|e| format!("无法定位二进制：{}", e))?;
        let mut child = Command::new(&exe)
            .args(["report-token-usage", "claude-code"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 report-token-usage 失败：{}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.to_string().as_bytes());
        }

        let output = child
            .wait_with_output()
            .map_err(|e| format!("等待 report-token-usage 失败：{}", e))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !stdout.is_empty() {
            info(&stdout);
        }
        if !output.status.success() {
            return Err(format!(
                "report-token-usage 退出码 {}：{}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        info("检查是否正确上报 ...");
        let entries =
            wait_for_log_entries(&self.log_offset, "token", &session_id, TURNS, REPORT_WAIT);
        if entries.is_empty() {
            return Err("未在日志中发现本次 token 上报记录".to_string());
        }

        let saved: Vec<&String> = entries
            .iter()
            .filter(|line| parse_log_fields(line).get("saved").map(String::as_str) == Some("true"))
            .collect();

        let record = saved
            .first()
            .and_then(|line| parse_log_fields(line).get("record_id").cloned())
            .map(|r| format!("，recordId={}", r))
            .unwrap_or_default();

        if saved.len() == TURNS {
            ok(&format!("上报成功（{} 轮{}）", saved.len(), record));
            Ok(format!("token 上报成功，{} 轮", saved.len()))
        } else if saved.is_empty() {
            Err("token 上报未入库".to_string())
        } else {
            Err(format!("token 上报仅 {} / {} 轮入库", saved.len(), TURNS))
        }
    }

    /// Step 7: summary and exit code.
    fn finish(&mut self, json_output: bool) {
        step_header(7, "结论");

        // Clear the bar before the summary so it cannot overwrite those lines.
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
        let _ = PROGRESS_BAR.get().map(|bar| bar.finish_and_clear());

        let temp_dir = self.temp_dir.clone();
        let failed: Vec<&CheckResult> = self.checks.iter().filter(|c| !c.ok).collect();
        let skipped: Vec<&CheckResult> = self.checks.iter().filter(|c| c.skipped).collect();
        let blocking = self.has_failure();

        if !failed.is_empty() {
            for check in &failed {
                if check.blocking {
                    fail(&format!("{}：{}", check.name, check.detail));
                } else {
                    warn(&format!("{}：{}", check.name, check.detail));
                }
            }
            progress_line("");
            if blocking {
                info("下一步：运行 `easylife-ai ops-check <你的问题>` 获取排查指引");
            }
        } else if skipped.is_empty() {
            ok("全部检查通过，两条上报链路正常");
        } else {
            // Passing checks must not be reported as "both pipelines work" when
            // one of them was never exercised.
            for check in &skipped {
                skipped_line(&format!("{}：{}", check.name, check.detail));
            }
            progress_line("");
            ok("已执行的检查全部通过");
            warn(&format!(
                "{} 项未验证，不能据此认为两条链路都正常",
                skipped.len()
            ));
        }

        if let Some(dir) = &temp_dir {
            if self.opts.keep {
                info(&format!("临时目录已保留：{}", dir.display()));
            } else {
                let _ = fs::remove_dir_all(dir);
            }
        }

        // Save before the JSON branch so `--json` runs also leave a transcript
        // behind. The JSON itself goes to stdout and is not part of the report.
        if json_output {
            if !self.opts.no_save {
                match save_report() {
                    Ok(path) => eprintln!("诊断结果已保存：{}", path.display()),
                    Err(e) => eprintln!("诊断结果保存失败（不影响本次结论）：{}", e),
                }
            }
        } else if self.opts.no_save {
            info("已跳过保存诊断结果（--no-save）");
        } else {
            match save_report() {
                Ok(path) => info(&format!(
                    "诊断结果已保存：{}（`easylife-ai ops-check` 会自动引用）",
                    path.display()
                )),
                Err(e) => warn(&format!("诊断结果保存失败（不影响本次结论）：{}", e)),
            }
        }

        if json_output {
            let payload = json!({
                "ok": !blocking,
                "checks": self.checks.iter().map(|c| json!({
                    "name": c.name,
                    "ok": c.ok,
                    "blocking": c.blocking,
                    "skipped": c.skipped,
                    "detail": c.detail,
                })).collect::<Vec<_>>(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }

        if blocking {
            std::process::exit(1);
        }
    }
}

/// Reject a `--repo-url` that embeds credentials.
///
/// The tracker reports `repo_url` verbatim to the server, so a token in the
/// remote URL would be uploaded and stored in plain text.
fn reject_credentials(url: &str) -> Result<(), String> {
    // Look only at the authority component, so an `@` in a path is fine.
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let ssh = url.starts_with("ssh://")
        || (!url.contains("://") && authority.contains('@') && authority.contains(':'));
    let userinfo = authority.rsplit_once('@').map(|(user, _)| user);
    if userinfo.is_some_and(|user| !ssh || user.contains(':')) {
        return Err(
            "--repo-url 不能包含用户名或 token（仓库地址会被原样上报到服务端）".to_string(),
        );
    }
    Ok(())
}

/// Only a branch created for this run is eligible for cleanup. Never delete a
/// repository's default branch, even when a check fails halfway through.
struct RemoteBranchCleanup {
    work: PathBuf,
    remote: String,
    branch: String,
    branch_created: bool,
    finished: bool,
}

impl RemoteBranchCleanup {
    fn new(work: PathBuf, remote: String, branch: String) -> Self {
        Self {
            work,
            remote,
            branch,
            branch_created: false,
            finished: false,
        }
    }

    fn mark_branch_created(&mut self) {
        self.branch_created = true;
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if self.branch_created {
            if !self.branch.starts_with("doctor/") {
                return Err("拒绝清理非 doctor 分支".to_string());
            }
            git_in(
                &self.work,
                &["push", "--quiet", &self.remote, "--delete", &self.branch],
            )?;
        }
        git_in(&self.work, &["remote", "remove", &self.remote])
    }
}

impl Drop for RemoteBranchCleanup {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.cleanup()
        {
            warn(&format!(
                "远端分支 {} 清理失败，请手动检查：{}",
                self.branch, error
            ));
        }
    }
}

fn run_install_hooks_captured() -> Result<(), String> {
    let exe = get_current_binary_path().map_err(|e| format!("无法定位二进制：{}", e))?;
    let output = Command::new(exe)
        .arg("install-hooks")
        .output()
        .map_err(|e| format!("启动 install-hooks 失败：{}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "install-hooks 退出码 {}：{} {}",
            output.status.code().unwrap_or(-1),
            stdout.trim(),
            stderr.trim()
        ))
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {} 失败：{}", args.first().unwrap_or(&""), e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git_capture(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {} 失败：{}", args.join(" "), e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!(
            "git {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn resolve_git_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("git");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Whether `path` is an easylife-ai shim (a symlink back to our binary).
fn is_our_shim(path: &Path) -> bool {
    let Ok(target) = fs::canonicalize(path) else {
        return false;
    };
    match get_current_binary_path() {
        Ok(ours) => target == ours,
        Err(_) => {
            // Fall back to a name check when the current exe is unresolvable
            // (e.g. running from a test harness).
            target
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("easylife-ai") || n.starts_with("git-ai"))
        }
    }
}

/// TCP-connect to the tracker host. Returns `host:port` on success.
fn probe_tcp(url: &str) -> Result<String, String> {
    let (host, port) = parse_host_port(url)?;
    let addr = format!("{}:{}", host, port);
    let mut last_err = String::new();

    for resolved in std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| format!("无法解析 {}：{}", addr, e))?
    {
        match std::net::TcpStream::connect_timeout(&resolved, Duration::from_secs(5)) {
            Ok(_) => return Ok(addr),
            Err(e) => last_err = e.to_string(),
        }
    }

    Err(if last_err.is_empty() {
        format!("{} 无可用地址", addr)
    } else {
        last_err
    })
}

fn parse_host_port(url: &str) -> Result<(String, u16), String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = without_scheme
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("无法从 {} 解析主机", url))?;

    // Drop any userinfo component before splitting off the port.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);

    if let Some((host, port)) = host_port.rsplit_once(':') {
        let port: u16 = port.parse().map_err(|_| format!("端口不合法：{}", port))?;
        Ok((host.to_string(), port))
    } else {
        let port = if url.starts_with("https://") { 443 } else { 80 };
        Ok((host_port.to_string(), port))
    }
}

fn current_log_len() -> u64 {
    fs::metadata(log::log_path()).map(|m| m.len()).unwrap_or(0)
}

/// Read log lines appended since `offset`.
fn read_new_log_lines(offset: u64) -> (Vec<String>, u64) {
    let path = log::log_path();
    let Ok(mut file) = fs::File::open(&path) else {
        return (Vec::new(), offset);
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }

    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return (Vec::new(), offset);
    }

    // Only complete lines are usable; a partially flushed last line is kept
    // for the next poll by not advancing past its start.
    let complete = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let lines = buf[..complete]
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    (lines, offset + complete as u64)
}

fn parse_log_fields(line: &str) -> std::collections::HashMap<String, String> {
    line.split('\t')
        .skip(5)
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn wait_for_code_log_entry(offset: &u64, commit_sha: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut cursor = *offset;
    let short_sha = truncate_sha(commit_sha, 7);

    loop {
        let (lines, next) = read_new_log_lines(cursor);
        cursor = next;
        if let Some(line) = lines.iter().find(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            let fields = parse_log_fields(line);
            parts.get(2).is_some_and(|sha| *sha == short_sha)
                && fields.get("kind").map(String::as_str) == Some("code")
        }) {
            return Some(line.clone());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(REPORT_POLL);
    }
}

fn truncate_sha(sha: &str, len: usize) -> &str {
    &sha[..sha.len().min(len)]
}

/// Collect up to `want` token-report lines for `session_id`.
fn wait_for_log_entries(
    offset: &u64,
    kind: &str,
    session_id: &str,
    want: usize,
    timeout: Duration,
) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    let mut cursor = *offset;
    let mut found: Vec<String> = Vec::new();

    loop {
        let (lines, next) = read_new_log_lines(cursor);
        cursor = next;
        for line in lines {
            let fields = parse_log_fields(&line);
            if fields.get("kind").map(String::as_str) != Some(kind) {
                continue;
            }
            if fields.get("session_id").map(String::as_str) == Some(session_id) {
                found.push(line);
            }
        }
        if found.len() >= want || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(REPORT_POLL);
    }
}

fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    // Mix in address-space entropy so concurrent runs do not collide.
    let stack_marker = &nanos as *const u128 as usize as u128;
    let mixed = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(pid << 17)
        .wrapping_add(stack_marker);
    format!("{:08x}", (mixed >> 16) as u32)
}

/// Generate 50-100 characters of plausible Python for the upload payload.
fn random_python(id: &str) -> String {
    let seed = id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let pick = |n: usize, salt: u32| ((seed.wrapping_add(salt)) as usize) % n;

    let names = ["total", "count", "items", "score", "value", "limit"];
    let ops = ["+", "-", "*"];
    let verbs = ["计算", "汇总", "校验", "统计"];

    let name = names[pick(names.len(), 1)];
    let other = names[pick(names.len(), 7)];
    let op = ops[pick(ops.len(), 13)];

    let mut body = String::new();
    body.push_str(&format!("# doctor 自检文件 id={}\n", id));
    body.push_str(&format!(
        "def {}({}):\n",
        ["run", "check", "probe", "verify"][pick(4, 3)],
        other
    ));
    body.push_str(&format!(
        "    \"\"\"{}上报链路。\"\"\"\n",
        verbs[pick(verbs.len(), 5)]
    ));
    body.push_str(&format!(
        "    {} = {} {} {}\n",
        name,
        other,
        op,
        pick(90, 11) + 1
    ));
    body.push_str(&format!("    return {} * {}\n", name, pick(9, 17) + 2));

    // Pad to at least 50 characters with a comment block.
    let mut pad = 1;
    while body.chars().count() < 50 {
        body.push_str(&format!("# padding line {}\n", pad));
        pad += 1;
    }
    // Trim to at most 100 characters, keeping whole lines.
    while body.chars().count() > 100 {
        let trimmed = body.trim_end();
        match trimmed.rfind('\n') {
            Some(i) if i > 0 => {
                body.truncate(i + 1);
            }
            _ => break,
        }
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sgr_but_keeps_text() {
        assert_eq!(strip_ansi("\x1b[1m[1/7] 环境自检\x1b[0m"), "[1/7] 环境自检");
        assert_eq!(
            strip_ansi("      \x1b[90m⊘\x1b[0m 已禁用"),
            "      ⊘ 已禁用"
        );
        // A line with no escapes is unchanged.
        assert_eq!(strip_ansi("plain"), "plain");
        // A trailing unterminated escape must not swallow the text.
        assert_eq!(strip_ansi("text\x1b["), "text");
    }

    #[test]
    fn report_path_lives_under_the_app_dir() {
        let path = report_path();
        assert!(path.ends_with(REPORT_FILE_NAME));
        assert!(path.to_string_lossy().contains(APP_DIR_NAME));
    }

    #[test]
    fn token_report_disabled_skips_without_creating_transcript() {
        let root = tempfile::tempdir().unwrap();
        let mut doctor = Doctor::new(Options::default());
        doctor.temp_dir = Some(root.path().to_path_buf());
        doctor.step_token_report(false);
        assert!(!root.path().join("transcripts").exists());
        assert_eq!(doctor.checks.len(), 1);
        let check = &doctor.checks[0];
        assert_eq!(check.name, "token_report");
        assert!(check.ok && !check.blocking);
        assert!(check.skipped, "a skipped check must be marked skipped");
        assert!(check.detail.contains("未验证"));
        assert!(!check.detail.contains("成功"));
    }

    /// A skipped pipeline must never be summarised as "both pipelines work":
    /// the summary branches on `skipped`, not just on `ok`.
    #[test]
    fn skipped_check_is_not_summarised_as_all_pipelines_healthy() {
        let mut doctor = Doctor::new(Options::default());
        doctor.record("code_report", true, false, "上报成功");
        doctor.record_skipped("token_report", "已禁用，未验证");

        assert!(!doctor.has_failure(), "skipping is not a failure");
        assert!(
            doctor.checks.iter().any(|c| c.skipped),
            "summary must see the skip"
        );
        // Both checks pass, but one was never exercised.
        assert!(doctor.checks.iter().all(|c| c.ok));
    }

    #[test]
    fn parse_host_port_handles_common_forms() {
        assert_eq!(
            parse_host_port("http://192.168.110.18:39527").unwrap(),
            ("192.168.110.18".to_string(), 39527)
        );
        assert_eq!(
            parse_host_port("https://tracker.example.com").unwrap(),
            ("tracker.example.com".to_string(), 443)
        );
        assert_eq!(
            parse_host_port("http://tracker.example.com").unwrap(),
            ("tracker.example.com".to_string(), 80)
        );
        // An `@` in a path must not be mistaken for userinfo.
        assert_eq!(
            parse_host_port("http://host:8080/a@b").unwrap(),
            ("host".to_string(), 8080)
        );
    }

    #[test]
    fn reject_credentials_flags_userinfo() {
        assert!(reject_credentials("https://oauth2:secret@example.com/x.git").is_err());
        assert!(reject_credentials("https://user@example.com/x.git").is_err());
        assert!(reject_credentials("https://example.com/x.git").is_ok());
        assert!(reject_credentials("git@github.com:org/repo.git").is_ok());
        assert!(reject_credentials("ssh://git@github.com/org/repo.git").is_ok());
        assert!(reject_credentials("ssh://git:secret@github.com/org/repo.git").is_err());
    }

    #[test]
    fn random_python_respects_length_bounds() {
        for id in ["00000000", "deadbeef", "ffffffff", "12345678"] {
            let body = random_python(id);
            let len = body.chars().count();
            assert!(
                (50..=100).contains(&len),
                "id={} generated {} chars:\n{}",
                id,
                len,
                body
            );
        }
    }

    #[test]
    fn parse_log_fields_reads_key_values() {
        let line = "2026-10-16T08:30:35+00:00\tuploaded\t3f607d1\torigin/main\t/tmp/x\tkind=code\thttp=200\tsaved=true\tinserted=1\trecord_id=612";
        let fields = parse_log_fields(line);
        assert_eq!(fields.get("kind").map(String::as_str), Some("code"));
        assert_eq!(fields.get("saved").map(String::as_str), Some("true"));
        assert_eq!(fields.get("record_id").map(String::as_str), Some("612"));
        // Legacy reason text has no `=` and must be ignored here.
        let legacy = "t\tuploaded\tabc1234\torigin/main\t/tmp/x\talready_reported";
        assert!(parse_log_fields(legacy).is_empty());
    }

    #[test]
    fn options_parse_supports_both_repo_url_forms() {
        let opts =
            Options::parse(&["--repo-url".to_string(), "https://x/y.git".to_string()]).unwrap();
        assert_eq!(opts.repo_url.as_deref(), Some("https://x/y.git"));

        let opts = Options::parse(&["--repo-url=https://x/y.git".to_string()]).unwrap();
        assert_eq!(opts.repo_url.as_deref(), Some("https://x/y.git"));

        let opts = Options::parse(&["--no-fix".to_string(), "--keep".to_string()]).unwrap();
        assert!(opts.no_fix && opts.keep && !opts.json);

        assert!(Options::parse(&["--bogus".to_string()]).is_err());
        assert!(Options::parse(&["--repo-url".to_string()]).is_err());
    }
}
