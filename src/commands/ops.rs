//! `easylife-ai ops-check`: print a copy-paste prompt that points an external
//! AI coding agent at the operational skill installed under
//! `~/.easylife-ai/skills/`.
//!
//! This command performs no diagnosis itself. It only resolves the skill path
//! and emits the prompt text so the user can paste it into any agent tool.

use crate::config::{APP_DIR_NAME, skills_dir_path};
use crate::mdm::utils::home_dir;
use std::path::{Path, PathBuf};

/// Default skill used when the user does not name one.
const DEFAULT_SKILL: &str = "tracker-troubleshoot";

/// One-line purpose for each embedded operational skill, shown in `ops --list`
/// and when the requested skill cannot be found.
const SKILL_CATALOG: &[(&str, &str)] = &[(
    DEFAULT_SKILL,
    "排查 AI token 用量与代码统计上报问题（客户端装好但服务端收不到数据）",
)];

/// Placeholder the user replaces with their own question.
const QUESTION_PLACEHOLDER: &str = "<输入自己的问题>";

/// Shown in `--help` so users can find (and delete) the saved diagnosis.
const REPORT_HINT: &str = "~/.easylife-ai/doctor-last-report.txt";

/// Whether to include the doctor diagnosis section in the prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoctorSection<'a> {
    /// A saved report exists: inline it verbatim.
    Report(&'a str),
    /// No report on disk: keep the heading and say `doctor` has not run, so the
    /// agent knows the evidence is missing rather than concluding it is clean.
    NotRun,
    /// The user asked to omit the section entirely (`--no-doctor`).
    Omitted,
}

pub fn handle_ops(args: &[String]) {
    let mut skill_name: Option<String> = None;
    let mut question_parts: Vec<String> = Vec::new();
    let mut list_only = false;
    let mut no_doctor = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--list" | "-l" => list_only = true,
            // Accept the bare word too: users type `ops-check no-doctor`.
            "--no-doctor" | "no-doctor" => no_doctor = true,
            "--skill" | "-s" => {
                skill_name = iter.next().cloned().filter(|s| !s.trim().is_empty());
                if skill_name.is_none() {
                    eprintln!("error: --skill requires a skill name");
                    eprintln!("Run `easylife-ai ops-check --list` to see available skills.");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                print_ops_help();
                return;
            }
            other => question_parts.push(other.to_string()),
        }
    }

    if list_only {
        print_skill_list();
        return;
    }

    let name = skill_name.unwrap_or_else(|| DEFAULT_SKILL.to_string());
    let Some(skill_dir) = resolve_skill_dir(&name) else {
        eprintln!(
            "error: skill '{}' not found under {}",
            name,
            skills_root_display()
        );
        eprintln!();
        eprintln!("The skill is installed together with the client. To restore it:");
        eprintln!("  easylife-ai install-hooks");
        eprintln!();
        print_skill_list();
        std::process::exit(1);
    };

    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        eprintln!(
            "error: {} exists but {} is missing",
            skill_dir.display(),
            skill_md.display()
        );
        eprintln!("Run `easylife-ai install-hooks` to reinstall the skills.");
        std::process::exit(1);
    }

    let question = question_parts.join(" ");
    let question = if question.trim().is_empty() {
        QUESTION_PLACEHOLDER.to_string()
    } else {
        question
    };

    // Keep the report alive in this scope so the borrow stays valid; reading it
    // is pointless when the user asked for the section to be dropped.
    let report = if no_doctor {
        None
    } else {
        crate::commands::doctor::read_last_report()
    };
    let section = match (no_doctor, report.as_deref()) {
        (true, _) => DoctorSection::Omitted,
        (false, Some(text)) => DoctorSection::Report(text),
        (false, None) => DoctorSection::NotRun,
    };
    print_prompt(&skill_md, section, &question);
}

/// Build the prompt text. Kept separate from printing so it can be tested.
pub fn build_prompt(skill_md: &str, doctor: DoctorSection<'_>, question: &str) -> String {
    let intro = "读取easylife-ai的运维skill：";
    // `--no-doctor` drops the section and its blank-line separator, leaving the
    // original two-line prompt exactly as it was before diagnosis was added.
    let doctor_block = match doctor {
        DoctorSection::Omitted => String::new(),
        DoctorSection::Report(text) => {
            format!("\neasylife-ai doctor诊断情况如下：\n{}\n", text.trim())
        }
        DoctorSection::NotRun => format!(
            "\neasylife-ai doctor诊断情况如下：\n{}\n",
            "（尚未运行过 `easylife-ai doctor`，无诊断结果可用）"
        ),
    };
    format!(
        "{intro}{skill_md}\n\
         根据skill相关背景知识或指引，帮助用户排查和解决任何问题，\n\
         {doctor_block}\
         用户问题如下：\n\
         {question}\n"
    )
}

fn print_prompt(skill_md: &Path, doctor: DoctorSection<'_>, question: &str) {
    println!(
        "{}",
        build_prompt(&skill_md.display().to_string(), doctor, question)
    );
}

