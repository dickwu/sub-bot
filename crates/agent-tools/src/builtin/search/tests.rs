use super::*;
use std::sync::Arc;

/// 带 `.git` 的临时工作区（ignore crate 与 rg 一样只在 git 仓库里应用 .gitignore）。
fn ws(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    for (path, content) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    dir
}

async fn run(tool: &dyn AgentTool, args: Value) -> Result<ToolResult> {
    tool.execute("1", args, CancellationToken::new(), Arc::new(|_| {})).await
}

fn text(r: &ToolResult) -> String {
    r.content.iter().filter_map(|c| if let agent_ai::Content::Text { text } = c { Some(text.as_str()) } else { None }).collect()
}

async fn grep(dir: &tempfile::TempDir, args: Value) -> String {
    let r = run(&GrepTool { root: dir.path().to_owned() }, args).await.unwrap();
    assert!(!r.is_error);
    text(&r)
}

async fn grep_err(dir: &tempfile::TempDir, args: Value) -> String {
    run(&GrepTool { root: dir.path().to_owned() }, args).await.unwrap_err().to_string()
}

async fn find(dir: &tempfile::TempDir, args: Value) -> String {
    let r = run(&FindTool { root: dir.path().to_owned() }, args).await.unwrap();
    assert!(!r.is_error);
    text(&r)
}

#[tokio::test]
async fn grep_respects_gitignore_hidden_and_skips_binary() {
    let dir = ws(&[
        (".gitignore", b"ignored.txt\nbuild/\n"),
        ("ignored.txt", b"needle\n"),
        ("build/out.txt", b"needle\n"),
        ("src/a.rs", b"let needle = 1;\n"),
        (".hidden/h.txt", b"needle\n"),
        (".env", b"needle\n"),
        (".git/config", b"needle\n"),
        ("bin.dat", b"needle\0\x01\x02 needle\n"),
    ]);
    assert_eq!(grep(&dir, json!({"pattern": "needle"})).await, "src/a.rs:1:let needle = 1;");
    assert_eq!(grep(&dir, json!({"pattern": "needle", "hidden": true})).await, ".env:1:needle\n.hidden/h.txt:1:needle\nsrc/a.rs:1:let needle = 1;");
    assert_eq!(grep(&dir, json!({"pattern": "needle", "noIgnore": true})).await, "build/out.txt:1:needle\nignored.txt:1:needle\nsrc/a.rs:1:let needle = 1;");
    // 二进制文件整体跳过；没有匹配不是错误
    assert_eq!(grep(&dir, json!({"pattern": "needle", "path": "bin.dat"})).await, "No matches found (1 binary file(s) skipped)");
    assert_eq!(grep(&dir, json!({"pattern": "nothing-here"})).await, "No matches found");
    // 单个文件路径：输出仍是相对工作区根的路径
    assert_eq!(grep(&dir, json!({"pattern": "needle", "path": "src/a.rs"})).await, "src/a.rs:1:let needle = 1;");
}

#[tokio::test]
async fn grep_glob_and_type_filters() {
    let dir = ws(&[("a.rs", b"hit\n"), ("b.py", b"hit\n"), ("sub/c.rs", b"hit\n"), ("sub/d.min.js", b"hit\n")]);
    assert_eq!(grep(&dir, json!({"pattern": "hit", "glob": "*.rs"})).await, "a.rs:1:hit\nsub/c.rs:1:hit");
    assert_eq!(grep(&dir, json!({"pattern": "hit", "glob": ["*.rs", "!sub/**"]})).await, "a.rs:1:hit");
    assert_eq!(grep(&dir, json!({"pattern": "hit", "glob": "!*.min.js"})).await, "a.rs:1:hit\nb.py:1:hit\nsub/c.rs:1:hit");
    assert_eq!(grep(&dir, json!({"pattern": "hit", "type": "py"})).await, "b.py:1:hit");
    assert_eq!(grep(&dir, json!({"pattern": "hit", "type": ["rust", "js"], "path": "sub"})).await, "sub/c.rs:1:hit\nsub/d.min.js:1:hit");
    let e = grep_err(&dir, json!({"pattern": "hit", "type": "nope"})).await;
    assert!(e.contains("unknown file type 'nope'"), "{e}");
    let e = grep_err(&dir, json!({"pattern": "hit", "glob": "a[.rs"})).await;
    assert!(e.contains("invalid glob"), "{e}");
}

