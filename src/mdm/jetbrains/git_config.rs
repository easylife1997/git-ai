use super::git_xml::{self, Removal};
use crate::error::GitAiError;
use crate::mdm::utils::{generate_diff, home_dir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The value written to `myPathToGit` in JetBrains' git.xml.
/// JetBrains config files use `$USER_HOME$` as a variable for the home directory
/// and always use forward slashes, even on Windows.
pub fn jetbrains_git_path_value() -> String {
    #[cfg(windows)]
    {
        "$USER_HOME$/.easylife-ai/bin/git.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        "$USER_HOME$/.easylife-ai/bin/git".to_string()
    }
}

/// The absolute path equivalent of `jetbrains_git_path_value()`, used only for
/// checking whether the current value already points to our shim.
fn absolute_git_shim_path() -> String {
    let path = home_dir().join(".easylife-ai").join("bin");
    #[cfg(windows)]
    let path = path.join("git.exe");
    #[cfg(not(windows))]
    let path = path.join("git");
    path.to_string_lossy().replace('\\', "/")
}

fn is_managed_value(value: &str) -> bool {
    value == jetbrains_git_path_value() || value == absolute_git_shim_path()
}

/// What git.xml looked like before easylife-ai took it over.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupEntry {
    /// Whether git.xml existed before we first touched it.
    file_existed: bool,
    /// The user's own `myPathToGit`, or `None` when the option was absent.
    previous_value: Option<String>,
}

type Backups = BTreeMap<String, BackupEntry>;

/// Where the pre-install values are kept (`~/.easylife-ai/internal/jetbrains-git-backup.json`).
pub fn default_backup_path() -> PathBuf {
    crate::config::internal_dir_path()
        .unwrap_or_else(|| home_dir().join(".easylife-ai").join("internal"))
        .join("jetbrains-git-backup.json")
}

fn load_backups(path: &Path) -> Result<Backups, GitAiError> {
    if !path.exists() {
        return Ok(Backups::new());
    }
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| {
        GitAiError::Generic(format!("Invalid backup file {}: {}", path.display(), error))
    })
}

fn save_backups(path: &Path, backups: &Backups) -> Result<(), GitAiError> {
    if backups.is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(backups)
        .map_err(|error| GitAiError::Generic(format!("Failed to serialize backup: {}", error)))?;
    fs::write(path, text)?;
    Ok(())
}

/// Point `options/git.xml` at our git shim.
///
/// The first time we replace a value, the user's previous setting is saved to
/// `backup_path` so `restore_jetbrains_git_xml` can put it back. Re-running while the
/// shim is already configured changes nothing and keeps the existing backup.
///
/// Returns `Ok(None)` when nothing needed to change, otherwise a diff. An unparsable
/// git.xml is reported as an error and left untouched.
pub fn update_jetbrains_git_xml(
    options_dir: &Path,
    backup_path: &Path,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let git_xml = options_dir.join("git.xml");
    let file_existed = git_xml.exists();
    let original = if file_existed {
        fs::read_to_string(&git_xml)?
    } else {
        String::new()
    };
    let blank = original.trim().is_empty();

    let current = if blank {
        None
    } else {
        git_xml::read_option_value(&original)?
    };
    if current.as_deref().is_some_and(is_managed_value) {
        return Ok(None);
    }

    let target = jetbrains_git_path_value();
    let new_content = if blank {
        git_xml::build_git_xml(&target)
    } else {
        git_xml::set_option_value(&original, &target)?
    };
    if new_content == original {
        return Ok(None);
    }
    let diff = generate_diff(&git_xml, &original, &new_content);

    if !dry_run {
        // Save the backup first so a failure here never leaves an unrecoverable change.
        let mut backups = load_backups(backup_path)?;
        let key = git_xml.to_string_lossy().to_string();
        let file_existed = backups
            .get(&key)
            .map(|entry| entry.file_existed)
            .unwrap_or(file_existed);
        backups.insert(
            key,
            BackupEntry {
                file_existed,
                previous_value: current,
            },
        );
        save_backups(backup_path, &backups)?;

        fs::create_dir_all(options_dir)?;
        fs::write(&git_xml, &new_content)?;
    }
    Ok(Some(diff))
}

