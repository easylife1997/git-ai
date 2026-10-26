//! 删除行来源：对本提交删掉的每一行，在父提交上 `git blame`，
//! 得到它是哪次提交、哪个文件、第几行引入的。
//!
//! 服务端用它精确判定「哪一行 AI 代码被删了」，不再按「文件 + 行内容」猜：
//! 人工删掉一行内容相同的 `}` 不会误伤 AI 行，内容相同的 AI 行也能分清是哪一行。
//!
//! 上报格式（按父提交里的旧路径分组，区间按旧行号升序）：
//! `{ "旧路径": [[旧起始行, 行数, 来源提交, 来源起始行, 来源路径], ...] }`
//! - `null`：合并提交或执行失败，服务端回退到按内容匹配。
//! - `{}`：没有删除行（含根提交）。

use crate::git::repository::exec_git;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 单次上报最多 blame 的删除行数，超出部分不上报来源（服务端对这些行回退按内容匹配）。
const MAX_BLAME_LINES: u32 = 20_000;
/// 每次 `git blame` 调用最多带的 `-L` 区间数，避免参数过长。
const MAX_RANGES_PER_CALL: usize = 200;

/// 一个被 blame 的行：父提交里的行号 → 来源提交 / 来源行号 / 来源路径。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlamedLine {
    final_line: u32,
    sha: String,
    orig_line: u32,
    orig_path: String,
}

pub fn deleted_line_origins(repo_path: &str, commit_sha: &str) -> Value {
    collect(repo_path, commit_sha).unwrap_or(Value::Null)
}

fn git<S: AsRef<str>>(repo_path: &str, args: &[S]) -> Option<String> {
    let mut full = vec!["-C".to_string(), repo_path.to_string()];
    full.extend(args.iter().map(|a| a.as_ref().to_string()));
    let out = exec_git(&full).ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn collect(repo_path: &str, commit_sha: &str) -> Option<Value> {
    // 父提交数：0 = 根提交（没有删除）；>1 = 合并提交（父提交不唯一，交给服务端回退）
    let parents_line = git(repo_path, &["rev-list", "--parents", "-n", "1", commit_sha])?;
    let parents: Vec<&str> = parents_line.split_whitespace().skip(1).collect();
    match parents.len() {
        0 => return Some(json!({})),
        1 => {}
        _ => return None,
    }
    let parent = parents[0];

    // -U0：hunk 头里的旧区间就是删除行；前缀固定为 a/ b/，不受 diff.mnemonicPrefix 影响
    let diff = git(
        repo_path,
        &[
            "-c",
            "core.quotepath=off",
            "show",
            "--format=",
            "--no-color",
            "--no-ext-diff",
            "-U0",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            commit_sha,
        ],
    )?;

    let mut budget = MAX_BLAME_LINES;
    let mut out = serde_json::Map::new();
    for (old_path, ranges) in deleted_ranges(&diff) {
        let ranges = cap_ranges(&ranges, &mut budget);
        if ranges.is_empty() {
            break;
        }
        // 某个文件 blame 失败只丢这个文件，服务端对它回退按内容匹配
        let Some(blamed) = blame_file(repo_path, parent, &old_path, &ranges) else {
            continue;
        };
        let groups = group_blamed(&blamed);
        if !groups.is_empty() {
            out.insert(old_path, Value::Array(groups));
        }
    }
    Some(Value::Object(out))
}

fn blame_file(
    repo_path: &str,
    parent: &str,
    path: &str,
    ranges: &[(u32, u32)],
) -> Option<Vec<BlamedLine>> {
    let mut all = Vec::new();
    for chunk in ranges.chunks(MAX_RANGES_PER_CALL) {
        let mut args: Vec<String> = vec![
            "-c".into(),
            "core.quotepath=off".into(),
            "blame".into(),
            "--line-porcelain".into(),
        ];
        for (start, count) in chunk {
            args.push("-L".into());
            args.push(format!("{},+{}", start, count));
        }
        args.push(parent.to_string());
        args.push("--".into());
        args.push(path.to_string());
        all.extend(parse_porcelain(&git(repo_path, &args)?));
    }
    Some(all)
}

/// 从剩余额度里截取区间；额度用完后返回空。
fn cap_ranges(ranges: &[(u32, u32)], budget: &mut u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for &(start, count) in ranges {
        if *budget == 0 {
            break;
        }
        let take = count.min(*budget);
        out.push((start, take));
        *budget -= take;
    }
    out
}

/// 解析 `-U0` 的 diff：旧路径 → 删除区间 `(旧起始行, 行数)`。只看代码文件（与 diff_gz 口径一致）。
fn deleted_ranges(diff: &str) -> BTreeMap<String, Vec<(u32, u32)>> {
    let mut out: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    let mut in_code = false;
    let mut in_hunk = false;
    let mut old_path: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_code = super::diff::is_code_diff_header(line);
            in_hunk = false;
            old_path = None;
            continue;
        }
        if line.starts_with("diff --cc ") || line.starts_with("diff --combined ") {
            in_code = false;
            continue;
        }
        if !in_code {
            continue;
        }
        // hunk 内容里以 "--- " 开头的删除行（如删掉一行 SQL 注释 "-- x"）不是文件头
        if !in_hunk {
            if let Some(p) = line.strip_prefix("rename from ") {
                old_path = Some(unquote(p));
                continue;
            }
            if let Some(p) = line.strip_prefix("--- ") {
                let p = unquote(p.trim_end_matches('\t'));
                if p != "/dev/null" {
                    old_path = Some(p.strip_prefix("a/").unwrap_or(&p).to_string());
                }
                continue;
            }
        }
        if line.starts_with("@@") {
            in_hunk = true;
            if let (Some(path), Some((start, count))) = (&old_path, parse_old_range(line))
                && count > 0
                && start > 0
            {
                out.entry(path.clone()).or_default().push((start, count));
            }
        }
    }
    out
}

