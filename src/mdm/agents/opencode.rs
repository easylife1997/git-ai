use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir, write_atomic};
use std::fs;
use std::path::{Path, PathBuf};

// OpenCode 1.x plugin (named `GitAiPlugin` factory export, no default export).
const OPENCODE_PLUGIN_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/opencode/git-ai.ts"
));

// OpenCode 2.x plugin (default `{ id, setup }` export). It cannot share a file
// with the 1.x plugin: 1.x prefers a default export and then ignores the hooks
// it returns, while 2.x rejects a default export that is not an object.
const OPENCODE_PLUGIN_V2_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/opencode/git-ai-v2.ts"
));

pub struct OpenCodeInstaller;

impl OpenCodeInstaller {
    fn plugins_dir() -> PathBuf {
        home_dir().join(".config").join("opencode").join("plugins")
    }

    /// 1.x plugin location (unchanged from earlier releases).
    fn plugin_path() -> PathBuf {
        Self::plugins_dir().join("git-ai.ts")
    }

    /// 2.x plugin location. 1.x also loads this file, but its `setup` returns
    /// immediately when the 1.x-only context is detected.
    fn plugin_v2_path() -> PathBuf {
        Self::plugins_dir().join("git-ai-v2.ts")
    }

    /// Every file we manage, paired with a function producing its content.
    fn managed_plugins(binary_path: &Path) -> Vec<(PathBuf, String)> {
        vec![
            (
                Self::plugin_path(),
                Self::generate_plugin_content(binary_path),
            ),
            (
                Self::plugin_v2_path(),
                Self::generate_plugin_v2_content(binary_path),
            ),
        ]
    }

    /// Substitute the absolute binary path into a plugin template.
    fn substitute(template: &str, binary_path: &Path) -> String {
        // Escape backslashes for the TypeScript string literal (needed for Windows paths)
        let path_str = binary_path.display().to_string().replace('\\', "\\\\");
        template.replace("__GIT_AI_BINARY_PATH__", &path_str)
    }

    /// Generate the 1.x plugin content with the absolute binary path substituted in
    fn generate_plugin_content(binary_path: &Path) -> String {
        Self::substitute(OPENCODE_PLUGIN_CONTENT, binary_path)
    }

    /// Generate the 2.x plugin content with the absolute binary path substituted in
    fn generate_plugin_v2_content(binary_path: &Path) -> String {
        Self::substitute(OPENCODE_PLUGIN_V2_CONTENT, binary_path)
    }
}

impl HookInstaller for OpenCodeInstaller {
    fn name(&self) -> &str {
        "OpenCode"
    }

    fn id(&self) -> &str {
        "opencode"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["opencode"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("opencode");
        let has_global_config = home_dir().join(".config").join("opencode").exists();
        let has_local_config = Path::new(".opencode").exists();

        if !has_binary && !has_global_config && !has_local_config {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // Both the 1.x and 2.x plugin files must be present.
        let plugins = Self::managed_plugins(&params.binary_path);
        if plugins.iter().any(|(path, _)| !path.exists()) {
            // Partially installed (e.g. upgraded from a release that only
            // shipped the 1.x file) counts as installed-but-outdated so that
            // `install-hooks` writes the missing file.
            let any_exists = plugins.iter().any(|(path, _)| path.exists());
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: any_exists,
                hooks_up_to_date: false,
            });
        }

        // Compare against the template with the binary path substituted
        let is_up_to_date = plugins.iter().all(|(path, expected)| {
            fs::read_to_string(path).unwrap_or_default().trim() == expected.trim()
        });

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: true,
            hooks_up_to_date: is_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let legacy_path = home_dir()
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            let _ = fs::remove_file(&legacy_path);
        }

        let mut diffs: Vec<String> = Vec::new();
        for (plugin_path, new_content) in Self::managed_plugins(&params.binary_path) {
            // Read existing content if present
            let existing_content = if plugin_path.exists() {
                fs::read_to_string(&plugin_path)?
            } else {
                String::new()
            };

            // Skip files that are already up to date
            if existing_content.trim() == new_content.trim() {
                continue;
            }

            diffs.push(generate_diff(&plugin_path, &existing_content, &new_content));

            if !dry_run {
                if let Some(dir) = plugin_path.parent() {
                    fs::create_dir_all(dir)?;
                }
                write_atomic(&plugin_path, new_content.as_bytes())?;
            }
        }