#[tokio::test]
async fn grep_case_word_and_fixed_strings() {
    let dir = ws(&[("f.txt", b"Hello\nhello\nHELLO\nhelloworld\n"), ("g.txt", b"abc\na.c\n")]);
    let count = |extra: Value| {
        let mut args = json!({"path": "f.txt", "outputMode": "count"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let dir = &dir;
        async move { grep(dir, args).await }
    };
    assert_eq!(count(json!({"pattern": "hello"})).await, "f.txt:4", "smart case: lowercase => insensitive");
    assert_eq!(count(json!({"pattern": "Hello"})).await, "f.txt:1", "smart case: uppercase => sensitive");
    assert_eq!(count(json!({"pattern": "hello", "smartCase": false})).await, "f.txt:2");
    assert_eq!(count(json!({"pattern": "Hello", "ignoreCase": true})).await, "f.txt:4");
    assert_eq!(count(json!({"pattern": "hello", "ignoreCase": false})).await, "f.txt:2");
    assert_eq!(count(json!({"pattern": "hello", "wordRegexp": true})).await, "f.txt:3");
    assert_eq!(grep(&dir, json!({"pattern": "a.c", "path": "g.txt"})).await, "g.txt:1:abc\ng.txt:2:a.c");
    assert_eq!(grep(&dir, json!({"pattern": "a.c", "path": "g.txt", "fixedStrings": true})).await, "g.txt:2:a.c");
    let e = grep_err(&dir, json!({"pattern": "foo("})).await;
    assert!(e.starts_with("invalid regex:"), "{e}");
}

#[tokio::test]
async fn grep_multiline() {
    let dir = ws(&[("m.rs", b"fn main() {\n    body\n}\n")]);
    let e = grep_err(&dir, json!({"pattern": "main\\(\\) \\{\\n\\s+body"})).await;
    assert!(e.contains("invalid regex") && e.contains("multiline=true"), "{e}");
    assert_eq!(grep(&dir, json!({"pattern": "main\\(\\) \\{\\n\\s+body", "multiline": true})).await, "m.rs:1:fn main() {\nm.rs:2:    body");
    // `.` 默认不跨行，(?s) 打开
    assert_eq!(grep(&dir, json!({"pattern": "main.*body", "multiline": true})).await, "No matches found");
    assert_eq!(grep(&dir, json!({"pattern": "(?s)main.*body", "multiline": true})).await, "m.rs:1:fn main() {\nm.rs:2:    body");
}

#[tokio::test]
async fn grep_context_separators() {
    let lines: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let dir = ws(&[("c.txt", lines.as_bytes()), ("d.txt", b"line 3\n")]);
    let out = grep(&dir, json!({"pattern": "^line (3|4|15)$", "context": 1})).await;
    assert_eq!(out, "c.txt-2-line 2\nc.txt:3:line 3\nc.txt:4:line 4\nc.txt-5-line 5\n--\nc.txt-14-line 14\nc.txt:15:line 15\nc.txt-16-line 16\n--\nd.txt:1:line 3");
    let out = grep(&dir, json!({"pattern": "^line 15$", "before": 2, "after": 0, "path": "c.txt"})).await;
    assert_eq!(out, "c.txt-13-line 13\nc.txt-14-line 14\nc.txt:15:line 15");
}

#[tokio::test]
async fn grep_max_count_limit_and_output_modes() {
    let dir = ws(&[("many.txt", "x\n".repeat(10).as_bytes()), ("other.txt", "x\n".repeat(5).as_bytes())]);
    let out = grep(&dir, json!({"pattern": "x", "maxCount": 2})).await;
    assert!(out.starts_with("many.txt:1:x\nmany.txt:2:x\nother.txt:1:x\nother.txt:2:x\n[maxCount 2: stopped early in 2 file(s)"), "{out}");
    let out = grep(&dir, json!({"pattern": "x", "limit": 3})).await;
    assert_eq!(out, "many.txt:1:x\nmany.txt:2:x\nmany.txt:3:x\n[truncated (limit 3): showing 3 of 15 matching lines in 2 file(s); refine pattern/path/glob/type, use outputMode=files_with_matches/count or raise limit (max 1000)]");
    assert_eq!(grep(&dir, json!({"pattern": "x", "outputMode": "files_with_matches"})).await, "many.txt\nother.txt");
    assert_eq!(grep(&dir, json!({"pattern": "x", "outputMode": "count"})).await, "many.txt:10\nother.txt:5");
    let out = grep(&dir, json!({"pattern": "x", "outputMode": "files_with_matches", "limit": 1})).await;
    assert!(out.starts_with("many.txt\n[truncated (limit 1): showing 1 of 2 files"), "{out}");
    let e = grep_err(&dir, json!({"pattern": "x", "outputMode": "lines"})).await;
    assert!(e.contains("invalid outputMode"), "{e}");
    let r = run(&GrepTool { root: dir.path().to_owned() }, json!({"pattern": "x", "limit": 3})).await.unwrap();
    assert_eq!((r.details["matches"].as_u64(), r.details["truncated"].as_bool(), r.details["filesSearched"].as_u64()), (Some(15), Some(true), Some(2)));
}

#[tokio::test]
async fn grep_is_deterministic_and_bounded() {
    let files: Vec<(String, Vec<u8>)> = (0..60).map(|i| (format!("d{}/f{i:02}.txt", i % 7), b"hit\nhit\n".to_vec())).collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, c)| (p.as_str(), c.as_slice())).collect();
    let dir = ws(&refs);
    let first = grep(&dir, json!({"pattern": "hit", "limit": 9})).await;
    for _ in 0..5 {
        assert_eq!(grep(&dir, json!({"pattern": "hit", "limit": 9})).await, first);
    }
    let mut expected: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
    expected.sort();
    let shown: Vec<&str> = first.lines().filter(|l| !l.starts_with('[')).collect();
    let want: Vec<String> = expected.iter().flat_map(|p| [format!("{p}:1:hit"), format!("{p}:2:hit")]).take(9).collect();
    assert_eq!(shown, want);
    assert!(first.contains("showing 9 of 120 matching lines in 60 file(s)"), "{first}");
    // 超长行截断
    let long = format!("needle{}\n", "a".repeat(2000));
    let dir = ws(&[("long.txt", long.as_bytes())]);
    let out = grep(&dir, json!({"pattern": "needle"})).await;
    assert!(out.ends_with("… [line truncated: 2006 chars]") && out.len() < 600, "{out}");
}

