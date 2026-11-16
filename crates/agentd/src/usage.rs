//! 本地用量统计（v0.11.0）：每次 LLM 调用 provider 返回的 `Usage` 追加写入 `<dataDir>/usage/YYYY-MM-DD.jsonl`。
//!
//! - 记录点：会话的每个账号 provider 外面包一层 [`RecordingProvider`]（在 `FailoverProvider` 链内，账号归属准确），
//!   观察 `Done` / `Error` / `Retry` 事件；覆盖 run / ask / rpc / serve 的回合调用、压缩摘要调用和路由分类调用。
//! - 只记录 provider 真实报告过的用量：没有用量（全 0）的调用不写，绝不虚构 0；被丢弃的重试尝试与失败调用只要报告过
//!   用量也会记录（`discarded` / `failed` 标记）。
//! - 文件按本机本地时区的日期切分；目录 0700、文件 0600；O_APPEND + flock 写整行并 `sync_data`。不在 `state/` 的
//!   独占锁里，`run` / `ask` 可与 `serve` 同时写。历史从不压缩改写。
//! - 读取（[`load`]）：缺文件 = 空；最后一行被截断（崩溃）则忽略；损坏的完整行跳过并计数，其余照常统计。
pub mod cli;
pub mod report;
#[cfg(test)]
mod tests;

use crate::accounts::AccountKey;
use agent_ai::{AssistantEvent, EventStream, ModelInfo, Provider, Request, StopReason, Usage};
use chrono::NaiveDate;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio_util::sync::CancellationToken;

/// `<dataDir>` 下的用量目录名。
pub const USAGE_DIR: &str = "usage";
/// 记录格式版本。
pub const RECORD_VERSION: u32 = 1;

/// 一次调用（尝试）的 token 计数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 推理 token（OpenAI 计入 output）
    pub reasoning: u64,
    pub total_tokens: u64,
}

impl From<&Usage> for Tokens {
    fn from(u: &Usage) -> Self {
        Self { input: u.input, output: u.output, cache_read: u.cache_read, cache_write: u.cache_write, reasoning: u.reasoning, total_tokens: u.total_tokens }
    }
}

impl Tokens {
    pub fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

/// JSONL 中的一行。只含名字（client / profile / 账号名），绝不含 token、token 哈希或 key。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    #[serde(default)]
    pub v: u32,
    /// Unix 毫秒
    pub ts: i64,
    /// 写入时的本地时间（RFC 3339，带偏移）
    #[serde(default)]
    pub time: String,
    /// 写入时的本地日期（文件名）
    #[serde(default)]
    pub day: String,
    pub provider: String,
    pub account: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 调用方：client 名（`local` / `stdio` / 配置的名字）
    #[serde(default)]
    pub caller: String,
    /// 同一次调用里第几次尝试（从 1 开始；瞬时错误重试递增）
    #[serde(default = "one")]
    pub attempt: u32,
    /// attempt > 1：这是一次重放
    #[serde(default)]
    pub retry: bool,
    /// 这次尝试被丢弃并重试（输出作废，但用量已计费）
    #[serde(default)]
    pub discarded: bool,
    /// 调用以错误结束（或 stopReason=error）
    #[serde(default)]
    pub failed: bool,
    pub tokens: Tokens,
}

fn one() -> u32 {
    1
}

impl UsageRecord {
    /// `provider/account`：聚合键。
    pub fn account_label(&self) -> String {
        format!("{}/{}", self.provider, self.account)
    }
}

/// 记录的调用归属（会话级）。
#[derive(Debug, Clone, Default)]
pub struct UsageMeta {
    pub caller: String,
    pub profile: Option<String>,
    pub session_id: Option<String>,
}

/// 追加写入端（进程内共享）。每条记录单独打开当天的文件：跨日自然切换，多个进程（serve 与 run）并发也安全。
pub struct Journal {
    dir: PathBuf,
    /// 进程内串行化（跨进程靠 flock）
    write_lock: Mutex<()>,
    prepared: AtomicBool,
    /// 上一次写入失败：只在第一次失败时 warn，恢复时 info，避免磁盘满时刷屏
    failing: AtomicBool,
}

