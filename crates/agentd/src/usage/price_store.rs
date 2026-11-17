//! Bounded models.dev downloads and an endpoint-scoped, last-good cache.
//! Only CLI commands and the serve worker do pricing I/O; reports clone a book.
use super::catalog::{CacheStatus, PriceBands, PriceBook, ProviderCounts, Table};
use super::models_dev::{compile_row, parse_models_dev, FeedRow, ParseStats};
use super::prices::{official_prices, PricingConfig, PricingSource, MODELS_DEV_URL};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CACHE_FILE: &str = "models-dev.json";
const CACHE_LOCK: &str = "models-dev.lock";
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy)]
struct FetchLimits {
    connect: Duration,
    total: Duration,
    max_bytes: usize,
}

impl FetchLimits {
    const CLI: Self = Self { connect: Duration::from_secs(3), total: Duration::from_secs(8), max_bytes: MAX_BYTES };
    const SERVE: Self = Self { connect: Duration::from_secs(2), total: Duration::from_secs(15), max_bytes: MAX_BYTES };
}

#[derive(Clone, Copy)]
pub(crate) enum RefreshUse {
    Usage,
    Pricing,
    Serve,
}

impl RefreshUse {
    fn trigger(self, force: bool) -> &'static str {
        match (self, force) {
            (Self::Serve, _) => "serve",
            (_, true) => "forced",
            _ => "cli",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheV1 {
    schema_version: u32,
    endpoint_hash: String,
    fetched_at: DateTime<Utc>,
    models: Table<FeedRow>,
}

struct LiveSnapshot {
    fetched_at: DateTime<Utc>,
    rates: Table<PriceBands>,
    counts: ProviderCounts,
}

impl LiveSnapshot {
    fn new(fetched_at: DateTime<Utc>, rates: Table<PriceBands>) -> Self {
        let counts = ProviderCounts::of(&rates);
        Self { fetched_at, rates, counts }
    }
}

struct Endpoint {
    url: Url,
    hash: String,
}

impl Endpoint {
    fn label(&self) -> &'static str {
        if self.url.as_str() == MODELS_DEV_URL {
            "default"
        } else {
            "custom"
        }
    }
}

pub(crate) struct PriceStore {
    dir: PathBuf,
    endpoint: Option<Endpoint>,
    current: RwLock<Arc<PriceBook>>,
}

impl PriceStore {
    pub(crate) fn new(data_dir: PathBuf, cfg: &PricingConfig) -> Self {
        let endpoint = if cfg.source == PricingSource::ModelsDev { cfg.models_dev_endpoint().map(|url| Endpoint { hash: crate::config::sha256_hex(url.as_str()), url }) } else { None };
        Self { dir: data_dir.join("pricing"), endpoint, current: RwLock::new(Arc::new(PriceBook::build(cfg))) }
    }

    pub(crate) fn snapshot(&self) -> Arc<PriceBook> {
        self.current.read().expect("pricing snapshot lock poisoned").clone()
    }

    // Only one writer in-process (a CLI invocation or the serve worker). Readers
    // don't observe in-flight disk/network work or hold the lock during a report.
    fn publish(&self, current: &PriceBook, new: LiveSnapshot) -> Arc<PriceBook> {
        let book = Arc::new(current.with_live(new.fetched_at, new.rates));
        *self.current.write().expect("pricing snapshot lock poisoned") = book.clone();
        book
    }

    pub(crate) fn worker_enabled(&self) -> bool {
        self.endpoint.is_some() && self.snapshot().enabled
    }

    pub(crate) async fn seed_from_disk(&self) {
        if let Some(endpoint) = self.endpoint.as_ref().filter(|_| self.worker_enabled()) {
            self.adopt_disk(self.snapshot(), endpoint, "serve", &Utc::now).await;
        }
    }

    pub(crate) async fn ensure_current(&self, purpose: RefreshUse, force: bool) -> Arc<PriceBook> {
        self.ensure_with(purpose, force, FetchLimits::CLI, &Utc::now).await
    }

    pub(crate) async fn run_worker(&self, shutdown: CancellationToken) {
        self.worker(shutdown, CHECK_INTERVAL, FetchLimits::SERVE, &Utc::now).await;
    }

