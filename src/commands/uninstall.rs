use crate::commands::install_hooks;
use crate::config;
use crate::error::GitAiError;
use crate::mdm::utils::get_current_binary_path;
use std::fs;
use std::path::{Path, PathBuf};

fn remove_path(path: &Path, dry_run: bool) -> Result<bool, GitAiError> {
    if path.symlink_metadata().is_err() {
        return Ok(false);
    }
    if dry_run {
        println!("Would remove {}", path.display());
        return Ok(true);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    println!("Removed {}", path.display());
    Ok(true)
}

/// Like `remove_path`, but on Windows a path that is still locked (typically the
/// running executable itself) is queued for deletion after this process exits
/// instead of aborting the whole uninstall.
fn remove_or_defer(
    path: &Path,
    dry_run: bool,
    deferred: &mut Vec<PathBuf>,
) -> Result<bool, GitAiError> {
    match remove_path(path, dry_run) {
        Err(GitAiError::IoError(error)) if cfg!(windows) => {
            println!(
                "Could not remove {} now ({}); it will be removed after this process exits",
                path.display(),
                error
            );
            deferred.push(path.to_path_buf());
            Ok(true)
        }
        other => other,
    }
}

#[cfg(windows)]
fn schedule_delete_after_exit(paths: &[PathBuf]) {
    use std::os::windows::process::CommandExt;

    if paths.is_empty() {
        return;
    }
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let list = paths
        .iter()
        .map(|path| format!("'{}'", path.display().to_string().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         foreach ($p in @({list})) {{ \
           for ($i = 0; $i -lt 40; $i++) {{ \
             Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue; \
             if (-not (Test-Path -LiteralPath $p)) {{ break }}; \
             Start-Sleep -Milliseconds 500 \
           }} \
         }}",
        pid = std::process::id(),
        list = list
    );
    let spawned = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    match spawned {
        Ok(child) => println!("Scheduled deferred removal (helper pid {})", child.id()),
        Err(error) => eprintln!(
            "Warning: could not schedule deferred removal ({}); delete these manually: {}",
            error, list
        ),
    }
}

#[cfg(not(windows))]
fn schedule_delete_after_exit(_paths: &[PathBuf]) {}

fn stop_daemon() {
    if let Ok(daemon_config) = crate::daemon::DaemonConfig::from_env_or_default_paths() {
        let _ =
            crate::commands::daemon::stop_daemon(&daemon_config, std::time::Duration::from_secs(5));
    }
}

fn managed_binary_paths() -> Result<Vec<PathBuf>, GitAiError> {
    let app = config::git_ai_dir_path().ok_or_else(|| {
        GitAiError::Generic("Could not determine application directory".to_string())
    })?;
    let bin = app.join("bin");
    let mut paths = vec![
        bin.join("easylife-ai"),
        bin.join("git"),
        bin.join("git-og"),
        bin.join("easylife-ai.exe"),
        bin.join("git.exe"),
        bin.join("git-og.cmd"),
    ];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local").join("bin").join("easylife-ai"));
    }
    Ok(paths)
}

fn cleanup_shell_profiles(dry_run: bool) -> Result<(), GitAiError> {
    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    for profile in [
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".zshrc",
        ".zshenv",
        ".config/fish/config.fish",
    ] {
        let profile = home.join(profile);
        cleanup_profile_lines(&profile, dry_run)?;
    }
    Ok(())
}

fn cleanup_profile_lines(profile: &Path, dry_run: bool) -> Result<(), GitAiError> {
    let content = match fs::read_to_string(profile) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(GitAiError::IoError(error)),
    };
    let install_dir = config::git_ai_dir_path()
        .ok_or_else(|| {
            GitAiError::Generic("Could not determine application directory".to_string())
        })?
        .join("bin");
    let marker = install_dir.to_string_lossy();
    let mut lines: Vec<&str> = content.lines().collect();
    let original_len = lines.len();
    lines.retain(|line| !line.contains(marker.as_ref()));
    if lines.len() == original_len {
        return Ok(());
    }
    if dry_run {
        println!(
            "Would remove easylife-ai PATH lines from {}",
            profile.display()
        );
        return Ok(());
    }
    fs::write(profile, lines.join("\n") + "\n")?;
    println!("Removed easylife-ai PATH lines from {}", profile.display());
    Ok(())
}

