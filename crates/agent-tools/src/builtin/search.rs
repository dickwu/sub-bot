//! 只读搜索工具 `grep` / `find`（对应 pi `coding-agent/src/core/tools/{grep,find}.ts`）。
//!
//! 在进程内嵌入 ripgrep 引擎：直接使用 ripgrep（BurntSushi/ripgrep）的库 crate——
//! `grep-regex` / `grep-matcher`（正则匹配）、`grep-searcher`（按行搜索、上下文、二进制检测）、
//! `ignore`（并行、遵守 .gitignore / .ignore / 全局 gitignore 的目录遍历，glob 覆盖与文件类型）、
//! `globset`（find 的文件名 glob）。不依赖外部 `rg` 可执行文件。许可证见 THIRD-PARTY-NOTICES.md。
//!
//! 结果先按文件收集再按路径排序，输出稳定；路径相对于会话 cwd（可直接传给 read / edit）。
//! 搜索路径与其它 fs 工具一样经 [`resolve_in_root`] 限制在会话 cwd 内。
use super::fs::resolve_in_root;
use agent_core::{AgentTool, ToolResult, UpdateSink};
use anyhow::{anyhow, bail, Context, Result};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::{overrides::OverrideBuilder, types::TypesBuilder, WalkBuilder, WalkState};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// 单次结果的总字节上限（与 bash/read 相同量级）。
const MAX_OUTPUT: usize = 50_000;
/// 每行输出最多字符数，超出部分截掉并标注。
const MAX_LINE_CHARS: usize = 500;
const MAX_CONTEXT: u64 = 20;
const GREP_DEFAULT_LIMIT: u64 = 100;
const GREP_MAX_LIMIT: u64 = 1000;
const FIND_DEFAULT_LIMIT: u64 = 200;
const FIND_MAX_LIMIT: u64 = 5000;
/// 读取失败（权限等）只汇总，最多附带几条示例。
const MAX_PROBLEM_SAMPLES: usize = 3;

// ---------- 参数解析 ----------

fn opt_str<'a>(args: &'a Value, k: &str) -> Result<Option<&'a str>> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => bail!("'{k}' must be a string"),
    }
}

fn opt_bool(args: &Value, k: &str) -> Result<Option<bool>> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => bail!("'{k}' must be a boolean"),
    }
}

fn opt_u64(args: &Value, k: &str) -> Result<Option<u64>> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).with_context(|| format!("'{k}' must be a non-negative integer")),
    }
}

/// 字符串或字符串数组（如 `glob: "*.rs"` / `glob: ["*.rs", "!target/**"]`）。
fn str_list(args: &Value, k: &str) -> Result<Vec<String>> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).with_context(|| format!("'{k}' must be a string or an array of strings"))).collect(),
        Some(_) => bail!("'{k}' must be a string or an array of strings"),
    }
}

/// `limit` 默认值 / 上限；0 视为参数错误。
fn limit_arg(args: &Value, default: u64, cap: u64) -> Result<usize> {
    let limit = opt_u64(args, "limit")?.unwrap_or(default);
    if limit == 0 {
        bail!("'limit' must be >= 1");
    }
    Ok(limit.min(cap) as usize)
}

/// 解析 `path`（默认 cwd），与其它 fs 工具共用 [`resolve_in_root`]，并要求路径存在。
fn search_root(root: &Path, args: &Value) -> Result<(PathBuf, String)> {
    let arg = opt_str(args, "path")?.unwrap_or(".");
    // 规范化掉 `.` 分量（`root/./src` → `root/src`）：ignore 的 glob 覆盖按字节前缀剥离 root，不会跳过 `.`。
    let path = normalized(&resolve_in_root(root, arg)?);
    if !path.exists() {
        bail!("path not found: {arg}");
    }
    Ok((path, arg.to_string()))
}

// ---------- 公共遍历 ----------

#[derive(Debug, Clone, Copy, Default)]
struct WalkOpts {
    hidden: bool,
    no_ignore: bool,
    max_depth: Option<usize>,
}

impl WalkOpts {
    fn parse(args: &Value) -> Result<Self> {
        Ok(Self { hidden: opt_bool(args, "hidden")?.unwrap_or(false), no_ignore: opt_bool(args, "noIgnore")?.unwrap_or(false), max_depth: None })
    }