    async fn worker(&self, shutdown: CancellationToken, period: Duration, limits: FetchLimits, now: &(impl Fn() -> DateTime<Utc> + Clone + Send + Sync + 'static)) {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {}
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = self.ensure_with(RefreshUse::Serve, false, limits, now) => {}
            }
        }
    }

    async fn adopt_disk(&self, current: Arc<PriceBook>, endpoint: &Endpoint, trigger: &str, now: &impl Fn() -> DateTime<Utc>) -> Arc<PriceBook> {
        let dir = self.dir.clone();
        let hash = endpoint.hash.clone();
        let disk = tokio::task::spawn_blocking(move || read_cache(&dir, &hash)).await.unwrap_or(Err("cache_task"));
        match disk {
            Ok(Some(disk)) => {
                let now = now();
                if current.fetched_at.is_none_or(|at| preferred(disk.fetched_at, at, now)) {
                    return self.publish(&current, disk);
                }
            }
            Ok(None) => {}
            Err(kind) => tracing::debug!(trigger, endpoint = endpoint.label(), error_kind = kind, "pricing.cache_read_fail"),
        }
        current
    }

    async fn ensure_with(&self, purpose: RefreshUse, force: bool, limits: FetchLimits, now: &(impl Fn() -> DateTime<Utc> + Clone + Send + Sync + 'static)) -> Arc<PriceBook> {
        let current = self.snapshot();
        let trigger = purpose.trigger(force);
        let disabled_by = match (&self.endpoint, purpose) {
            (None, _) => Some("pricing.source=builtin"),
            (_, RefreshUse::Usage | RefreshUse::Serve) if !current.enabled => Some("pricing.enabled=false"),
            _ => None,
        };
        if let Some(reason) = disabled_by {
            if force {
                tracing::warn!(trigger, reason, issue = "none", "pricing.refresh_disabled");
            }
            return current;
        }
        let endpoint = self.endpoint.as_ref().expect("checked live endpoint");
        let current = self.adopt_disk(current, endpoint, trigger, now).await;
        let checked_at = now();
        let info = current.cache_info(checked_at);
        let check = match info.cache_status {
            CacheStatus::Fresh => "pricing.cache_hit",
            CacheStatus::Stale => "pricing.cache_stale",
            _ => "pricing.cache_miss",
        };
        let age_secs = info.fetched_at.map(|at| (checked_at - at).num_seconds());
        let endpoint_kind = endpoint.label();
        let ttl_hours = current.refresh_hours;
        tracing::debug!(trigger, endpoint = endpoint_kind, ?age_secs, ttl_hours, fetched_at = ?info.fetched_at, "{check}");
        if info.cache_status == CacheStatus::Fresh && !force {
            return current;
        }

        tracing::info!(trigger, endpoint = endpoint_kind, ?age_secs, ttl_hours, fetched_at = ?info.fetched_at, budget_ms = limits.total.as_millis(), "pricing.fetch_start");
        let start = Instant::now();
        let mut progress = FetchProgress::default();
        let result = fetch_feed(&endpoint.url, limits, &mut progress).await;
        let duration_ms = start.elapsed().as_millis();
        let bytes_received = progress.bytes;
        let status = progress.status;
        let failed = |error_kind, counts: &ProviderCounts, stats: &ParseStats| {
            tracing::warn!(trigger, endpoint = endpoint_kind, error_kind, ?status, bytes_received, duration_ms,
                openai = counts.openai, anthropic = counts.anthropic, skipped = ?stats.skipped, issue = "none", "pricing.fetch_fail");
        };
        let fallback = |current: Arc<PriceBook>| {
            let info = current.cache_info(now());
            let chosen = if info.fetched_at.is_some() { info.cache_status.as_str() } else { "builtin" };
            tracing::warn!(trigger, endpoint = endpoint_kind, chosen, fetched_at = ?info.fetched_at, ttl_hours, issue = "none", "pricing.fallback");
            current
        };
        let download = match result {
            Ok(d) => d,
            Err(kind) => {
                failed(kind, &ProviderCounts::default(), &ParseStats::default());
                return fallback(current);
            }
        };
        let fetched_at = now();
        let live = LiveSnapshot::new(fetched_at, download.rates);
        if live.rates.is_empty() {
            failed("no_usable_models", &live.counts, &download.stats);
            return fallback(current);
        }
        if current.fetched_at.is_some_and(|at| at <= fetched_at) && live.counts.loses_provider(&current.live_counts) {
            failed("provider_lost", &live.counts, &download.stats);
            return fallback(current);
        }

        let candidate = CacheV1 { schema_version: SCHEMA_VERSION, endpoint_hash: endpoint.hash.clone(), fetched_at, models: download.models };
        let dir = self.dir.clone();
        let counts = live.counts;
        let commit_clock = now.clone();
        let commit = tokio::task::spawn_blocking(move || persist_atomic(&dir, &candidate, &counts, &commit_clock)).await.unwrap_or(Err("cache_task"));
        let chosen = match commit {
            Ok(Commit::KeptProvider(disk)) => {
                failed("provider_lost", &live.counts, &download.stats);
                return fallback(self.publish(&current, disk));
            }
            Ok(Commit::KeptNewer(disk)) => {
                tracing::debug!(trigger, endpoint = endpoint_kind, fetched_at = %disk.fetched_at, "pricing.cache_newer");
                disk
            }
            Ok(Commit::Written) => live,
            Err(kind) => {
                tracing::warn!(trigger, endpoint = endpoint_kind, error_kind = kind, %fetched_at, issue = "none", "pricing.cache_write_fail");
                live
            }
        };
        tracing::info!(trigger, endpoint = endpoint_kind, ?status, bytes_received, duration_ms, %fetched_at,
            openai = counts.openai, anthropic = counts.anthropic, skipped = ?download.stats.skipped, "pricing.fetch_ok");
        self.publish(&current, chosen)
    }
}

fn preferred(candidate: DateTime<Utc>, old: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    // Prefer a nonfuture timestamp to any future one, regardless of their relative ages.
    (candidate <= now, candidate) > (old <= now, old)
}

#[derive(Default)]
struct FetchProgress {
    bytes: usize,
    status: Option<u16>,
}

struct Download {
    models: Table<FeedRow>,
    rates: Table<PriceBands>,
    stats: ParseStats,
}

type SafeResult<T> = Result<T, &'static str>;

async fn fetch_feed(url: &Url, limits: FetchLimits, progress: &mut FetchProgress) -> SafeResult<Download> {
    tokio::time::timeout(limits.total, async {
        let client = agent_ai::http::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            // Do not inherit authenticated environment proxies for the public feed.
            .no_proxy()
            .connect_timeout(limits.connect)
            .timeout(limits.total)
            .build()
            .map_err(|_| "client_build")?;
        let mut response = client.get(url.clone()).send().await.map_err(|e| {
            if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connect"
            } else {
                "request"
            }
        })?;
        let status = response.status();
        progress.status = Some(status.as_u16());
        if status != reqwest::StatusCode::OK {
            return Err("http_status");
        }
        if response.content_length().is_some_and(|n| n > limits.max_bytes as u64) {
            return Err("size_limit");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| if e.is_timeout() { "timeout" } else { "body" })? {
            progress.bytes = progress.bytes.saturating_add(chunk.len());
            if progress.bytes > limits.max_bytes {
                return Err("size_limit");
            }
            body.extend_from_slice(&chunk);
        }
        // This task has no access to the cache/store: a parse that outlives the
        // deadline or shutdown can neither publish nor write anything.
        tokio::task::spawn_blocking(move || compile_download(&body)).await.map_err(|_| "parse_task")?
    })
    .await
    .unwrap_or(Err("timeout"))
}

