use crate::error::GitAiError;
use crate::mdm::utils::home_dir;
use std::fs;
use std::path::Path;

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
    let home = home_dir();
    #[cfg(windows)]
    {
        home.join(".easylife-ai")
            .join("bin")
            .join("git.exe")
            .to_string_lossy()
            .replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        home.join(".easylife-ai")
            .join("bin")
            .join("git")
            .to_string_lossy()
            .to_string()
    }
}

/// Parse the current `myPathToGit` value out of a git.xml string.
fn parse_git_path(xml: &str) -> Option<String> {
    // Minimal hand-rolled parse: find the option element with name="myPathToGit"
    // and extract its value="..." attribute. This avoids pulling in an XML crate
    // while still being robust enough for the single-element file JetBrains writes.
    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.contains("myPathToGit") {
            if let Some(after) = trimmed.find("value=\"") {
                let start = after + 7;
                if let Some(end) = trimmed[start..].find('"') {
                    return Some(trimmed[start..start + end].to_string());
                }
            }
        }
    }
    None
}

/// Build the full git.xml content with our git shim path set.
fn build_git_xml(git_path: &str) -> String {
    format!(
        "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"{}\" />\n  </component>\n</application>\n",
        git_path
    )
}

/// Update (or create) `options/git.xml` so that IDEA uses our git shim.
///
/// Returns `Ok(None)` when the file is already correct, `Ok(Some(diff))` when it
/// was changed (or would be changed in a dry run).
pub fn update_jetbrains_git_xml(
    options_dir: &Path,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let target_value = jetbrains_git_path_value();
    let absolute_value = absolute_git_shim_path();
    let git_xml = options_dir.join("git.xml");

    // Read existing content if present.
    let original = if git_xml.exists() {
        fs::read_to_string(&git_xml)?
    } else {
        String::new()
    };

    // Check whether the current value already points at our shim.
    if let Some(current) = parse_git_path(&original) {
        if current == target_value || current == absolute_value {
            return Ok(None);
        }
    }

    let new_content = if original.trim().is_empty() {
        // File does not exist or is empty — write a clean new file.
        build_git_xml(&target_value)
    } else {
        // File exists. Try to update in place; fall back to rewriting.
        patch_git_xml(&original, &target_value)
    };

    if new_content == original {
        return Ok(None);
    }

    let diff = generate_diff(&git_xml, &original, &new_content);

    if !dry_run {
        if let Some(parent) = git_xml.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&git_xml, &new_content)?;
    }

    Ok(Some(diff))
}

/// Restore (or clear) `options/git.xml` during uninstall.
///
/// If our value is the only option in the file we remove the file entirely.
/// If there are other options we remove just the `myPathToGit` option element.
/// Returns `Ok(None)` when the file was not managed by us.
pub fn restore_jetbrains_git_xml(
    options_dir: &Path,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let git_xml = options_dir.join("git.xml");
    if !git_xml.exists() {
        return Ok(None);
    }

    let original = fs::read_to_string(&git_xml)?;
    let target_value = jetbrains_git_path_value();
    let absolute_value = absolute_git_shim_path();

    let current = match parse_git_path(&original) {
        Some(v) => v,
        None => return Ok(None),
    };

    if current != target_value && current != absolute_value {
        // File does not belong to us.
        return Ok(None);
    }

    // Count how many <option ...> elements remain besides myPathToGit.
    let other_options = original
        .lines()
        .filter(|l| l.trim().starts_with("<option") && !l.contains("myPathToGit"))
        .count();

    let diff;
    if other_options == 0 {
        // Only our option — remove the file.
        diff = format!("(remove) {}", git_xml.display());
        if !dry_run {
            fs::remove_file(&git_xml)?;
        }
    } else {
        // Other settings present — remove only the myPathToGit line.
        let new_content = remove_git_path_line(&original);
        diff = generate_diff(&git_xml, &original, &new_content);
        if !dry_run {
            fs::write(&git_xml, &new_content)?;
        }
    }

    Ok(Some(diff))
}

// ── XML helpers ──────────────────────────────────────────────────────────────