    /// 与 `rg` 默认一致：遵守 .gitignore / .ignore / .rgignore / 全局 gitignore / .git/info/exclude（含父目录），
    /// 跳过隐藏文件，不跟随符号链接。额外：即使 `hidden=true` 也不进入 `.git`（相当于 `rg --hidden -g '!.git'`）。
    fn builder(&self, search: &Path) -> WalkBuilder {
        let mut b = WalkBuilder::new(search);
        b.standard_filters(!self.no_ignore).hidden(!self.hidden).follow_links(false).max_depth(self.max_depth);
        b.filter_entry(|e| e.depth() == 0 || e.file_name() != ".git");
        b
    }
}

/// 遍历/读取失败：不致命，汇总后附在输出末尾并 warn 一次。
#[derive(Default)]
struct Problems {
    count: u64,
    samples: Vec<String>,
}

impl Problems {
    fn add(&mut self, msg: String) {
        self.count += 1;
        if self.samples.len() < MAX_PROBLEM_SAMPLES {
            self.samples.push(msg);
        }
    }

    fn note(&self) -> Option<String> {
        (self.count > 0).then(|| format!("[{} path(s) could not be read and were skipped, e.g. {}]", self.count, self.samples.join("; ")))
    }

    fn log(&self, tool: &str) {
        if self.count > 0 {
            // 已在工具输出里告知模型；工作区里的权限问题不是服务故障，不再作为 log_warn issue 上报。
            tracing::warn!(issue = "typed", tool, errors = self.count, sample = %self.samples.first().map(String::as_str).unwrap_or(""), "search skipped unreadable paths");
        }
    }
}

/// 错误信息里去掉工作区绝对路径前缀，只给模型看相对路径。
fn scrub(root: &Path, msg: String) -> String {
    msg.replace(&format!("{}/", root.display()), "")
}

/// 去掉 `.` 分量与多余分隔符，使 root / 搜索路径 / 遍历结果的前缀按字节一致。
fn normalized(p: &Path) -> PathBuf {
    p.components().collect()
}

fn rel(root: &Path, p: &Path) -> PathBuf {
    p.strip_prefix(root).map(Path::to_path_buf).unwrap_or_else(|_| p.to_path_buf())
}

fn display(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.is_empty() {
        ".".into()
    } else {
        s.into_owned()
    }
}

/// 解码一行（有损 UTF-8），去掉行尾换行，超长截断并标注。
fn clip(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim_end_matches(['\n', '\r']);
    if s.len() <= MAX_LINE_CHARS {
        return s.to_string();
    }
    let n = s.chars().count();
    if n <= MAX_LINE_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}… [line truncated: {n} chars]")
}

/// 按路径排序、只保留排序后前 `limit` 个单位（匹配行或文件）所需的条目：
/// 并行遍历的到达顺序不确定，但这样得到的结果与"全部收集后排序再截断"相同，且内存有界。
struct Ranked<T> {
    items: BTreeMap<PathBuf, (usize, T)>,
    units: usize,
    limit: usize,
}

impl<T> Ranked<T> {
    fn new(limit: usize) -> Self {
        Self { items: BTreeMap::new(), units: 0, limit }
    }

    fn insert(&mut self, path: PathBuf, units: usize, item: T) {
        if self.units >= self.limit && self.items.last_key_value().is_some_and(|(last, _)| &path > last) {
            return;
        }
        self.units += units;
        self.items.insert(path, (units, item));
        while let Some((_, (u, _))) = self.items.last_key_value() {
            if self.units - u < self.limit {
                break;
            }
            let u = *u;
            self.items.pop_last();
            self.units -= u;
        }
    }
}

/// 在阻塞线程池上运行搜索；future 被丢弃或会话中止时通过子 token 让遍历尽快退出。
async fn run_blocking<T: Send + 'static>(cancel: &CancellationToken, f: impl FnOnce(&CancellationToken) -> T + Send + 'static) -> Result<T> {
    let token = cancel.child_token();
    let _guard = token.clone().drop_guard();
    let span = tracing::Span::current();
    let out = tokio::task::spawn_blocking(move || span.in_scope(|| f(&token))).await.context("search task failed")?;
    Ok(out)
}

// ---------- grep ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Content,
    FilesWithMatches,
    Count,
}

