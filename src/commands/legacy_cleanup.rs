//! Explicit handling of a legacy `~/.git-ai` runtime directory.
//!
//! `install-hooks` already stops the orphaned legacy daemon and repoints the
//! hooks it left behind, but it deliberately never deletes the old directory:
//! that directory holds `distinct_id` and the historical databases, and losing
//! them is not reversible. Removal is therefore a separate, deliberate action.
//!
//! Default behaviour is a read-only report. `--yes` renames the directory to
//! `~/.git-ai.legacy-<timestamp>` so the user can keep observing before
//! deleting it themselves; `--purge` then deletes a previously renamed copy.

use crate::commands::legacy_migration;
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
easylife-ai legacy-cleanup — 处理旧版 ~/.git-ai 目录

用法：
  easylife-ai legacy-cleanup              仅检查并报告（不修改任何内容）
  easylife-ai legacy-cleanup --yes        停止旧 daemon 并重命名为 ~/.git-ai.legacy-<时间>
  easylife-ai legacy-cleanup --purge      删除已重命名的旧目录（不可恢复）
  easylife-ai legacy-cleanup --help       显示帮助

说明：
  * 旧目录包含 distinct_id 与历史数据库，默认只报告、不删除。
  * 建议先 --yes 观察一段时间，确认无影响后再 --purge。
  * 新目录 ~/.easylife-ai 不受影响。";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    yes: bool,
    purge: bool,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    for arg in args {
        match arg.as_str() {
            "--yes" => opts.yes = true,
            "--purge" => opts.purge = true,
            "--help" | "-h" => opts.help = true,
            other => return Err(format!("unknown argument: {}", other)),
        }
    }
    if opts.yes && opts.purge {
        return Err("--yes 与 --purge 不能同时使用".to_string());
    }
    Ok(opts)
}

pub fn handle_legacy_cleanup(args: &[String]) {
    let opts = match parse_args(args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("easylife-ai legacy-cleanup: {}", e);
            eprintln!();
            eprintln!("{}", USAGE);
            std::process::exit(1);
        }
    };

    if opts.help {
        println!("{}", USAGE);
        return;
    }

    if let Err(e) = run(&opts) {
        eprintln!("easylife-ai legacy-cleanup: {}", e);
        std::process::exit(1);
    }
}

fn run(opts: &Options) -> Result<(), String> {
    let home = crate::mdm::utils::home_dir();
    let legacy = legacy_migration::legacy_dir(&home);

    if opts.purge {
        return purge(&home);
    }

    if !legacy.exists() {
        println!(
            "\x1b[1;32m✓\x1b[0m 未发现旧版目录 {}，无需处理",
            legacy.display()
        );
        return Ok(());
    }

    let size = dir_size(&legacy);
    println!("\x1b[1m旧版目录\x1b[0m        {}", legacy.display());
    println!("\x1b[1m占用空间\x1b[0m        {}", human_size(size));
    println!(
        "\x1b[1m包含内容\x1b[0m        {}",
        top_entries(&legacy).join("、")
    );

    if !opts.yes {
        println!();
        println!("当前为只读检查，未修改任何内容。");
        println!("如需停止旧后台服务并重命名旧目录：easylife-ai legacy-cleanup --yes");
        return Ok(());
    }

    // Stop the legacy daemon first: renaming the directory out from under a
    // running daemon would leave it writing to a path that no longer exists.
    let (stopped, notes) = legacy_migration::stop_legacy_daemon(&home);
    for note in &notes {
        println!("\x1b[1;33m⚠ {}\x1b[0m", note);
    }
    if let Some(pid) = stopped {
        println!("\x1b[1;32m✓\x1b[0m 已停止旧后台服务（PID {}）", pid);
    }

    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let target = home.join(format!(
        "{}.legacy-{}",
        legacy_migration::LEGACY_DIR_NAME,
        stamp
    ));
    fs::rename(&legacy, &target).map_err(|e| format!("重命名失败：{}", e))?;

    println!("\x1b[1;32m✓\x1b[0m 旧目录已重命名为 {}", target.display());
    println!("观察一段时间确认无影响后，可执行：easylife-ai legacy-cleanup --purge");
    Ok(())
}

fn purge(home: &Path) -> Result<(), String> {
    let mut found: Vec<PathBuf> = Vec::new();
    let entries = fs::read_dir(home).map_err(|e| format!("读取 home 目录失败：{}", e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&format!("{}.legacy-", legacy_migration::LEGACY_DIR_NAME)) {
            found.push(entry.path());
        }
    }

    if found.is_empty() {
        println!("未发现已重命名的旧目录，无需删除。");
        return Ok(());
    }

    for path in &found {
        let size = dir_size(path);
        fs::remove_dir_all(path).map_err(|e| format!("删除 {} 失败：{}", path.display(), e))?;
        println!(
            "\x1b[1;32m✓\x1b[0m 已删除 {}（释放 {}）",
            path.display(),
            human_size(size)
        );
    }
    Ok(())
}

fn top_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_rejects_conflicting_and_unknown_flags() {
        assert!(parse_args(&["--yes".into(), "--purge".into()]).is_err());
        assert!(parse_args(&["--nope".into()]).is_err());
        assert_eq!(
            parse_args(&["--yes".into()]).unwrap(),
            Options {
                yes: true,
                purge: false,
                help: false
            }
        );
        assert!(parse_args(&[]).unwrap() == Options::default());
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn report_mode_leaves_legacy_dir_untouched() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".git-ai");
        fs::create_dir_all(legacy.join("bin")).unwrap();
        fs::write(legacy.join("bin/git-ai"), b"x").unwrap();

        assert!(legacy.exists());
        assert!(dir_size(&legacy) > 0);
        assert_eq!(top_entries(&legacy), vec!["bin".to_string()]);
    }
}
