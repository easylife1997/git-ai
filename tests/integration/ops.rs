//! Integration tests for the `ops-check` command.
//!
//! The command prints a prompt that points an external AI agent at the
//! operational skill installed under `~/.easylife-ai/skills/`. It must not
//! depend on a running daemon, and it must give a recoverable error when the
//! skill is missing (e.g. right after a manual upgrade).

use crate::repos::test_repo::TestRepo;
use std::fs;

const SKILL: &str = "tracker-troubleshoot";

/// Build an isolated HOME containing the skill, mirroring a real client install.
fn seed_skill_home(home: &std::path::Path) {
    let dir = home.join(".easylife-ai").join("skills").join(SKILL);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: tracker-troubleshoot\ndescription: test\n---\n# Test skill\n",
    )
    .unwrap();
}

#[test]
fn ops_inlines_the_saved_doctor_report() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-doc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);

    // Stand in for a real `easylife-ai doctor` run: the command persists its
    // transcript here and ops-check must pick it up without re-running it.
    let report = home.join(".easylife-ai").join("doctor-last-report.txt");
    fs::write(
        &report,
        "[5/7] 代码上报\n      ✓ 上报成功（recordId=123）\n[6/7] token 上报（3 轮）\n      ⊘ 已禁用\n",
    )
    .unwrap();

    let out = repo
        .git_ai_with_env(
            &["ops-check", "后台没有数据"],
            &[("HOME", home.to_str().unwrap())],
        )
        .unwrap();

    assert!(
        out.contains("easylife-ai doctor诊断情况如下："),
        "prompt must have a doctor section:\n{out}"
    );
    assert!(
        out.contains("recordId=123") && out.contains("⊘ 已禁用"),
        "the saved transcript must be inlined verbatim:\n{out}"
    );
    assert!(
        !out.contains("尚未运行"),
        "a present report must not be reported as missing:\n{out}"
    );
    // The transcript must precede the question so the agent reads evidence first.
    assert!(
        out.find("recordId=123").unwrap() < out.find("用户问题如下：").unwrap(),
        "diagnosis must come before the question:\n{out}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_says_so_when_doctor_has_not_run() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-nodoc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);
    // No doctor-last-report.txt.

    let out = repo
        .git_ai_with_env(
            &["ops-check", "没数据"],
            &[("HOME", home.to_str().unwrap())],
        )
        .unwrap();

    assert!(out.contains("easylife-ai doctor诊断情况如下："));
    assert!(
        out.contains("尚未运行"),
        "missing report must be stated explicitly, not left blank:\n{out}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_no_doctor_omits_the_section_even_when_a_report_exists() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-nd-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);
    // A report IS present: the flag must still suppress it.
    fs::write(
        home.join(".easylife-ai").join("doctor-last-report.txt"),
        "[5/7] 代码上报\n      ✓ 上报成功（recordId=123）\n",
    )
    .unwrap();

    for flag in ["--no-doctor", "no-doctor"] {
        let out = repo
            .git_ai_with_env(
                &["ops-check", flag, "后台没有数据"],
                &[("HOME", home.to_str().unwrap())],
            )
            .unwrap();

        assert!(
            !out.contains("doctor"),
            "`{flag}` must drop the whole doctor section:\n{out}"
        );
        assert!(
            !out.contains("recordId=123"),
            "`{flag}` must not inline the saved report:\n{out}"
        );
        assert!(out.contains("读取easylife-ai的运维skill："));
        assert!(out.contains("后台没有数据"));
        assert!(out.contains("用户问题如下："));
    }

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_prints_prompt_template_without_question() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);

    let out = repo
        .git_ai_with_env(&["ops-check"], &[("HOME", home.to_str().unwrap())])
        .unwrap();

    let expected_path = home
        .join(".easylife-ai")
        .join("skills")
        .join(SKILL)
        .join("SKILL.md");
    assert!(
        out.contains(&expected_path.display().to_string()),
        "prompt must reference the installed SKILL.md path, got:\n{out}"
    );
    assert!(out.contains("读取easylife-ai的运维skill："));
    assert!(out.contains("根据skill相关背景知识或指引"));
    assert!(out.contains("用户问题如下："));
    // No question supplied -> the user is told where to type theirs.
    assert!(
        out.contains("<输入自己的问题>"),
        "expected placeholder, got:\n{out}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_inlines_user_question() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-q-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);

    let out = repo
        .git_ai_with_env(
            &["ops-check", "我提交了代码", "但后台没有数据"],
            &[("HOME", home.to_str().unwrap())],
        )
        .unwrap();

    // Multiple shell arguments are joined with a single space.
    assert!(
        out.contains("我提交了代码 但后台没有数据"),
        "question should be joined, got:\n{out}"
    );
    assert!(!out.contains("<输入自己的问题>"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_removed_old_command_names_are_rejected() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-old-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    seed_skill_home(&home);

    // `ops` and `运维` were superseded by `ops-check`; they must no longer be
    // accepted, so users get a clear error instead of a silent behaviour change.
    for old in ["ops", "运维"] {
        let err = repo
            .git_ai_with_env(&[old], &[("HOME", home.to_str().unwrap())])
            .expect_err("old command name must be rejected");
        assert!(
            err.contains("Unknown git-ai command"),
            "`{old}` should be reported as unknown, got:\n{err}"
        );
    }

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_missing_skill_gives_recovery_hint() {
    let repo = TestRepo::new();
    let home = std::env::temp_dir().join(format!("git-ai-ops-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    // Deliberately no skill installed.
    fs::create_dir_all(home.join(".easylife-ai")).unwrap();

    let err = repo
        .git_ai_with_env(&["ops-check"], &[("HOME", home.to_str().unwrap())])
        .expect_err("ops-check must fail when the skill is absent");

    assert!(
        err.contains("not found"),
        "error should say the skill is missing, got:\n{err}"
    );
    assert!(
        err.contains("install-hooks"),
        "error should tell the user how to restore the skill, got:\n{err}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ops_list_reports_install_state() {
    let repo = TestRepo::new();

    let with_skill = std::env::temp_dir().join(format!("git-ai-ops-l1-{}", std::process::id()));
    let _ = fs::remove_dir_all(&with_skill);
    fs::create_dir_all(&with_skill).unwrap();
    seed_skill_home(&with_skill);

    let out = repo
        .git_ai_with_env(
            &["ops-check", "--list"],
            &[("HOME", with_skill.to_str().unwrap())],
        )
        .unwrap();
    assert!(out.contains(SKILL));
    assert!(
        out.contains('✓'),
        "installed skill should be marked:\n{out}"
    );

    let without_skill = std::env::temp_dir().join(format!("git-ai-ops-l2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&without_skill);
    fs::create_dir_all(without_skill.join(".easylife-ai")).unwrap();

    let out = repo
        .git_ai_with_env(
            &["ops-check", "--list"],
            &[("HOME", without_skill.to_str().unwrap())],
        )
        .unwrap();
    assert!(out.contains('✗'), "missing skill should be marked:\n{out}");

    let _ = fs::remove_dir_all(&with_skill);
    let _ = fs::remove_dir_all(&without_skill);
}