/// hunk 头 `@@ -a,b +c,d @@` 的旧区间 `(a, b)`；省略行数时为 1。
fn parse_old_range(header: &str) -> Option<(u32, u32)> {
    let token = header
        .trim_start_matches('@')
        .split_whitespace()
        .find(|t| t.starts_with('-'))?;
    let mut parts = token[1..].splitn(2, ',');
    let start = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// 解析 `git blame --line-porcelain`：每行都带完整头，`\t` 开头的内容行结束一条记录。
fn parse_porcelain(text: &str) -> Vec<BlamedLine> {
    let mut out = Vec::new();
    let mut cur: Option<(String, u32, u32)> = None; // (sha, orig_line, final_line)
    let mut path: Option<String> = None;
    for line in text.lines() {
        if line.starts_with('\t') {
            if let (Some((sha, orig_line, final_line)), Some(orig_path)) = (cur.take(), path.take())
            {
                out.push(BlamedLine {
                    final_line,
                    sha,
                    orig_line,
                    orig_path,
                });
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("filename ") {
            path = Some(unquote(p));
            continue;
        }
        // 头行：<sha> <来源行> <当前行> [<组内行数>]
        let mut it = line.split(' ');
        if let (Some(sha), Some(orig), Some(fin)) = (it.next(), it.next(), it.next()) {
            let is_sha =
                (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit());
            if is_sha && let (Ok(o), Ok(f)) = (orig.parse(), fin.parse()) {
                cur = Some((sha.to_string(), o, f));
                path = None;
            }
        }
    }
    out
}

/// 合并连续的行：旧行号、来源行号同步递增且来源提交和路径相同。
fn group_blamed(lines: &[BlamedLine]) -> Vec<Value> {
    let mut groups: Vec<(u32, u32, &str, u32, &str)> = Vec::new();
    for l in lines {
        if let Some(g) = groups.last_mut()
            && g.2 == l.sha
            && g.4 == l.orig_path
            && l.final_line == g.0 + g.1
            && l.orig_line == g.3 + g.1
        {
            g.1 += 1;
            continue;
        }
        groups.push((l.final_line, 1, &l.sha, l.orig_line, &l.orig_path));
    }
    groups
        .into_iter()
        .map(|(start, count, sha, orig_start, orig_path)| {
            json!([start, count, sha, orig_start, orig_path])
        })
        .collect()
}

/// 解码 git 的带引号路径（`"src/\344\270\255.rs"`）；未加引号的原样返回。
fn unquote(s: &str) -> String {
    if !(s.len() >= 2 && s.starts_with('"') && s.ends_with('"')) {
        return s.to_string();
    }
    let inner = &s.as_bytes()[1..s.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' || i + 1 >= inner.len() {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        if i + 4 <= inner.len()
            && inner[i + 1..i + 4]
                .iter()
                .all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (inner[i + 1] - b'0') as u32 * 64
                + (inner[i + 2] - b'0') as u32 * 8
                + (inner[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
            continue;
        }
        out.push(match inner[i + 1] {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            other => other, // \" 和 \\
        });
        i += 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_old_range_from_hunk_headers() {
        assert_eq!(parse_old_range("@@ -5,3 +5,0 @@ fn x()"), Some((5, 3)));
        assert_eq!(parse_old_range("@@ -7 +6 @@"), Some((7, 1)));
        assert_eq!(parse_old_range("@@ -0,0 +1,2 @@"), Some((0, 0)));
    }

    #[test]
    fn deleted_ranges_follow_old_paths_and_skip_non_code() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
            @@ -2,2 +1,0 @@\n-x\n-y\n@@ -9 +7,2 @@\n-z\n+p\n+q\n\
            diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +0,0 @@\n-r\n\
            diff --git a/db/q.sql b/db/q.sql\n--- a/db/q.sql\n+++ b/db/q.sql\n@@ -3 +2,0 @@\n--- note\n\
            diff --git a/old/n.go b/new/n.go\nsimilarity index 90%\nrename from old/n.go\nrename to new/n.go\n\
            --- a/old/n.go\n+++ b/new/n.go\n@@ -4 +4 @@\n-a\n+b\n\
            diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-1\n-2\n";
        let got = deleted_ranges(diff);
        let want: BTreeMap<String, Vec<(u32, u32)>> = [
            ("db/q.sql".to_string(), vec![(3, 1)]),
            ("gone.py".to_string(), vec![(1, 2)]),
            ("old/n.go".to_string(), vec![(4, 1)]),
            ("src/a.rs".to_string(), vec![(2, 2), (9, 1)]),
        ]
        .into_iter()
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn cap_ranges_stops_at_budget() {
        let mut budget = 5;
        assert_eq!(
            cap_ranges(&[(1, 3), (10, 4), (20, 1)], &mut budget),
            vec![(1, 3), (10, 2)]
        );
        assert_eq!(budget, 0);
        assert!(cap_ranges(&[(30, 1)], &mut budget).is_empty());
    }

    #[test]
    fn parses_line_porcelain_and_groups_consecutive_lines() {
        let s1 = "1111111111111111111111111111111111111111";
        let s2 = "2222222222222222222222222222222222222222";
        let text = format!(
            "{s1} 3 2 2\nauthor A\nsummary x 1 2\nfilename src/a.rs\n\tl2\n\
             {s1} 4 3\nauthor A\nprevious {s2} src/a.rs\nfilename src/a.rs\n\tl3\n\
             {s2} 1 9 1\nauthor B\nfilename \"src/\\344\\270\\255.rs\"\n\tl9\n"
        );
        let lines = parse_porcelain(&text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].orig_path, "src/中.rs");
        assert_eq!(
            Value::Array(group_blamed(&lines)),
            json!([[2, 2, s1, 3, "src/a.rs"], [9, 1, s2, 1, "src/中.rs"]])
        );
    }

    #[test]
    fn unquote_handles_plain_and_escaped_paths() {
        assert_eq!(unquote("plain/p.rs"), "plain/p.rs");
        assert_eq!(unquote("\"a \\\"q\\\"\\\\b\""), "a \"q\"\\b");
    }

    #[test]
    fn blames_deleted_lines_to_the_commit_that_introduced_them() {
        use crate::git::test_utils::TmpRepo;

        let repo = TmpRepo::new().expect("tmp repo");
        let path = repo.path().to_str().unwrap().to_string();
        repo.write_file("src/a.rs", "fn a() {\n}\n", true).unwrap();
        repo.commit_with_message("c1").unwrap();
        let c1 = repo.head_commit_sha().unwrap();
        repo.write_file("src/a.rs", "fn a() {\n}\nfn b() {\n}\n", true)
            .unwrap();
        repo.commit_with_message("c2").unwrap();
        let c2 = repo.head_commit_sha().unwrap();
        // 删掉第 4 行的 "}"：内容和第 2 行（c1 引入）相同，但来源必须是 c2 的第 4 行
        repo.write_file("src/a.rs", "fn a() {\n}\nfn b() {\n", true)
            .unwrap();
        repo.commit_with_message("c3").unwrap();
        let c3 = repo.head_commit_sha().unwrap();

        assert_eq!(
            deleted_line_origins(&path, &c3),
            json!({ "src/a.rs": [[4, 1, c2, 4, "src/a.rs"]] })
        );
        // 根提交没有删除行
        assert_eq!(deleted_line_origins(&path, &c1), json!({}));
    }
}