struct GrepOpts {
    matcher: RegexMatcher,
    search: PathBuf,
    path_arg: String,
    globs: Vec<String>,
    types: Vec<String>,
    overrides: Option<ignore::overrides::Override>,
    file_types: Option<ignore::types::Types>,
    walk: WalkOpts,
    ignore_case: Option<bool>,
    smart_case: bool,
    fixed_strings: bool,
    word: bool,
    multiline: bool,
    before: usize,
    after: usize,
    max_count: Option<u64>,
    limit: usize,
    mode: Mode,
}

impl GrepOpts {
    fn parse(root: &Path, args: &Value) -> Result<Self> {
        let pattern = opt_str(args, "pattern")?.context("missing string argument 'pattern'")?;
        let (search, path_arg) = search_root(root, args)?;
        let ignore_case = opt_bool(args, "ignoreCase")?;
        let smart_case = opt_bool(args, "smartCase")?.unwrap_or(true);
        let fixed_strings = opt_bool(args, "fixedStrings")?.unwrap_or(false);
        let word = opt_bool(args, "wordRegexp")?.unwrap_or(false);
        let multiline = opt_bool(args, "multiline")?.unwrap_or(false);
        let mode = match opt_str(args, "outputMode")?.unwrap_or("content") {
            "content" => Mode::Content,
            "files_with_matches" => Mode::FilesWithMatches,
            "count" => Mode::Count,
            other => bail!("invalid outputMode '{other}' (expected content, files_with_matches or count)"),
        };
        let context = opt_u64(args, "context")?.unwrap_or(0);
        let (before, after) = match mode {
            Mode::Content => (opt_u64(args, "before")?.unwrap_or(context).min(MAX_CONTEXT) as usize, opt_u64(args, "after")?.unwrap_or(context).min(MAX_CONTEXT) as usize),
            _ => (0, 0),
        };
        let max_count = opt_u64(args, "maxCount")?;
        if max_count == Some(0) {
            bail!("'maxCount' must be >= 1");
        }
        let limit = limit_arg(args, GREP_DEFAULT_LIMIT, GREP_MAX_LIMIT)?;

        // 与 rg 相同：^/$ 按行匹配；非 multiline 时正则不能跨行（禁止匹配 \n）；`.` 默认不匹配换行（可用 (?s)）。
        let mut b = RegexMatcherBuilder::new();
        b.multi_line(true).fixed_strings(fixed_strings).word(word);
        match ignore_case {
            Some(yes) => b.case_insensitive(yes),
            None => b.case_smart(smart_case),
        };
        if !multiline {
            b.line_terminator(Some(b'\n'));
        }
        let matcher = b.build(pattern).map_err(|e| {
            let hint = if !multiline && e.to_string().contains("\\n") { " (set multiline=true to match across lines)" } else { "" };
            anyhow!("invalid regex: {e}{hint}")
        })?;

        let globs = str_list(args, "glob")?;
        let overrides = if globs.is_empty() {
            None
        } else {
            // 与 rg -g 相同：相对 cwd 匹配；不含 `/` 的 glob 匹配任意层级的文件名；`!` 前缀表示排除。
            let mut ob = OverrideBuilder::new(root);
            for g in &globs {
                ob.add(g).map_err(|e| anyhow!("invalid glob '{g}': {e}"))?;
            }
            Some(ob.build().map_err(|e| anyhow!("invalid glob: {e}"))?)
        };
        let types = str_list(args, "type")?;
        let file_types = if types.is_empty() {
            None
        } else {
            let mut tb = TypesBuilder::new();
            tb.add_defaults();
            for t in &types {
                tb.select(t);
            }
            Some(tb.build().map_err(|e| match e {
                ignore::Error::UnrecognizedFileType(name) => anyhow!("unknown file type '{name}' (use ripgrep type names, e.g. rust, py, js, ts, go, java, c, cpp, md, json, yaml, toml, sh)"),
                e => anyhow!("invalid file type: {e}"),
            })?)
        };
        Ok(Self {
            matcher,
            search,
            path_arg,
            globs,
            types,
            overrides,
            file_types,
            walk: WalkOpts::parse(args)?,
            ignore_case,
            smart_case,
            fixed_strings,
            word,
            multiline,
            before,
            after,
            max_count,
            limit,
            mode,
        })
    }

