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
        for platform in ["claude-code", "codex", "cursor", "opencode"] {
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

mod cursor_report {
    use crate::repos::test_repo::{TestRepo, get_binary_path};
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    /// Minimal tracker stub: records each POST body and acknowledges it.
    fn spawn_tracker_stub() -> (String, Receiver<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                let Some(start) = body_start else { continue };
                let headers = String::from_utf8_lossy(&buf[..start]).to_lowercase();
                let len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < start + len {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                if let Ok(body) = serde_json::from_slice::<Value>(&buf[start..]) {
                    let _ = tx.send(body);
                }
                let resp =
                    r#"{"code":200,"data":{"success":true,"message":"上报成功","recordId":7}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp.len(),
                    resp
                );
            }
        });
        (url, rx)
    }

    fn setup_home(tracker_url: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".easylife-ai");
        std::fs::create_dir_all(&dir).unwrap();
        let config = json!({
            "tracker_url": tracker_url, "team_id": "1", "team_key": "test-key",
            "username": "test-user", "blacklist": [], "token_report": true
        });
        std::fs::write(dir.join("tracker-config.json"), config.to_string()).unwrap();
        home
    }

    fn write_transcript(dir: &std::path::Path) -> String {
        let lines = [
            json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfirst\n</user_query>"}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"answer one"}]}}),
            json!({"type":"turn_ended","status":"success"}),
            json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nsecond question\n</user_query>"}]}}),
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"answer two"}]}}),
            json!({"type":"turn_ended","status":"success"}),
        ];
        let path = dir.join("conv-e2e.jsonl");
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&path, body.join("\n")).unwrap();
        path.to_string_lossy().to_string()
    }

    /// Run `report-token-usage cursor` as the Cursor `stop` hook does: payload on
    /// stdin, working directory unrelated to the repo. Returns (exit ok, stdout).
    fn run_stop_hook(home: &std::path::Path, payload: &Value) -> (bool, String) {
        let mut child = Command::new(get_binary_path())
            .args(["report-token-usage", "cursor"])
            .current_dir(home)
            .env("HOME", home)
            .env("GIT_AI_TEST_DB_PATH", home.join("test.db"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    }

    #[test]
    fn stop_hook_reports_real_tokens_with_prompt_and_dedups() {
        let repo = TestRepo::new();
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://example.com/acme/demo.git",
        ])
        .unwrap();
        let (url, rx) = spawn_tracker_stub();
        let home = setup_home(&url);
        let transcript = write_transcript(home.path());
        let payload = json!({
            "conversation_id": "conv-e2e", "generation_id": "g1", "status": "completed",
            "model": "default", "transcript_path": transcript,
            "workspace_roots": [repo.path().to_string_lossy()],
            "input_tokens": 1000, "output_tokens": 200,
            "cache_read_tokens": 300, "cache_write_tokens": 40
        });

        let (ok, stdout) = run_stop_hook(home.path(), &payload);
        assert!(ok);
        // stdout is parsed by Cursor as the hook response; it must stay empty.
        assert_eq!(stdout, "");

        let body = rx.recv_timeout(Duration::from_secs(5)).expect("one POST");
        assert_eq!(body["platform"], "cursor");
        assert_eq!(body["session_id"], "conv-e2e");
        assert_eq!(body["turn_index"], 2);
        assert_eq!(body["input_tokens"], 1000);
        assert_eq!(body["output_tokens"], 200);
        assert_eq!(body["cache_read_tokens"], 300);
        assert_eq!(body["cache_creation_tokens"], 40);
        assert_eq!(body["total_tokens"], 1540);
        assert_eq!(body["user_prompts"], "second question");
        assert_eq!(body["assistant_responses"], "answer two");
        assert_eq!(body["repo_url"], "https://example.com/acme/demo.git");
        assert_eq!(body["username"], "test-user");
        assert!(body.get("cost_usd").is_none(), "cost is left to the server");

        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".easylife-ai/reported-turns.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["cursor:conv-e2e"], 2);

        // Same turn fired again (e.g. hook retry) must not be uploaded twice.
        let (ok, _) = run_stop_hook(home.path(), &payload);
        assert!(ok);
        assert!(rx.recv_timeout(Duration::from_millis(700)).is_err());
    }

    #[test]
    fn stop_hook_never_reports_missing_or_zero_tokens() {
        let (url, rx) = spawn_tracker_stub();
        let home = setup_home(&url);
        let transcript = write_transcript(home.path());
        for tokens in [
            json!({}),
            json!({"input_tokens": 0, "output_tokens": 0}),
            json!({"input_tokens": null, "output_tokens": null}),
        ] {
            let mut payload = json!({
                "conversation_id": "conv-e2e", "status": "completed",
                "transcript_path": transcript, "workspace_roots": []
            });
            payload
                .as_object_mut()
                .unwrap()
                .extend(tokens.as_object().unwrap().clone());
            let (ok, stdout) = run_stop_hook(home.path(), &payload);
            assert!(ok, "cursor hook must always exit 0");
            assert_eq!(stdout, "");
        }
        assert!(rx.recv_timeout(Duration::from_millis(700)).is_err());
        assert!(
            !home
                .path()
                .join(".easylife-ai/reported-turns.json")
                .exists()
        );
    }

    #[test]
    fn stop_hook_with_garbage_stdin_exits_zero() {
        let (url, _rx) = spawn_tracker_stub();
        let home = setup_home(&url);
        let (ok, stdout) = run_stop_hook(home.path(), &json!("not an object"));
        assert!(ok);
        assert_eq!(stdout, "");
    }
}