/// Update an existing git.xml string in place, replacing just the value of
/// `myPathToGit`.  If the component or option element doesn't exist in the
/// expected shape we fall back to a fresh file.
fn patch_git_xml(original: &str, new_value: &str) -> String {
    if original.contains("myPathToGit") {
        // Replace the value attribute of the existing option element.
        let mut out = String::with_capacity(original.len());
        for line in original.lines() {
            if line.trim().contains("myPathToGit") {
                // Re-emit the line with the new value, preserving leading whitespace.
                let leading: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                out.push_str(&format!(
                    "{}<option name=\"myPathToGit\" value=\"{}\" />\n",
                    leading, new_value
                ));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        // Preserve no-trailing-newline originals by trimming if the original had none.
        if !original.ends_with('\n') {
            out.pop();
        }
        out
    } else if original.contains("Git.Application.Settings") {
        // Component exists but option is missing — inject before </component>.
        original.replacen(
            "</component>",
            &format!(
                "    <option name=\"myPathToGit\" value=\"{}\" />\n  </component>",
                new_value
            ),
            1,
        )
    } else {
        // Unrecognised shape — rewrite.
        build_git_xml(new_value)
    }
}

/// Remove the `myPathToGit` option line from an existing xml string.
fn remove_git_path_line(original: &str) -> String {
    original
        .lines()
        .filter(|l| !l.trim().contains("myPathToGit"))
        .map(|l| format!("{}\n", l))
        .collect()
}

fn generate_diff(path: &Path, before: &str, after: &str) -> String {
    crate::mdm::utils::generate_diff(path, before, after)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn options_dir_with_content(content: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let options = dir.path().join("options");
        fs::create_dir_all(&options).unwrap();
        if !content.is_empty() {
            fs::write(options.join("git.xml"), content).unwrap();
        }
        (dir, options)
    }

    #[test]
    fn test_creates_git_xml_when_missing() {
        let (_dir, options) = options_dir_with_content("");
        let result = update_jetbrains_git_xml(&options, false).unwrap();
        assert!(result.is_some(), "should report a change");
        let written = fs::read_to_string(options.join("git.xml")).unwrap();
        assert!(
            written.contains("myPathToGit"),
            "git.xml should contain myPathToGit"
        );
        assert!(
            written.contains(".easylife-ai"),
            "git.xml should reference .easylife-ai"
        );
    }

    #[test]
    fn test_dry_run_does_not_write() {
        let (_dir, options) = options_dir_with_content("");
        update_jetbrains_git_xml(&options, true).unwrap();
        assert!(!options.join("git.xml").exists(), "dry run must not write");
    }

    #[test]
    fn test_no_change_when_already_set() {
        let target = jetbrains_git_path_value();
        let content = build_git_xml(&target);
        let (_dir, options) = options_dir_with_content(&content);
        let result = update_jetbrains_git_xml(&options, false).unwrap();
        assert!(result.is_none(), "no change expected when already correct");
    }

    #[test]
    fn test_updates_existing_other_value() {
        let content = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"/usr/bin/git\" />\n  </component>\n</application>\n";
        let (_dir, options) = options_dir_with_content(content);
        let result = update_jetbrains_git_xml(&options, false).unwrap();
        assert!(result.is_some(), "should report a change");
        let written = fs::read_to_string(options.join("git.xml")).unwrap();
        assert!(!written.contains("/usr/bin/git"));
        assert!(written.contains(".easylife-ai"));
    }

    #[test]
    fn test_restore_removes_file_when_only_our_option() {
        let target = jetbrains_git_path_value();
        let content = build_git_xml(&target);
        let (_dir, options) = options_dir_with_content(&content);
        let result = restore_jetbrains_git_xml(&options, false).unwrap();
        assert!(result.is_some(), "should report a change");
        assert!(!options.join("git.xml").exists(), "file should be removed");
    }

    #[test]
    fn test_restore_dry_run_does_not_delete() {
        let target = jetbrains_git_path_value();
        let content = build_git_xml(&target);
        let (_dir, options) = options_dir_with_content(&content);
        restore_jetbrains_git_xml(&options, true).unwrap();
        assert!(options.join("git.xml").exists(), "dry run must not delete");
    }

    #[test]
    fn test_restore_leaves_file_alone_when_not_ours() {
        let content = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"/usr/bin/git\" />\n  </component>\n</application>\n";
        let (_dir, options) = options_dir_with_content(content);
        let result = restore_jetbrains_git_xml(&options, false).unwrap();
        assert!(result.is_none(), "should not touch a file we did not write");
        assert!(options.join("git.xml").exists());
    }

    #[test]
    fn test_restore_removes_only_our_line_when_other_options_present() {
        let target = jetbrains_git_path_value();
        let content = format!(
            "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"{}\" />\n    <option name=\"someOther\" value=\"true\" />\n  </component>\n</application>\n",
            target
        );
        let (_dir, options) = options_dir_with_content(&content);
        restore_jetbrains_git_xml(&options, false).unwrap();
        assert!(
            options.join("git.xml").exists(),
            "file should stay when other options remain"
        );
        let written = fs::read_to_string(options.join("git.xml")).unwrap();
        assert!(!written.contains("myPathToGit"));
        assert!(written.contains("someOther"));
    }

    #[test]
    fn test_options_dir_derivation_macos_windows_style() {
        // plugins_dir ends with "plugins" -> options is a sibling
        let plugins = std::path::PathBuf::from("/home/user/.config/JetBrains/IU2025.2/plugins");
        let fake_ide = crate::mdm::jetbrains::ide_types::DetectedIde {
            ide: &crate::mdm::jetbrains::ide_types::JETBRAINS_IDES[0],
            install_path: plugins.clone(),
            binary_path: plugins.clone(),
            build_number: None,
            major_build: None,
            plugins_dir: plugins,
        };
        assert_eq!(
            fake_ide.options_dir(),
            std::path::PathBuf::from("/home/user/.config/JetBrains/IU2025.2/options")
        );
    }

    #[test]
    fn test_options_dir_derivation_linux_style() {
        // plugins_dir is the version dir itself (no "plugins" suffix)
        let plugins =
            std::path::PathBuf::from("/home/user/.local/share/JetBrains/IntelliJIdea2025.2");
        let fake_ide = crate::mdm::jetbrains::ide_types::DetectedIde {
            ide: &crate::mdm::jetbrains::ide_types::JETBRAINS_IDES[0],
            install_path: plugins.clone(),
            binary_path: plugins.clone(),
            build_number: None,
            major_build: None,
            plugins_dir: plugins.clone(),
        };
        assert_eq!(fake_ide.options_dir(), plugins.join("options"));
    }
}