    fn searcher(&self) -> Searcher {
        SearcherBuilder::new()
            .line_number(true)
            .multi_line(self.multiline)
            .before_context(self.before)
            .after_context(self.after)
            // rg 默认：遇到 NUL 视为二进制文件并停止（整文件跳过）。
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .max_matches(if self.mode == Mode::FilesWithMatches { Some(1) } else { self.max_count })
            .build()
    }
}

enum Line {
    Match(u64, String),
    Context(u64, String),
    Break,
}

#[derive(Default)]
struct FileHits {
    lines: Vec<Line>,
    /// 已保存的匹配行数（content 模式，最多 limit）
    stored: usize,
    /// 匹配行总数
    matched_lines: u64,
    /// 匹配次数（multiline 下一次匹配可跨多行；用于判断是否触达 maxCount）
    events: u64,
    /// 已有匹配行未保存：之后的上下文也不再保存
    skipped: bool,
    binary: bool,
}

struct GrepSink<'a> {
    hits: FileHits,
    mode: Mode,
    limit: usize,
    cancel: &'a CancellationToken,
}

impl Sink for GrepSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        self.hits.events += 1;
        let first = m.line_number().unwrap_or(0);
        for (i, line) in m.lines().enumerate() {
            self.hits.matched_lines += 1;
            if self.mode != Mode::Content {
                continue;
            }
            if self.hits.skipped || self.hits.stored >= self.limit {
                self.hits.skipped = true;
            } else {
                self.hits.lines.push(Line::Match(first + i as u64, clip(line)));
                self.hits.stored += 1;
            }
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        if !self.hits.skipped {
            self.hits.lines.push(Line::Context(c.line_number().unwrap_or(0), clip(c.bytes())));
        }
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, std::io::Error> {
        if !self.hits.skipped && !self.hits.lines.is_empty() {
            self.hits.lines.push(Line::Break);
        }
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, std::io::Error> {
        self.hits.binary = true;
        Ok(false)
    }
}

#[derive(Default)]
struct GrepStats {
    files_searched: AtomicU64,
    files_matched: AtomicU64,
    matched_lines: AtomicU64,
    max_count_hit: AtomicU64,
    binary_skipped: AtomicU64,
}

fn run_grep(root: &Path, o: &GrepOpts, cancel: &CancellationToken) -> Result<(String, Value)> {
    let started = Instant::now();
    let mut walk = o.walk.builder(&o.search);
    if let Some(ov) = &o.overrides {
        walk.overrides(ov.clone());
    }
    if let Some(t) = &o.file_types {
        walk.types(t.clone());
    }
    let stats = GrepStats::default();
    let ranked = Mutex::new(Ranked::<FileHits>::new(o.limit));
    let problems = Mutex::new(Problems::default());
    walk.build_parallel().run(|| {
        let mut searcher = o.searcher();
        let (stats, ranked, problems) = (&stats, &ranked, &problems);
        Box::new(move |entry| {
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    problems.lock().unwrap().add(scrub(root, e.to_string()));
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            stats.files_searched.fetch_add(1, Relaxed);
            let path = rel(root, entry.path());
            let mut sink = GrepSink { hits: FileHits::default(), mode: o.mode, limit: o.limit, cancel };
            if let Err(e) = searcher.search_path(&o.matcher, entry.path(), &mut sink) {
                problems.lock().unwrap().add(format!("{}: {}", display(&path), scrub(root, e.to_string())));
                return WalkState::Continue;
            }
            let hits = sink.hits;
            if hits.binary {
                stats.binary_skipped.fetch_add(1, Relaxed);
                return WalkState::Continue;
            }
            if hits.matched_lines == 0 {
                return WalkState::Continue;
            }
            stats.files_matched.fetch_add(1, Relaxed);
            stats.matched_lines.fetch_add(hits.matched_lines, Relaxed);
            if o.mode != Mode::FilesWithMatches && o.max_count.is_some_and(|m| hits.events >= m) {
                stats.max_count_hit.fetch_add(1, Relaxed);
            }
            let units = if o.mode == Mode::Content { hits.stored } else { 1 };
            ranked.lock().unwrap().insert(path, units, hits);
            WalkState::Continue
        })
    });
    if cancel.is_cancelled() {
        bail!("grep aborted");
    }
    let problems = problems.into_inner().unwrap();
    problems.log("grep");
    let ranked = ranked.into_inner().unwrap();
    let total_lines = stats.matched_lines.load(Relaxed);
    let files_matched = stats.files_matched.load(Relaxed);
    let binary = stats.binary_skipped.load(Relaxed);

    let mut out = String::new();
    let mut shown = 0u64;
    let mut byte_capped = false;
    let context = o.before > 0 || o.after > 0;
    let mut push = |out: &mut String, line: String| -> bool {
        if out.len() + line.len() + 1 > MAX_OUTPUT {
            byte_capped = true;
            return false;
        }
        out.push_str(&line);
        out.push('\n');
        true
    };
    'files: for (i, (path, (_, hits))) in ranked.items.iter().enumerate() {
        let p = display(path);
        match o.mode {
            Mode::FilesWithMatches | Mode::Count => {
                if shown as usize >= o.limit {
                    break;
                }
                let line = if o.mode == Mode::Count { format!("{p}:{}", hits.matched_lines) } else { p };
                if !push(&mut out, line) {
                    break;
                }
                shown += 1;
            }
            Mode::Content => {
                if shown as usize >= o.limit {
                    break;
                }
                // rg：开启上下文时，不连续的行组之间（包括跨文件）用 `--` 分隔。
                if context && i > 0 && !push(&mut out, "--".into()) {
                    break;
                }
                for line in &hits.lines {
                    let full = shown as usize >= o.limit;
                    let ok = match line {
                        Line::Match(..) | Line::Break if full => break,
                        Line::Match(n, t) => {
                            let ok = push(&mut out, format!("{p}:{n}:{t}"));
                            shown += ok as u64;
                            ok
                        }
                        Line::Context(n, t) => push(&mut out, format!("{p}-{n}-{t}")),
                        Line::Break => push(&mut out, "--".into()),
                    };
                    if !ok {
                        break 'files;
                    }
                }
            }
        }
    }

    let (total, unit) = match o.mode {
        Mode::Content => (total_lines, "matching lines"),
        _ => (files_matched, "files"),
    };
    let truncated = shown < total;
    let mut notes = vec![];
    if total == 0 {
        // 只有显式搜索单个文件时才提示二进制跳过；目录遍历时与 rg 一样静默跳过。
        out = if binary > 0 && o.search.is_file() { format!("No matches found ({binary} binary file(s) skipped)") } else { "No matches found".into() };
    } else if truncated {
        let why = if byte_capped { format!("output exceeded {MAX_OUTPUT} bytes") } else { format!("limit {}", o.limit) };
        notes.push(format!(
            "[truncated ({why}): showing {shown} of {total} {unit} in {files_matched} file(s); refine pattern/path/glob/type{} or raise limit (max {GREP_MAX_LIMIT})]",
            if o.mode == Mode::Content { ", use outputMode=files_with_matches/count" } else { "" }
        ));
    }
    let max_hit = stats.max_count_hit.load(Relaxed);
    if max_hit > 0 {
        notes.push(format!("[maxCount {}: stopped early in {max_hit} file(s); per-file results are partial]", o.max_count.unwrap_or(0)));
    }
    notes.extend(problems.note());
    let mut text = out.trim_end_matches('\n').to_string();
    for n in notes {
        text.push('\n');
        text.push_str(&n);
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let files_searched = stats.files_searched.load(Relaxed);
    tracing::debug!(files_searched, files_matched, matches = total_lines, shown, binary_skipped = binary, errors = problems.count, elapsed_ms, truncated, "grep finished");
    let details = json!({
        "matches": total_lines, "filesMatched": files_matched, "filesSearched": files_searched, "shown": shown,
        "truncated": truncated, "binarySkipped": binary, "errors": problems.count, "elapsedMs": elapsed_ms,
    });
    Ok((text, details))
}

pub struct GrepTool {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex (embedded ripgrep engine; Rust regex syntax). Respects .gitignore, skips hidden and binary files. \
         Output: `path:line:text` (context lines `path-line-text`, groups separated by `--`), paths relative to the workspace root, sorted. \
         Prefer this over running rg/grep via bash (they may not be installed). Smart case by default."
    }
    fn parameters(&self) -> Value {
        let str_or_list = || json!([{"type": "string"}, {"type": "array", "items": {"type": "string"}}]);
        json!({"type": "object", "properties": {
            "pattern": {"type": "string", "description": "Regex (Rust regex syntax); literal if fixedStrings"},
            "path": {"type": "string", "description": "File or directory relative to the workspace root (default .)"},
            "glob": {"description": "Include glob(s) like rg -g, e.g. \"*.rs\" or [\"src/**\", \"!*.min.js\"]; prefix ! to exclude", "anyOf": str_or_list()},
            "type": {"description": "ripgrep file type(s), e.g. \"rust\", \"py\", \"ts\"", "anyOf": str_or_list()},
            "ignoreCase": {"type": "boolean", "description": "Force case-insensitive (true) or case-sensitive (false); overrides smartCase"},
            "smartCase": {"type": "boolean", "description": "Case-insensitive if the pattern is all lowercase (default true)"},
            "fixedStrings": {"type": "boolean", "description": "Treat pattern as a literal string"},
            "wordRegexp": {"type": "boolean", "description": "Only match whole words"},
            "multiline": {"type": "boolean", "description": "Allow matches to span lines (\\n in pattern; use (?s) for dot to match newline)"},
            "context": {"type": "integer", "description": "Context lines before and after each match (default 0, max 20)"},
            "before": {"type": "integer", "description": "Context lines before (overrides context)"},
            "after": {"type": "integer", "description": "Context lines after (overrides context)"},
            "hidden": {"type": "boolean", "description": "Search hidden files and directories (default false)"},
            "noIgnore": {"type": "boolean", "description": "Do not respect .gitignore/.ignore files (default false)"},
            "maxCount": {"type": "integer", "description": "Max matches per file"},
            "limit": {"type": "integer", "description": "Max matching lines (or files in files_with_matches/count mode) returned (default 100, max 1000)"},
            "outputMode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "default content"}
        }, "required": ["pattern"]})
    }
    async fn execute(&self, _id: &str, args: Value, cancel: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let root = normalized(&self.root);
        let o = GrepOpts::parse(&root, &args)?;
        let span = tracing::debug_span!(
            "grep",
            // 只记录正则长度不记原文（与日志约定一致：工具参数可能含敏感文本）
            pattern_chars = opt_str(&args, "pattern")?.map_or(0, |p| p.chars().count()),
            root = %o.path_arg,
            mode = ?o.mode,
            globs = ?o.globs,
            types = ?o.types,
            ignore_case = ?o.ignore_case,
            smart_case = o.smart_case,
            fixed_strings = o.fixed_strings,
            word = o.word,
            multiline = o.multiline,
            before = o.before,
            after = o.after,
            hidden = o.walk.hidden,
            no_ignore = o.walk.no_ignore,
            max_count = ?o.max_count,
            limit = o.limit,
        );
        use tracing::Instrument;
        let (text, details) = run_blocking(&cancel, move |token| run_grep(&root, &o, token)).instrument(span).await??;
        let mut r = ToolResult::text(text);
        r.details = details;
        Ok(r)
    }
}