#[tokio::test]
async fn grep_path_must_stay_in_workspace() {
    let dir = ws(&[("a.txt", b"x\n")]);
    for path in ["../", "/etc", "a/../../x"] {
        let e = grep_err(&dir, json!({"pattern": "x", "path": path})).await;
        assert!(e.contains("must be relative to the workspace root"), "{path}: {e}");
    }
    assert!(grep_err(&dir, json!({"pattern": "x", "path": "missing"})).await.contains("path not found: missing"));
    assert!(grep_err(&dir, json!({})).await.contains("missing string argument 'pattern'"));
    assert!(grep_err(&dir, json!({"pattern": "x", "hidden": "yes"})).await.contains("'hidden' must be a boolean"));
}

#[tokio::test]
async fn grep_and_find_respect_cancellation() {
    let dir = ws(&[("a.txt", b"x\n")]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let e = GrepTool { root: dir.path().to_owned() }.execute("1", json!({"pattern": "x"}), cancel.clone(), Arc::new(|_| {})).await.unwrap_err();
    assert_eq!(e.to_string(), "grep aborted");
    let e = FindTool { root: dir.path().to_owned() }.execute("1", json!({"pattern": "*"}), cancel, Arc::new(|_| {})).await.unwrap_err();
    assert_eq!(e.to_string(), "find aborted");
}

#[tokio::test]
async fn find_glob_type_depth_limit_and_dirs() {
    let dir = ws(&[
        (".gitignore", b"ignored.rs\ntarget/\n"),
        ("src/a.rs", b""),
        ("src/mod/mod.rs", b""),
        ("src/lib/mod.rs", b""),
        ("README.md", b""),
        (".hidden.rs", b""),
        ("ignored.rs", b""),
        ("target/x.rs", b""),
    ]);
    assert_eq!(find(&dir, json!({"pattern": "*.rs"})).await, "src/a.rs\nsrc/lib/mod.rs\nsrc/mod/mod.rs");
    assert_eq!(find(&dir, json!({"pattern": "src/**/mod.rs"})).await, "src/lib/mod.rs\nsrc/mod/mod.rs");
    assert_eq!(find(&dir, json!({"pattern": "src/*.rs"})).await, "src/a.rs", "* does not cross /");
    assert_eq!(find(&dir, json!({"pattern": "*", "type": "dir"})).await, "src/\nsrc/lib/\nsrc/mod/");
    assert_eq!(find(&dir, json!({"pattern": "*", "type": "any", "maxDepth": 1})).await, "README.md\nsrc/");
    assert_eq!(find(&dir, json!({"pattern": "*.rs", "hidden": true})).await, ".hidden.rs\nsrc/a.rs\nsrc/lib/mod.rs\nsrc/mod/mod.rs");
    assert_eq!(find(&dir, json!({"pattern": "*.rs", "noIgnore": true})).await, "ignored.rs\nsrc/a.rs\nsrc/lib/mod.rs\nsrc/mod/mod.rs\ntarget/x.rs");
    // path 子目录：pattern 相对 path 匹配，输出仍相对工作区根
    assert_eq!(find(&dir, json!({"pattern": "*/mod.rs", "path": "src"})).await, "src/lib/mod.rs\nsrc/mod/mod.rs");
    assert_eq!(find(&dir, json!({"pattern": "*.rs", "limit": 1})).await, "src/a.rs\n[truncated: showing 1 of 3 paths; narrow pattern/path/maxDepth or raise limit (max 5000)]");
    assert_eq!(find(&dir, json!({"pattern": "*.nothing"})).await, "No files found");
    let tool = FindTool { root: dir.path().to_owned() };
    assert!(run(&tool, json!({"pattern": "*", "path": "../"})).await.unwrap_err().to_string().contains("must be relative to the workspace root"));
    assert!(run(&tool, json!({"pattern": "*", "type": "socket"})).await.unwrap_err().to_string().contains("invalid type 'socket'"));
    assert!(run(&tool, json!({"pattern": "a[", "type": "any"})).await.unwrap_err().to_string().contains("invalid glob"));
}