        if diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(diffs.join("\n")))
        }
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let legacy_path = home_dir()
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            let _ = fs::remove_file(&legacy_path);
        }

        let mut diffs: Vec<String> = Vec::new();
        for plugin_path in [Self::plugin_path(), Self::plugin_v2_path()] {
            if !plugin_path.exists() {
                continue;
            }
            let existing_content = fs::read_to_string(&plugin_path)?;
            diffs.push(generate_diff(&plugin_path, &existing_content, ""));
            if !dry_run {
                fs::remove_file(&plugin_path)?;
            }
        }

        if diffs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(diffs.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("git-ai.ts");
        (temp_dir, plugin_path)
    }

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    #[test]
    fn test_opencode_install_plugin_creates_file_from_scratch() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());

        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("GitAiPlugin"));
        assert!(content.contains("tool.execute.before"));
        assert!(content.contains("tool.execute.after"));
        // Token reporting must ship with the generated plugin, not a local patch.
        assert!(content.contains("session.idle"));
        assert!(content.contains("\"report-token-usage\", \"opencode\", \"--session-id\""));
        assert!(content.contains("runCommand(GIT_AI_BIN"));
        // Uses the opencode preset with session_id-based hook input and absolute path
        assert!(content.contains("session_id"));
        // Placeholder should be replaced with actual binary path in the const declaration
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
    }

    #[test]
    fn test_opencode_plugin_content_is_valid_typescript() {
        let content = OPENCODE_PLUGIN_CONTENT;

        assert!(content.contains("import type { Plugin }"));
        assert!(content.contains("@opencode-ai/plugin"));
        assert!(content.contains("export const GitAiPlugin: Plugin"));
        assert!(content.contains("\"tool.execute.before\""));
        assert!(content.contains("\"tool.execute.after\""));
        assert!(content.contains("FILE_EDIT_TOOLS"));
        assert!(content.contains("isBashTool"));
        assert!(content.contains("apply_patch"));
        // Template contains placeholder for binary path
        assert!(content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains("hook_event_name"));
        assert!(content.contains("session_id"));
        assert!(content.contains("PreToolUse"));
        assert!(content.contains("PostToolUse"));
    }

    #[test]
    fn test_opencode_plugin_placeholder_substitution() {
        let binary_path = create_test_binary_path();
        let content = OpenCodeInstaller::generate_plugin_content(&binary_path);

        // Placeholder should be replaced with the actual binary path in the const
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
        // Commands reference the const which now holds the absolute path
        assert!(content.contains("runCommand(GIT_AI_BIN, [\"--version\"]"));
        assert!(content.contains("runCommand(GIT_AI_BIN, [\"checkpoint\", \"opencode\""));
    }

    #[test]
    fn test_opencode_plugin_skips_if_already_exists() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();
        let content1 = fs::read_to_string(&plugin_path).unwrap();

        fs::write(&plugin_path, &generated).unwrap();
        let content2 = fs::read_to_string(&plugin_path).unwrap();

        assert_eq!(content1, content2);
    }

    #[test]
    fn test_opencode_plugin_updates_outdated_content() {
        let (_temp_dir, plugin_path) = setup_test_env();
        let binary_path = create_test_binary_path();

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let old_content = "// Old plugin version\nexport const OldPlugin = {}";
        fs::write(&plugin_path, old_content).unwrap();

        let content_before = fs::read_to_string(&plugin_path).unwrap();
        assert!(content_before.contains("OldPlugin"));

        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        let content_after = fs::read_to_string(&plugin_path).unwrap();
        assert!(content_after.contains("GitAiPlugin"));
        assert!(!content_after.contains("OldPlugin"));
    }

    #[test]
    fn test_opencode_plugin_windows_path_escaping() {
        let binary_path = PathBuf::from(r"C:\Users\foo\.easylife-ai\bin\git-ai.exe");
        let content = OpenCodeInstaller::generate_plugin_content(&binary_path);

        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        // Backslashes should be doubled for the TS string literal
        assert!(
            content
                .contains(r#"const GIT_AI_BIN = "C:\\Users\\foo\\.easylife-ai\\bin\\git-ai.exe""#)
        );
    }

    /// 1.x prefers a default export and then drops the hooks it returns, so the
    /// 1.x file must not have one. 2.x requires one, so the 2.x file must.
    #[test]
    fn test_opencode_templates_have_the_export_shape_each_version_requires() {
        assert!(!OPENCODE_PLUGIN_CONTENT.contains("export default"));
        assert!(OPENCODE_PLUGIN_CONTENT.contains("export const GitAiPlugin: Plugin"));

        assert!(OPENCODE_PLUGIN_V2_CONTENT.contains("export default gitAiPluginV2"));
        assert!(OPENCODE_PLUGIN_V2_CONTENT.contains("id: \"git-ai\""));
        assert!(!OPENCODE_PLUGIN_V2_CONTENT.contains("export const GitAiPlugin"));
        // 1.x also loads the 2.x file; it must bail out on the reduced context.
        assert!(OPENCODE_PLUGIN_V2_CONTENT.contains("typeof ctx?.tool?.hook"));
        // 2.x does not publish `session.idle` to plugins, so execution events trigger reports.
        assert!(OPENCODE_PLUGIN_V2_CONTENT.contains("session.execution.succeeded"));
        assert!(
            OPENCODE_PLUGIN_V2_CONTENT
                .contains("\"report-token-usage\", \"opencode\", \"--session-id\"")
        );
    }

    #[test]
    fn test_opencode_v2_plugin_placeholder_substitution() {
        let content = OpenCodeInstaller::generate_plugin_v2_content(&create_test_binary_path());
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
    }

    #[test]
    fn test_opencode_manages_both_plugin_files_in_plugins_dir() {
        let plugins = OpenCodeInstaller::managed_plugins(&create_test_binary_path());
        let names: Vec<String> = plugins
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["git-ai.ts", "git-ai-v2.ts"]);
        assert_eq!(
            plugins[0].0.parent(),
            plugins[1].0.parent(),
            "both files live in the same directory"
        );
        assert_ne!(plugins[0].1, plugins[1].1);
    }

    #[test]
    fn test_opencode_plugin_handles_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let binary_path = create_test_binary_path();
        let plugin_path = temp_dir
            .path()
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("git-ai.ts");

        assert!(!plugin_path.parent().unwrap().exists());

        if let Some(parent) = plugin_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let generated = OpenCodeInstaller::generate_plugin_content(&binary_path);
        fs::write(&plugin_path, &generated).unwrap();

        assert!(plugin_path.exists());
        let content = fs::read_to_string(&plugin_path).unwrap();
        assert!(content.contains("GitAiPlugin"));
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
    }
}