// ---------- find ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Any,
}

struct FindOpts {
    glob: globset::GlobMatcher,
    /// glob 含 `/` 时匹配相对搜索目录的路径，否则只匹配文件名（同 fd --glob）
    match_path: bool,
    search: PathBuf,
    path_arg: String,
    pattern: String,
    kind: Kind,
    walk: WalkOpts,
    limit: usize,
}

impl FindOpts {
    fn parse(root: &Path, args: &Value) -> Result<Self> {
        let pattern = opt_str(args, "pattern")?.context("missing string argument 'pattern'")?;
        let (search, path_arg) = search_root(root, args)?;
        let pat = pattern.strip_prefix("./").unwrap_or(pattern);
        let glob = globset::GlobBuilder::new(pat).literal_separator(true).build().map_err(|e| anyhow!("invalid glob '{pattern}': {e}"))?.compile_matcher();
        let kind = match opt_str(args, "type")?.unwrap_or("file") {
            "file" | "f" => Kind::File,
            "dir" | "d" | "directory" => Kind::Dir,
            "any" => Kind::Any,
            other => bail!("invalid type '{other}' (expected file, dir or any)"),
        };
        let mut walk = WalkOpts::parse(args)?;
        walk.max_depth = opt_u64(args, "maxDepth")?.map(|d| d as usize);
        Ok(Self { glob, match_path: pat.contains('/'), search, path_arg, pattern: pattern.to_string(), kind, walk, limit: limit_arg(args, FIND_DEFAULT_LIMIT, FIND_MAX_LIMIT)? })
    }
}