impl Journal {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, write_lock: Mutex::new(()), prepared: AtomicBool::new(false), failing: AtomicBool::new(false) }
    }

    /// `<dataDir>/usage`
    pub fn for_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join(USAGE_DIR))
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 追加一条记录：完整一行 + 换行，O_APPEND，持有 flock 期间写入并 `sync_data`。
    pub fn append(&self, record: &UsageRecord) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.prepared.load(Ordering::Relaxed) {
            // 临时回退 dataDir 同样要求属主/权限安全（与会话目录、state/ 一致）
            crate::config::prepare_temp_data_dir(&self.dir, true)?;
            private_dir(&self.dir)?;
            self.prepared.store(true, Ordering::Relaxed);
        }
        anyhow::ensure!(valid_day(&record.day), "invalid usage record day '{}'", record.day);
        let path = self.dir.join(format!("{}.jsonl", record.day));
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = private_file(&path)?;
        let _lock = FileLock::exclusive(&file)?;
        // 上一个写入者崩溃留下的半行：先补换行，让它成为一行（读取时作为损坏行跳过），不污染这条记录
        if torn_tail(&file)? {
            tracing::warn!(file = %path.display(), "usage journal ended with a torn line; terminating it before appending");
            file.write_all(b"\n")?;
        }
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    /// 追加并记录日志；写入失败不影响 LLM 调用本身。
    pub fn record(&self, record: &UsageRecord) {
        match self.append(record) {
            Ok(()) => {
                if self.failing.swap(false, Ordering::Relaxed) {
                    tracing::info!(dir = %self.dir.display(), "usage journal writable again");
                }
                tracing::debug!(
                    session_id = record.session_id.as_deref().unwrap_or("-"),
                    provider = %record.provider,
                    account = %record.account,
                    model = %record.model,
                    caller = %record.caller,
                    attempt = record.attempt,
                    discarded = record.discarded,
                    failed = record.failed,
                    input_tokens = record.tokens.input,
                    output_tokens = record.tokens.output,
                    cache_read_tokens = record.tokens.cache_read,
                    cache_write_tokens = record.tokens.cache_write,
                    reasoning_tokens = record.tokens.reasoning,
                    total_tokens = record.tokens.total_tokens,
                    "usage recorded"
                );
            }
            Err(error) if !self.failing.swap(true, Ordering::Relaxed) => {
                // 无 issue 字段：作为 log_warn 上报一次（之后的失败只记 debug，恢复时 info）
                tracing::warn!(
                    session_id = record.session_id.as_deref().unwrap_or("-"),
                    dir = %self.dir.display(),
                    provider = %record.provider,
                    account = %record.account,
                    error = %error,
                    "failed to write usage record; usage statistics will be incomplete"
                );
            }
            Err(error) => tracing::debug!(session_id = record.session_id.as_deref().unwrap_or("-"), error = %error, "usage record write failed again"),
        }
    }
}

/// `YYYY-MM-DD`
pub fn valid_day(day: &str) -> bool {
    day.len() == 10 && NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok()
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// 0600，O_APPEND（与 `store.rs` 的 `private_file` 相同，另加读权限以检查结尾的半行）。
fn private_file(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    Ok(options.open(path)?)
}

/// 文件非空且最后一个字节不是换行。
fn torn_tail(file: &File) -> std::io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut last = [0u8];
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut last, len - 1)?;
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = file;
        f.seek(SeekFrom::Start(len - 1))?;
        f.read_exact(&mut last)?;
    }
    Ok(last[0] != b'\n')
}

/// 写入期间的 flock（跨进程：serve 与 run 同时写同一天的文件）。drop 时解锁。
/// 只保存 raw fd，不借用 `File`，这样持锁期间仍可对同一个文件 `write_all`。
struct FileLock {
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

impl FileLock {
    fn exclusive(file: &File) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: `file` stays open until `_lock` is dropped (same function body).
            if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { fd })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(Self {})
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: see `exclusive`.
            unsafe { libc::flock(self.fd, libc::LOCK_UN) };
        }
    }
}

/// 读取结果。
#[derive(Debug, Default)]
pub struct Loaded {
    pub records: Vec<UsageRecord>,
    /// 读取的日文件数
    pub files: usize,
    /// 跳过的损坏完整行
    pub corrupt_lines: usize,
    /// 忽略的结尾半行（写入中或崩溃）
    pub torn_lines: usize,
}