fn compile_download(body: &[u8]) -> SafeResult<Download> {
    let mut parsed = parse_models_dev(body).map_err(|_| "invalid_json")?;
    let builtins = official_prices();
    let mut rates = Table::new();
    parsed.models.retain(|key, row| match compile_row(&builtins, key, row) {
        Ok(bands) => {
            rates.insert(key.clone(), bands);
            true
        }
        Err(reason) => {
            parsed.stats.skip(reason);
            false
        }
    });
    Ok(Download { models: parsed.models, rates, stats: parsed.stats })
}

fn read_cache(dir: &Path, hash: &str) -> SafeResult<Option<LiveSnapshot>> {
    crate::config::prepare_temp_data_dir(dir, false).map_err(|_| "unsafe_data_dir")?;
    let path = dir.join(CACHE_FILE);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cache_metadata"),
    };
    if !meta.is_file() {
        return Err("not_regular");
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err("size_limit");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A replacement between stat and open must not follow a symlink or block on a FIFO.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cache_open"),
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("not_regular");
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|_| "cache_read")?;
    if bytes.len() > MAX_BYTES {
        return Err("size_limit");
    }
    let cache: CacheV1 = serde_json::from_slice(&bytes).map_err(|_| "cache_schema")?;
    if cache.schema_version != SCHEMA_VERSION {
        return Err("cache_version");
    }
    if cache.endpoint_hash != hash {
        return Err("cache_endpoint");
    }
    if cache.models.is_empty() {
        return Err("no_usable_models");
    }
    let builtins = official_prices();
    let rates = cache.models.iter().map(|(k, row)| compile_row(&builtins, k, row).map(|bands| (k.clone(), bands))).collect::<Result<Table<_>, _>>().map_err(|_| "cache_rates")?;
    Ok(Some(LiveSnapshot::new(cache.fetched_at, rates)))
}

