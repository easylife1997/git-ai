pub mod config;
pub mod diff;
pub mod filter;
pub mod log;
pub mod notes;
pub mod origins;
pub mod retry;
pub mod upload;

use std::collections::HashMap;
use std::process::Command;

pub fn report_pushed_commits(
    repo_path: &str,
    pre_push_refs: &HashMap<String, String>,
    remote: &str,
) {
    let config = match config::load_config() {
        Some(c) => c,
        None => {
            tracing::debug!(
                "tracker: config not found at {:?}, skipping upload",
                config::config_path()
            );
            return;
        }
    };

    // Resolve to work tree if repo_path points to .git directory
    let work_tree = resolve_work_tree(repo_path);
    tracing::debug!(
        "tracker: processing push to remote {} for repo {} (work_tree: {})",
        remote,
        repo_path,
        work_tree
    );
    tracing::debug!("tracker: pre_push_refs = {:?}", pre_push_refs);

    let repo_url = get_remote_url(&work_tree, remote).unwrap_or_default();
    let post_push_refs = get_remote_refs(&work_tree, remote);
    tracing::debug!("tracker: post_push_refs = {:?}", post_push_refs);

    for (branch, old_sha) in pre_push_refs {
        let new_sha = match post_push_refs.get(branch) {
            Some(sha) if sha != old_sha => sha,
            _ => {
                tracing::debug!(
                    "tracker: branch {} unchanged or not found in post_push_refs",
                    branch
                );
                continue;
            }
        };

        tracing::debug!(
            "tracker: branch {} changed from {} to {}",
            branch,
            old_sha,
            new_sha
        );
        let commits = get_commits_in_range(&work_tree, old_sha, new_sha);
        tracing::debug!("tracker: found {} commits to process", commits.len());

        for commit_sha in commits {
            if let Err(reason) =
                filter::should_upload(&work_tree, &repo_url, &commit_sha, &config.blacklist)
            {
                log::append_log(
                    log::LogStatus::Skipped,
                    log::LogKind::Code,
                    &commit_sha,
                    remote,
                    branch,
                    &work_tree,
                    Some(&reason),
                    None,
                );
                continue;
            }

            let diff_gz = match diff::collect_code_diff(&work_tree, &commit_sha) {
                Ok(gz) => gz,
                Err(e) => {
                    tracing::debug!("tracker diff failed {}: {}", &commit_sha, e);
                    continue;
                }
            };

            match upload::upload_commit(
                &work_tree,
                &commit_sha,
                diff_gz.clone(),
                &config,
                remote,
                branch,
            ) {
                Ok(outcome) => {
                    if outcome.accepted() {
                        let _ = notes::mark_reported(&work_tree, &commit_sha);
                    } else {
                        let _ =
                            retry::save_to_queue(&work_tree, &commit_sha, diff_gz, remote, branch);
                    }
                    // A 2xx response is not proof of storage -- the server also
                    // returns 200 when it rejects the row.  Record which happened.
                    let status = if outcome.saved {
                        log::LogStatus::Uploaded
                    } else {
                        log::LogStatus::NotSaved
                    };
                    log::append_log(
                        status,
                        log::LogKind::Code,
                        &commit_sha,
                        remote,
                        branch,
                        &work_tree,
                        None,
                        Some(&outcome),
                    );
                }
                Err(e) => {
                    tracing::debug!("tracker upload failed {}: {}", &commit_sha, e);
                    log::append_log(
                        log::LogStatus::Failed,
                        log::LogKind::Code,
                        &commit_sha,
                        remote,
                        branch,
                        &work_tree,
                        Some(&e),
                        None,
                    );
                    let _ = retry::save_to_queue(&work_tree, &commit_sha, diff_gz, remote, branch);
                }
            }
        }
    }
}

fn get_remote_url(repo_path: &str, remote: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", repo_path, "remote", "get-url", remote])
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

fn get_remote_refs(repo_path: &str, remote: &str) -> HashMap<String, String> {
    let output = match Command::new("git")
        .args(["-C", repo_path, "ls-remote", "--heads", remote])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                "tracker: ls-remote failed (status {}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return HashMap::new();
        }
        Err(e) => {
            tracing::debug!("tracker: ls-remote error: {}", e);
            return HashMap::new();
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut refs = HashMap::new();

    for line in text.lines() {
        // Trim any trailing \r for Windows CRLF output
        let line = line.trim_end_matches('\r');
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() == 2 {
            let sha = parts[0].trim();
            let refname = parts[1].trim().trim_start_matches("refs/heads/");
            refs.insert(refname.to_string(), sha.to_string());
        }
    }

    refs
}

fn get_commits_in_range(repo_path: &str, old_sha: &str, new_sha: &str) -> Vec<String> {
    let output = match Command::new("git")
        .args([
            "-C",
            repo_path,
            "rev-list",
            "--ancestry-path",
            &format!("{}..{}", old_sha, new_sha),
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().map(|s| s.to_string()).collect()
}

pub(super) fn resolve_work_tree(repo_path: &str) -> String {
    // Normalize path separators for cross-platform compatibility
    let path = repo_path.trim_end_matches('/').trim_end_matches('\\');
    if path.ends_with(".git") {
        path.trim_end_matches(".git")
            .trim_end_matches('/')
            .trim_end_matches('\\')
            .to_string()
    } else {
        let output = Command::new("git")
            .args(["-C", path, "rev-parse", "--show-toplevel"])
            .output();
        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => path.to_string(),
        }
    }
}