/// 读取 `dir` 下日期在 `[from, to]`（含，均可省略）内的日文件。目录或文件不存在 ⇒ 空。只读，不改写任何文件。
pub fn load(dir: &Path, from: Option<NaiveDate>, to: Option<NaiveDate>) -> anyhow::Result<Loaded> {
    use anyhow::Context;
    crate::config::prepare_temp_data_dir(dir, false)?;
    let mut out = Loaded::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("cannot read usage directory {}", dir.display())),
    };
    let mut days: Vec<(NaiveDate, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let day = name.to_str()?.strip_suffix(".jsonl")?;
            let date = valid_day(day).then(|| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()).flatten()?;
            Some((date, e.path()))
        })
        .filter(|(date, _)| from.is_none_or(|f| *date >= f) && to.is_none_or(|t| *date <= t))
        .collect();
    days.sort();
    for (_, path) in days {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // 与列目录之间被删除：按空处理
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot read usage file {}", path.display())),
        };
        out.files += 1;
        let (corrupt, torn) = parse_lines(&bytes, &mut out.records);
        if corrupt > 0 {
            // 不打印行内容；按文件与数量报告
            tracing::warn!(file = %path.display(), corrupt_lines = corrupt, "skipped corrupt usage records");
        }
        if torn > 0 {
            tracing::debug!(file = %path.display(), "ignored a torn final usage line");
        }
        out.corrupt_lines += corrupt;
        out.torn_lines += torn;
    }
    tracing::debug!(dir = %dir.display(), files = out.files, records = out.records.len(), corrupt_lines = out.corrupt_lines, torn_lines = out.torn_lines, "usage journal loaded");
    Ok(out)
}

/// 解析一个日文件的内容；返回 (损坏的完整行数, 结尾半行数)。
fn parse_lines(bytes: &[u8], records: &mut Vec<UsageRecord>) -> (usize, usize) {
    let (mut corrupt, mut torn) = (0, 0);
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            // 只可能是最后一段：写入中或崩溃截断
            torn += 1;
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<UsageRecord>(line) {
            Ok(record) => records.push(record),
            Err(_) => corrupt += 1,
        }
    }
    (corrupt, torn)
}

/// 包在单个账号 provider 外面：把该账号每次尝试报告的用量写入 [`Journal`]。事件原样透传。
pub struct RecordingProvider {
    inner: Arc<dyn Provider>,
    key: AccountKey,
    meta: Arc<UsageMeta>,
    journal: Arc<Journal>,
}

impl RecordingProvider {
    pub fn new(inner: Arc<dyn Provider>, key: AccountKey, meta: Arc<UsageMeta>, journal: Arc<Journal>) -> Self {
        Self { inner, key, meta, journal }
    }
}

/// 一次 `stream` 调用内的尝试计数。
struct CallRecorder {
    key: AccountKey,
    meta: Arc<UsageMeta>,
    journal: Arc<Journal>,
    model: String,
    attempt: u32,
}

impl CallRecorder {
    fn observe(&mut self, event: &AssistantEvent) {
        match event {
            AssistantEvent::Retry { usage, kind, .. } => {
                match usage.as_ref().filter(|u| !u.is_empty()) {
                    Some(usage) => self.write(usage, true, false),
                    None => {
                        tracing::debug!(session_id = self.session(), account = %self.key.label(), model = %self.model, attempt = self.attempt, kind = %kind, "discarded attempt reported no usage; not recorded")
                    }
                }
                self.attempt += 1;
            }
            AssistantEvent::Done { message } | AssistantEvent::Error { message } => {
                let failed = matches!(event, AssistantEvent::Error { .. }) || message.stop_reason == StopReason::Error;
                if message.usage.is_empty() {
                    tracing::debug!(session_id = self.session(), account = %self.key.label(), model = %self.model, attempt = self.attempt, failed, stop_reason = ?message.stop_reason, "llm call reported no usage; not recorded");
                } else {
                    self.write(&message.usage, false, failed);
                }
            }
            _ => {}
        }
    }

    fn session(&self) -> &str {
        self.meta.session_id.as_deref().unwrap_or("-")
    }

    fn write(&self, usage: &Usage, discarded: bool, failed: bool) {
        let now = chrono::Local::now();
        let record = UsageRecord {
            v: RECORD_VERSION,
            ts: now.timestamp_millis(),
            time: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            day: now.format("%Y-%m-%d").to_string(),
            provider: self.key.provider.clone(),
            account: self.key.account.clone(),
            model: self.model.clone(),
            profile: self.meta.profile.clone(),
            session_id: self.meta.session_id.clone(),
            caller: self.meta.caller.clone(),
            attempt: self.attempt,
            retry: self.attempt > 1,
            discarded,
            failed,
            tokens: Tokens::from(usage),
        };
        self.journal.record(&record);
    }
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
        self.inner.list_models()
    }

    fn stream(&self, req: Request, cancel: CancellationToken) -> EventStream {
        let mut recorder = CallRecorder { key: self.key.clone(), meta: self.meta.clone(), journal: self.journal.clone(), model: req.model.clone(), attempt: 1 };
        Box::pin(self.inner.stream(req, cancel).map(move |event| {
            recorder.observe(&event);
            event
        }))
    }
}
