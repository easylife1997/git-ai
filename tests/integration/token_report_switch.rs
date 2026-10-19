use crate::repos::test_repo::TestRepo;

#[test]
fn token_report_disabled_all_platforms_have_no_network_or_state_changes() {
    let repo = TestRepo::new();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".easylife-ai");
    std::fs::create_dir_all(&dir).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = b"{\"existing-session\":3}";
    let log = b"existing upload log\n";
    std::fs::write(dir.join("reported-turns.json"), state).unwrap();
    std::fs::write(dir.join("tracker-upload.log"), log).unwrap();

    // Malformed switch values must not fall back to enabling reporting.
    for switch in [
        serde_json::json!(false),
        serde_json::json!("false"),
        serde_json::Value::Null,
    ] {
        let config = serde_json::json!({
            "tracker_url": format!("http://{}", listener.local_addr().unwrap()),
            "team_id": "1", "team_key": "test-key", "username": "test-user",
            "blacklist": [], "token_report": switch
        });
        std::fs::write(dir.join("tracker-config.json"), config.to_string()).unwrap();
        for platform in ["claude-code", "codex", "opencode"] {
            repo.git_ai_with_env(
                &["report-token-usage", platform],
                &[("HOME", home.path().to_str().unwrap())],
            )
            .unwrap();
            assert_eq!(
                std::fs::read(dir.join("reported-turns.json")).unwrap(),
                state
            );
            assert_eq!(std::fs::read(dir.join("tracker-upload.log")).unwrap(), log);
        }
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
