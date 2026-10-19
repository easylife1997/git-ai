//! Argument validation must never trigger real uploads or hook installation.
use crate::repos::test_repo::TestRepo;

#[test]
fn doctor_help_has_no_side_effects() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let output = repo
        .git_ai_with_env(
            &["doctor", "--help"],
            &[("HOME", home.path().to_str().unwrap())],
        )
        .unwrap();
    assert!(output.contains("--repo-url"));
    assert!(output.contains("--json"));
    assert!(!home.path().join(".easylife-ai/tracker-upload.log").exists());
}

#[test]
fn doctor_rejects_credentials_before_side_effects() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let error = repo
        .git_ai_with_env(
            &[
                "doctor",
                "--repo-url",
                "https://user:secret@example.com/repo.git",
            ],
            &[("HOME", home.path().to_str().unwrap())],
        )
        .expect_err("credentials must be rejected");
    assert!(error.contains("--repo-url"));
    assert!(!home.path().join(".easylife-ai/tracker-upload.log").exists());
    assert!(!home.path().join(".claude/settings.json").exists());
}

#[test]
fn doctor_rejects_unknown_arguments_before_side_effects() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let error = repo
        .git_ai_with_env(
            &["doctor", "--unknown"],
            &[("HOME", home.path().to_str().unwrap())],
        )
        .expect_err("unknown option must fail");
    assert!(error.contains("unknown argument"));
    assert!(!home.path().join(".easylife-ai/tracker-upload.log").exists());
}

#[test]
fn legacy_cleanup_report_never_touches_the_legacy_directory() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let legacy = home.path().join(".git-ai");
    std::fs::create_dir_all(legacy.join("internal")).unwrap();
    std::fs::write(legacy.join("internal/distinct_id"), b"kept").unwrap();

    let output = repo
        .git_ai_with_env(
            &["legacy-cleanup"],
            &[("HOME", home.path().to_str().unwrap())],
        )
        .unwrap();

    assert!(output.contains(".git-ai"));
    assert!(legacy.exists(), "report mode must not remove the directory");
    assert_eq!(
        std::fs::read(legacy.join("internal/distinct_id")).unwrap(),
        b"kept"
    );
}

#[test]
fn legacy_cleanup_rejects_conflicting_flags() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let error = repo
        .git_ai_with_env(
            &["legacy-cleanup", "--yes", "--purge"],
            &[("HOME", home.path().to_str().unwrap())],
        )
        .expect_err("conflicting flags must fail");
    assert!(error.contains("不能同时使用"));
    assert!(!home.path().join(".git-ai.legacy-test").exists());
}