#[cfg(windows)]
fn cleanup_windows_path(dry_run: bool) -> Result<(), GitAiError> {
    let Some(app) = config::git_ai_dir_path() else {
        return Ok(());
    };
    let install_dir = app.join("bin");
    let marker = install_dir.to_string_lossy().to_lowercase();

    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let env_key = match key.open_subkey_with_flags(
        "Environment",
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    ) {
        Ok(key) => key,
        Err(_) => return Ok(()),
    };

    let raw: String = env_key.get_value("Path").unwrap_or_default();
    let entries: Vec<PathBuf> = std::env::split_paths(&raw).collect();
    let retained: Vec<&PathBuf> = entries
        .iter()
        .filter(|entry| {
            !entry
                .to_string_lossy()
                .to_lowercase()
                .contains(marker.as_str())
        })
        .collect();

    if retained.len() == entries.len() {
        return Ok(());
    }

    if dry_run {
        println!("Would remove easylife-ai from the Windows user PATH");
        return Ok(());
    }

    let joined = std::env::join_paths(retained)
        .map_err(|error| GitAiError::Generic(format!("Failed to rebuild user PATH: {}", error)))?;
    env_key
        .set_value("Path", &joined.to_string_lossy().to_string())
        .map_err(|error| GitAiError::Generic(format!("Failed to update user PATH: {}", error)))?;
    println!("Removed easylife-ai from the Windows user PATH");
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), GitAiError> {
    let dry_run = args
        .iter()
        .any(|arg| arg == "--dry-run" || arg == "--dry-run=true");
    let purge = args.iter().any(|arg| arg == "--purge");
    let keep_config = args.iter().any(|arg| arg == "--keep-config");

    if args.iter().any(|arg| {
        !matches!(
            arg.as_str(),
            "--dry-run" | "--dry-run=true" | "--purge" | "--keep-config"
        )
    }) {
        return Err(GitAiError::Generic(
            "Usage: easylife-ai uninstall [--dry-run] [--keep-config] [--purge]".to_string(),
        ));
    }

    if dry_run {
        println!("Dry-run: no changes will be made.");
    }

    stop_daemon();
    if !dry_run {
        let binary = get_current_binary_path().ok();
        let current_name = binary
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str());
        if current_name == Some("easylife-ai") || current_name == Some("easylife-ai.exe") {
            println!("The currently running executable will be removed after this process exits.");
        }
    }

    let hook_args = vec![
        if dry_run {
            "--dry-run"
        } else {
            "--dry-run=false"
        }
        .to_string(),
    ];
    if let Err(error) = install_hooks::run_uninstall(&hook_args) {
        eprintln!("Warning: failed to remove some hooks: {}", error);
    }

    let mut deferred: Vec<PathBuf> = Vec::new();
    for path in managed_binary_paths()? {
        let _ = remove_or_defer(&path, dry_run, &mut deferred)?;
    }

    cleanup_shell_profiles(dry_run)?;
    #[cfg(windows)]
    if let Err(error) = cleanup_windows_path(dry_run) {
        eprintln!(
            "Warning: failed to clean up the Windows user PATH: {}",
            error
        );
    }

    let app = config::git_ai_dir_path().ok_or_else(|| {
        GitAiError::Generic("Could not determine application directory".to_string())
    })?;
    if purge && !keep_config {
        remove_or_defer(&app, dry_run, &mut deferred)?;
    } else {
        for path in [app.join("internal"), app.join("tmp"), app.join("skills")] {
            let _ = remove_or_defer(&path, dry_run, &mut deferred)?;
        }
    }

    if !dry_run {
        schedule_delete_after_exit(&deferred);
        if purge && !keep_config {
            println!(
                "easylife-ai uninstall completed. Configuration, data, and backups were removed."
            );
        } else {
            println!(
                "easylife-ai uninstall completed. Configuration, data, and backups were retained."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct HomeGuard {
        previous_home: Option<std::ffi::OsString>,
        previous_userprofile: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let previous_home = std::env::var_os("HOME");
            let previous_userprofile = std::env::var_os("USERPROFILE");
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
            }
            Self {
                previous_home,
                previous_userprofile,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.previous_userprofile {
                    Some(value) => std::env::set_var("USERPROFILE", value),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
        }
    }

    #[test]
    #[serial]
    fn cleanup_profile_lines_only_removes_easylife_lines() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let _guard = HomeGuard::set(home);

        let profile = home.join(".zshrc");
        let app_bin = home.join(config::APP_DIR_NAME).join("bin");
        fs::write(
            &profile,
            format!(
                "export PATH=\"/usr/local/bin:$PATH\"\nexport PATH=\"{}:$PATH\"\nexport EDITOR=vim\n",
                app_bin.display()
            ),
        )
        .unwrap();

        cleanup_profile_lines(&profile, false).unwrap();

        let content = fs::read_to_string(&profile).unwrap();
        assert!(!content.contains(".easylife-ai"));
        assert!(content.contains("/usr/local/bin"));
        assert!(content.contains("EDITOR=vim"));
    }

    #[test]
    #[serial]
    fn cleanup_profile_lines_is_noop_for_missing_profile() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let _guard = HomeGuard::set(home);

        let profile = home.join(".zshrc");
        cleanup_profile_lines(&profile, false).unwrap();
        assert!(!profile.exists());
    }
}