fn print_ops_help() {
    eprintln!("Usage: easylife-ai ops-check [question...] [--no-doctor]");
    eprintln!();
    eprintln!("Print a prompt that points an AI agent at the installed ops skill.");
    eprintln!("Nothing is diagnosed locally -- copy the output into any agent tool.");
    eprintln!();
    eprintln!("  [question...]        Your question; joined with spaces");
    eprintln!("                       Omit it to get a <输入自己的问题> placeholder");
    eprintln!("  --no-doctor          Omit the last `easylife-ai doctor` diagnosis section");
    eprintln!("                       (also accepted as a bare `no-doctor`)");
    eprintln!("  --skill, -s <name>   Use a specific skill (default: {DEFAULT_SKILL})");
    eprintln!("  --list, -l           List installed ops skills");
    eprintln!();
    eprintln!("By default the prompt embeds the transcript saved by the most recent");
    eprintln!("`easylife-ai doctor` run ({REPORT_HINT}).");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  easylife-ai ops-check");
    eprintln!("  easylife-ai ops-check 我提交了代码但后台没有数据");
    eprintln!("  easylife-ai ops-check no-doctor token 上报没反应");
}

/// Absolute path to the skills root, for display.
fn skills_root_display() -> String {
    skills_dir_path()
        .unwrap_or_else(|| home_dir().join(APP_DIR_NAME).join("skills"))
        .display()
        .to_string()
}

/// Resolve a skill name to its directory, verifying SKILL.md exists.
fn resolve_skill_dir(name: &str) -> Option<PathBuf> {
    let base = skills_dir_path()?;
    let dir = base.join(name);
    if dir.join("SKILL.md").is_file() {
        Some(dir)
    } else {
        None
    }
}

fn print_skill_list() {
    println!("Installed ops skills under {}:", skills_root_display());
    for (name, purpose) in SKILL_CATALOG {
        let installed = resolve_skill_dir(name).is_some();
        let mark = if installed { "✓" } else { "✗" };
        println!("  {mark} {name:<22} {purpose}");
    }
    println!();
    println!("Tip: `easylife-ai ops-check --skill <name> <question>` selects one explicitly.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_skill_path_and_question() {
        let out = build_prompt(
            "/home/u/.easylife-ai/skills/tracker-troubleshoot/SKILL.md",
            DoctorSection::Report("[1/7] 环境自检"),
            "没数据",
        );
        assert!(out.contains("读取easylife-ai的运维skill："));
        assert!(out.contains("/home/u/.easylife-ai/skills/tracker-troubleshoot/SKILL.md"));
        assert!(out.contains("根据skill相关背景知识或指引"));
        assert!(out.contains("用户问题如下："));
        assert!(out.contains("没数据"));
    }

    #[test]
    fn prompt_embeds_the_doctor_report() {
        let out = build_prompt(
            "/p/SKILL.md",
            DoctorSection::Report("[5/7] 代码上报\n✓ 上报成功"),
            "没数据",
        );
        assert!(out.contains("easylife-ai doctor诊断情况如下："));
        assert!(out.contains("[5/7] 代码上报"));
        assert!(out.contains("✓ 上报成功"));
        // The report must come before the question so the agent reads the
        // evidence first, then what to answer.
        let report_at = out.find("诊断情况如下").unwrap();
        let question_at = out.find("用户问题如下：").unwrap();
        assert!(report_at < question_at);
    }

    #[test]
    fn prompt_states_when_doctor_has_not_run() {
        let out = build_prompt("/p/SKILL.md", DoctorSection::NotRun, "没数据");
        assert!(out.contains("easylife-ai doctor诊断情况如下："));
        assert!(
            out.contains("尚未运行"),
            "a missing report must be stated, not left blank:\n{out}"
        );
    }

    /// `--no-doctor` must remove the heading, not leave an empty section.
    #[test]
    fn no_doctor_drops_the_section_entirely() {
        let out = build_prompt("/p/SKILL.md", DoctorSection::Omitted, "没数据");
        assert!(
            !out.contains("doctor"),
            "the word doctor must not appear at all:\n{out}"
        );
        assert!(!out.contains("尚未运行"));
        assert!(out.contains("用户问题如下："));
        assert!(out.contains("没数据"));
        // Back to the original two-line layout: no blank line between the
        // instruction line and the question.
        assert!(
            out.contains("帮助用户排查和解决任何问题，\n用户问题如下："),
            "omitting the section must not leave stray blank lines:\n{out}"
        );
    }

    #[test]
    fn prompt_preserves_multiline_question() {
        let out = build_prompt("/p/SKILL.md", DoctorSection::NotRun, "第一行\n第二行");
        assert!(out.contains("第一行\n第二行"));
    }

    #[test]
    fn default_skill_is_in_catalog() {
        assert!(SKILL_CATALOG.iter().any(|(name, _)| *name == DEFAULT_SKILL));
    }

    #[test]
    fn placeholder_is_used_when_question_is_empty() {
        let out = build_prompt("/p/SKILL.md", DoctorSection::NotRun, QUESTION_PLACEHOLDER);
        assert!(out.contains("<输入自己的问题>"));
    }
}