enum Commit {
    Written,
    KeptNewer(LiveSnapshot),
    KeptProvider(LiveSnapshot),
}

fn persist_atomic(dir: &Path, candidate: &CacheV1, counts: &ProviderCounts, now: &impl Fn() -> DateTime<Utc>) -> SafeResult<Commit> {
    crate::config::prepare_temp_data_dir(dir, true).map_err(|_| "unsafe_data_dir")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir).map_err(|_| "cache_dir")?;
        let meta = fs::symlink_metadata(dir).map_err(|_| "cache_dir")?;
        if !meta.is_dir() {
            return Err("cache_dir");
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|_| "cache_dir")?;
        }
    }
    #[cfg(not(unix))]
    builder.create(dir).map_err(|_| "cache_dir")?;

    let temp = dir.join(format!(".models-dev-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&temp).map_err(|_| "cache_temp")?;
    let result = (|| {
        let mut writer = io::BufWriter::new(&file);
        serde_json::to_writer(CappedWriter { writer: &mut writer, remaining: MAX_BYTES }, candidate).map_err(|_| "cache_encode")?;
        writer.flush().map_err(|_| "cache_write")?;
        file.sync_all().map_err(|_| "cache_sync")?;
        drop(writer);
        drop(file);

        // Serialize commits, not fetches or temp-file writes. Dropping the file
        // releases flock, including on early return. Readers don't take this lock.
        let lock = try_lock(dir)?;
        if let Ok(Some(disk)) = read_cache(dir, &candidate.endpoint_hash) {
            let now = now();
            if preferred(disk.fetched_at, candidate.fetched_at, now) {
                return Ok(Commit::KeptNewer(disk));
            }
            if disk.fetched_at <= now && counts.loses_provider(&disk.counts) {
                return Ok(Commit::KeptProvider(disk));
            }
        }
        fs::rename(&temp, dir.join(CACHE_FILE)).map_err(|_| "cache_rename")?;
        drop(lock);
        #[cfg(unix)]
        File::open(dir).and_then(|dir| dir.sync_all()).map_err(|_| "cache_dir_sync")?;
        Ok(Commit::Written)
    })();
    if !matches!(result, Ok(Commit::Written)) {
        let _ = fs::remove_file(temp);
    }
    result
}

struct CappedWriter<W> {
    writer: W,
    remaining: usize,
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("cache size limit"));
        }
        let n = self.writer.write(bytes)?;
        self.remaining -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn try_lock(dir: &Path) -> SafeResult<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(dir.join(CACHE_LOCK)).map_err(|_| "cache_lock")?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("cache_lock");
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the fd is owned by `file` for the lifetime of the lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock { "cache_lock_busy" } else { "cache_lock" });
        }
    }
    Ok(file)
}

#[cfg(test)]
#[path = "price_store_tests.rs"]
mod tests;