/// Undo `update_jetbrains_git_xml` during uninstall.
///
/// Only acts when `myPathToGit` still points at our shim. The saved value is restored if
/// there was one; otherwise just our option (and a component it leaves empty) is removed,
/// and the file is deleted when nothing else remains. Returns `Ok(None)` when git.xml is
/// not managed by us.
pub fn restore_jetbrains_git_xml(
    options_dir: &Path,
    backup_path: &Path,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let git_xml = options_dir.join("git.xml");
    if !git_xml.exists() {
        return Ok(None);
    }
    let original = fs::read_to_string(&git_xml)?;
    match git_xml::read_option_value(&original)? {
        Some(value) if is_managed_value(&value) => {}
        _ => return Ok(None),
    }

    let mut backups = load_backups(backup_path)?;
    let key = git_xml.to_string_lossy().to_string();
    let previous = backups
        .get(&key)
        .and_then(|entry| entry.previous_value.clone());

    let diff = if let Some(previous) = previous {
        let restored = git_xml::set_option_value(&original, &previous)?;
        if !dry_run {
            fs::write(&git_xml, &restored)?;
        }
        generate_diff(&git_xml, &original, &restored)
    } else {
        match git_xml::remove_option(&original)? {
            Removal::NotPresent => return Ok(None),
            Removal::FileEmpty => {
                if !dry_run {
                    fs::remove_file(&git_xml)?;
                }
                format!("(remove) {}", git_xml.display())
            }
            Removal::Updated(updated) => {
                if !dry_run {
                    fs::write(&git_xml, &updated)?;
                }
                generate_diff(&git_xml, &original, &updated)
            }
        }
    };

    if !dry_run && backups.remove(&key).is_some() {
        save_backups(backup_path, &backups)?;
    }
    Ok(Some(diff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::jetbrains::ide_types::{DetectedIde, JETBRAINS_IDES};
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        options: PathBuf,
        backup: PathBuf,
    }

    impl Fixture {
        fn new(content: Option<&str>) -> Self {
            let dir = TempDir::new().unwrap();
            let options = dir.path().join("options");
            fs::create_dir_all(&options).unwrap();
            if let Some(content) = content {
                fs::write(options.join("git.xml"), content).unwrap();
            }
            let backup = dir.path().join("internal").join("backup.json");
            Self {
                _dir: dir,
                options,
                backup,
            }
        }

        fn install(&self) -> Option<String> {
            update_jetbrains_git_xml(&self.options, &self.backup, false).unwrap()
        }

        fn restore(&self) -> Option<String> {
            restore_jetbrains_git_xml(&self.options, &self.backup, false).unwrap()
        }

        fn xml(&self) -> String {
            fs::read_to_string(self.options.join("git.xml")).unwrap()
        }

        fn value(&self) -> Option<String> {
            git_xml::read_option_value(&self.xml()).unwrap()
        }
    }

    fn custom(value: &str) -> String {
        format!(
            "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"{value}\" />\n    <option name=\"other\" value=\"1\" />\n  </component>\n</application>\n"
        )
    }

    #[test]
    fn creates_git_xml_when_missing() {
        let fx = Fixture::new(None);
        assert!(fx.install().is_some());
        assert_eq!(fx.value(), Some(jetbrains_git_path_value()));
    }

    #[test]
    fn dry_run_writes_neither_file_nor_backup() {
        let fx = Fixture::new(Some(&custom("/usr/bin/git")));
        let before = fx.xml();
        update_jetbrains_git_xml(&fx.options, &fx.backup, true).unwrap();
        assert_eq!(fx.xml(), before);
        assert!(!fx.backup.exists());
    }

    #[test]
    fn no_change_when_already_managed() {
        let fx = Fixture::new(None);
        fx.install();
        assert!(fx.install().is_none());
    }

    #[test]
    fn install_keeps_other_options() {
        let fx = Fixture::new(Some(&custom("/usr/bin/git")));
        fx.install();
        assert_eq!(fx.value(), Some(jetbrains_git_path_value()));
        assert!(fx.xml().contains("name=\"other\""));
    }

    #[test]
    fn invalid_xml_is_an_error_and_left_untouched() {
        let fx = Fixture::new(Some("<application><component"));
        assert!(update_jetbrains_git_xml(&fx.options, &fx.backup, false).is_err());
        assert_eq!(fx.xml(), "<application><component");
        assert!(!fx.backup.exists());
    }

    #[test]
    fn uninstall_restores_users_custom_git_path() {
        let fx = Fixture::new(Some(&custom("/opt/custom/git")));
        fx.install();
        assert!(fx.restore().is_some());
        assert_eq!(fx.value().as_deref(), Some("/opt/custom/git"));
        assert!(fx.xml().contains("name=\"other\""));
        assert!(!fx.backup.exists(), "backup is dropped once restored");
    }

    #[test]
    fn repeated_install_keeps_the_original_backup() {
        let fx = Fixture::new(Some(&custom("/opt/custom/git")));
        fx.install();
        fx.install();
        fx.install();
        fx.restore();
        assert_eq!(fx.value().as_deref(), Some("/opt/custom/git"));
    }

    #[test]
    fn uninstall_removes_option_when_none_existed_before() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"other\" value=\"1\" />\n  </component>\n</application>\n";
        let fx = Fixture::new(Some(src));
        fx.install();
        fx.restore();
        assert_eq!(fx.value(), None);
        assert!(fx.xml().contains("name=\"other\""));
    }

    #[test]
    fn uninstall_deletes_file_we_created() {
        let fx = Fixture::new(None);
        fx.install();
        assert!(fx.restore().is_some());
        assert!(!fx.options.join("git.xml").exists());
    }

    #[test]
    fn uninstall_leaves_a_value_the_user_changed_afterwards() {
        let fx = Fixture::new(Some(&custom("/opt/custom/git")));
        fx.install();
        fs::write(fx.options.join("git.xml"), custom("/somewhere/else/git")).unwrap();
        assert!(fx.restore().is_none());
        assert_eq!(fx.value().as_deref(), Some("/somewhere/else/git"));
    }

    #[test]
    fn uninstall_dry_run_changes_nothing() {
        let fx = Fixture::new(Some(&custom("/opt/custom/git")));
        fx.install();
        let before = fx.xml();
        restore_jetbrains_git_xml(&fx.options, &fx.backup, true).unwrap();
        assert_eq!(fx.xml(), before);
        assert!(fx.backup.exists());
    }

    #[test]
    fn uninstall_without_backup_removes_only_our_option() {
        let fx = Fixture::new(Some(&custom(&jetbrains_git_path_value())));
        fx.restore();
        assert_eq!(fx.value(), None);
        assert!(fx.xml().contains("name=\"other\""));
    }

    fn fake_ide(plugins: &str) -> DetectedIde {
        let plugins = PathBuf::from(plugins);
        DetectedIde {
            ide: &JETBRAINS_IDES[0],
            install_path: plugins.clone(),
            binary_path: plugins.clone(),
            build_number: None,
            major_build: None,
            plugins_dir: plugins,
        }
    }

    #[test]
    fn options_dir_for_macos_windows_style_plugins_dir() {
        let ide = fake_ide("/home/user/.config/JetBrains/IU2025.2/plugins");
        assert_eq!(
            ide.options_dir(),
            PathBuf::from("/home/user/.config/JetBrains/IU2025.2/options")
        );
    }

    #[test]
    fn options_dir_for_linux_style_plugins_dir() {
        // Linux config lives under ~/.config, not next to the plugins in ~/.local/share.
        let ide = fake_ide("/home/user/.local/share/JetBrains/IntelliJIdea2025.2");
        assert_eq!(
            ide.options_dir(),
            home_dir()
                .join(".config")
                .join("JetBrains")
                .join("IntelliJIdea2025.2")
                .join("options")
        );
    }
}