fn run_find(root: &Path, o: &FindOpts, cancel: &CancellationToken) -> Result<(String, Value)> {
    let started = Instant::now();
    let total = AtomicU64::new(0);
    let visited = AtomicU64::new(0);
    let ranked = Mutex::new(Ranked::<bool>::new(o.limit));
    let problems = Mutex::new(Problems::default());
    o.walk.builder(&o.search).build_parallel().run(|| {
        let (total, visited, ranked, problems) = (&total, &visited, &ranked, &problems);
        Box::new(move |entry| {
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    problems.lock().unwrap().add(scrub(root, e.to_string()));
                    return WalkState::Continue;
                }
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            visited.fetch_add(1, Relaxed);
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let wanted = match o.kind {
                Kind::File => entry.file_type().is_some_and(|t| t.is_file()),
                Kind::Dir => is_dir,
                Kind::Any => true,
            };
            let hit = wanted && if o.match_path { o.glob.is_match(entry.path().strip_prefix(&o.search).unwrap_or(entry.path())) } else { o.glob.is_match(entry.file_name()) };
            if hit {
                total.fetch_add(1, Relaxed);
                ranked.lock().unwrap().insert(rel(root, entry.path()), 1, is_dir);
            }
            WalkState::Continue
        })
    });
    if cancel.is_cancelled() {
        bail!("find aborted");
    }
    let problems = problems.into_inner().unwrap();
    problems.log("find");
    let total = total.into_inner();
    let mut out = String::new();
    let mut shown = 0u64;
    for (path, (_, is_dir)) in &ranked.into_inner().unwrap().items {
        let line = format!("{}{}", display(path), if *is_dir { "/" } else { "" });
        if out.len() + line.len() + 1 > MAX_OUTPUT {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        shown += 1;
    }
    let truncated = shown < total;
    let mut text = if total == 0 { "No files found".to_string() } else { out.trim_end_matches('\n').to_string() };
    if truncated {
        text.push_str(&format!("\n[truncated: showing {shown} of {total} paths; narrow pattern/path/maxDepth or raise limit (max {FIND_MAX_LIMIT})]"));
    }
    if let Some(n) = problems.note() {
        text.push('\n');
        text.push_str(&n);
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let entries = visited.into_inner();
    tracing::debug!(entries_visited = entries, matches = total, shown, errors = problems.count, elapsed_ms, truncated, "find finished");
    Ok((text, json!({"matches": total, "shown": shown, "truncated": truncated, "entriesVisited": entries, "errors": problems.count, "elapsedMs": elapsed_ms})))
}

pub struct FindTool {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Find files or directories by glob (e.g. \"*.rs\", \"src/**/mod.rs\"), respecting .gitignore and skipping hidden entries like `rg --files`. \
         Returns sorted paths relative to the workspace root, directories with a trailing /. Prefer this over find/fd/rg via bash (they may not be installed)."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {
            "pattern": {"type": "string", "description": "Glob matched against the file name, or against the path relative to `path` if it contains /"},
            "path": {"type": "string", "description": "Directory relative to the workspace root (default .)"},
            "type": {"type": "string", "enum": ["file", "dir", "any"], "description": "default file"},
            "hidden": {"type": "boolean", "description": "Include hidden files and directories (default false)"},
            "noIgnore": {"type": "boolean", "description": "Do not respect .gitignore/.ignore files (default false)"},
            "maxDepth": {"type": "integer", "description": "Max directory depth below path (1 = direct children)"},
            "limit": {"type": "integer", "description": "Max paths returned (default 200, max 5000)"}
        }, "required": ["pattern"]})
    }
    async fn execute(&self, _id: &str, args: Value, cancel: CancellationToken, _u: UpdateSink) -> Result<ToolResult> {
        let root = normalized(&self.root);
        let o = FindOpts::parse(&root, &args)?;
        let span = tracing::debug_span!("find", pattern = %o.pattern, root = %o.path_arg, kind = ?o.kind, hidden = o.walk.hidden, no_ignore = o.walk.no_ignore, max_depth = ?o.walk.max_depth, limit = o.limit);
        use tracing::Instrument;
        let (text, details) = run_blocking(&cancel, move |token| run_find(&root, &o, token)).instrument(span).await??;
        let mut r = ToolResult::text(text);
        r.details = details;
        Ok(r)
    }
}

#[cfg(test)]
mod tests;
