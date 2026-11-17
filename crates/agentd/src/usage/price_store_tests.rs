use super::*;
use crate::usage::catalog::PriceSource;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/models-dev-pricing.json");
const OPENAI_ONLY: &[u8] = br#"{"openai":{"models":{"test":{"cost":{"input":1,"output":2}}}}}"#;
const LIMITS: FetchLimits = FetchLimits { connect: Duration::from_secs(1), total: Duration::from_secs(3), max_bytes: 4096 };

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sub-bot-prices-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn pricing(&self) -> PathBuf {
        self.0.join("pricing")
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Reply {
    first: Vec<u8>,
    hold: bool,
}
impl Reply {
    fn body(body: &[u8]) -> Self {
        let mut first = format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        first.extend_from_slice(body);
        Self { first, hold: false }
    }
    fn raw(first: &[u8], hold: bool) -> Self {
        Self { first: first.to_vec(), hold }
    }
}

struct Script {
    url: Url,
    calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Script {
    async fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/catalog?type=all", listener.local_addr().unwrap())).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut queue: VecDeque<_> = replies.into_iter().collect();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let reply = queue.pop_front().unwrap_or_else(|| Reply::body(b"{}"));
                let count = count.clone();
                connections.spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        let Ok(byte) = socket.read_u8().await else { return };
                        head.push(byte);
                        assert!(head.len() < 4096);
                    }
                    assert!(head.starts_with(b"GET /catalog?type=all HTTP/1.1\r\n"));
                    count.fetch_add(1, Ordering::Relaxed);
                    if socket.write_all(&reply.first).await.is_ok() && reply.hold {
                        // Never send the rest. The client must time out or cancel.
                        let _ = socket.read_u8().await;
                    }
                });
                while connections.try_join_next().is_some() {}
            }
        });
        Self { url, calls, task }
    }
    fn config(&self) -> PricingConfig {
        PricingConfig { models_dev_url: self.url.to_string(), ..Default::default() }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}
impl Drop for Script {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn epoch() -> DateTime<Utc> {
    "2026-11-17T12:00:00Z".parse().unwrap()
}

fn candidate(store: &PriceStore, fetched_at: DateTime<Utc>, feed: &[u8]) -> (CacheV1, ProviderCounts) {
    let download = compile_download(feed).unwrap();
    let counts = ProviderCounts::of(&download.rates);
    (CacheV1 { schema_version: SCHEMA_VERSION, endpoint_hash: store.endpoint.as_ref().unwrap().hash.clone(), fetched_at, models: download.models }, counts)
}

async fn ensure(store: &PriceStore, at: DateTime<Utc>, force: bool) -> Arc<PriceBook> {
    store.ensure_with(RefreshUse::Usage, force, LIMITS, &move || at).await
}

#[tokio::test]
async fn cold_fresh_ttl_equality_and_forced_refresh() {
    let server = Script::new([Reply::body(FIXTURE), Reply::body(FIXTURE), Reply::body(FIXTURE)]).await;
    let dir = TempDir::new();
    let store = PriceStore::new(dir.0.clone(), &server.config());
    let at = epoch();
    let cache = dir.pricing().join(CACHE_FILE);
    assert_eq!(store.snapshot().cache_info(at).cache_status, CacheStatus::Missing);
    assert!(!dir.pricing().exists());
    let first = ensure(&store, at, false).await;
    assert_eq!(first.fetched_at, Some(at));
    assert_eq!(first.cache_info(at).cache_status, CacheStatus::Fresh);
    assert_eq!((first.live_counts.openai, first.live_counts.anthropic), (3, 3));
    assert_eq!(server.count(), 1);
    let bytes = fs::read(&cache).unwrap();
    assert!(bytes.len() < FIXTURE.len());
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["endpointHash"], crate::config::sha256_hex(server.url.as_str()));
    assert_eq!(v["fetchedAt"], "2026-11-17T12:00:00Z");
    assert_eq!(v["models"]["openai/gpt-6.2-sol"]["tiers"][0]["threshold"], 272_000);
    assert!(v["models"]["openai/gpt-6.2-sol"]["tiers"][0].get("cacheWrite12h").is_none());
    assert!(!String::from_utf8(bytes).unwrap().contains(server.url.as_str()));
    assert!(!dir.0.join("usage").exists() && !dir.0.join("state").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(dir.pricing()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    let reopen = PriceStore::new(dir.0.clone(), &server.config());
    let ttl = chrono::TimeDelta::hours(24);
    assert_eq!(ensure(&reopen, at + ttl - chrono::TimeDelta::milliseconds(1), false).await.fetched_at, Some(at));
    assert_eq!(server.count(), 1);
    assert_eq!(first.cache_info(at + ttl).cache_status, CacheStatus::Stale);
    assert_eq!(first.cache_info(at - chrono::TimeDelta::seconds(1)).cache_status, CacheStatus::Stale);
    assert_eq!(ensure(&reopen, at + ttl, false).await.fetched_at, Some(at + ttl));
    assert_eq!(server.count(), 2);
    let forced = at + ttl + chrono::TimeDelta::minutes(1);
    assert_eq!(ensure(&reopen, forced, true).await.fetched_at, Some(forced));
    assert_eq!(server.count(), 3);
    // Held reports continue using the old metadata and rates after the swap.
    assert_eq!(first.fetched_at, Some(at));
}

#[tokio::test]
async fn offline_switch_and_usage_display_switch_do_not_inspect_cache() {
    let server = Script::new([Reply::body(FIXTURE), Reply::body(FIXTURE)]).await;
    let dir = TempDir::new();
    let cfg = server.config();
    let online = PriceStore::new(dir.0.clone(), &cfg);
    let at = epoch();
    ensure(&online, at, false).await;
    let offline = PriceStore::new(dir.0.clone(), &PricingConfig { source: PricingSource::Builtin, ..cfg.clone() });
    assert!(!offline.worker_enabled());
    offline.seed_from_disk().await;
    let book = offline.ensure_with(RefreshUse::Pricing, true, LIMITS, &move || at).await;
    assert_eq!(book.cache_info(at).cache_status, CacheStatus::Disabled);
    assert!(book.fetched_at.is_none());
    assert_eq!(book.lookup("openai", "gpt-6.2-sol").unwrap().source, PriceSource::Builtin);
    assert_eq!(server.count(), 1);

    let hidden = PriceStore::new(dir.0.clone(), &PricingConfig { enabled: false, ..cfg });
    assert!(!hidden.worker_enabled());
    hidden.seed_from_disk().await;
    assert!(ensure(&hidden, at, true).await.fetched_at.is_none());
    assert_eq!(server.count(), 1);
    assert_eq!(hidden.ensure_with(RefreshUse::Pricing, true, LIMITS, &move || at).await.fetched_at, Some(at));
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn future_cache_cannot_block_a_commit_and_mirror_changes_scope_the_cache() {
    let server = Script::new([Reply::body(OPENAI_ONLY)]).await;
    let dir = TempDir::new();
    let store = PriceStore::new(dir.0.clone(), &server.config());
    let at = epoch();
    let (future, counts) = candidate(&store, at + chrono::TimeDelta::days(1), FIXTURE);
    assert!(matches!(persist_atomic(&dir.pricing(), &future, &counts, &|| at), Ok(Commit::Written)));
    assert_eq!(ensure(&store, at, false).await.fetched_at, Some(at));
    assert_eq!(read_cache(&dir.pricing(), &future.endpoint_hash).unwrap().unwrap().fetched_at, at);
    assert_eq!(server.count(), 1);
    assert!(!preferred(future.fetched_at, at, at));
    assert!(preferred(at, future.fetched_at, at));

    // Change just the query parameter. Don't actually send a request for this check.
    let changed_url = server.url.as_str().replace("type=all", "type=other");
    let other = PriceStore::new(dir.0.clone(), &PricingConfig { models_dev_url: changed_url, ..server.config() });
    other.seed_from_disk().await;
    assert!(other.snapshot().fetched_at.is_none());
    assert!(matches!(read_cache(&dir.pricing(), &other.endpoint.unwrap().hash), Err("cache_endpoint")));
}

#[tokio::test]
async fn failed_refreshes_keep_stale_bytes_and_complete_previous_providers() {
    let replies = [
        Reply::body(FIXTURE),
        Reply::body(OPENAI_ONLY),
        Reply::body(br#"{"openai":{"models":[]},"anthropic":null}"#),
        Reply::body(b"{}"),
        Reply::body(b"{not-json"),
        Reply::body(b""),
        Reply::raw(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 100\r\n\r\n", true),
        Reply::raw(b"HTTP/1.1 302 Found\r\nLocation: /catalog?type=all\r\nContent-Length: 0\r\n\r\n", false),
        Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 4097\r\n\r\n", true),
        Reply::raw(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1001\r\n", false),
    ];
    let tries = replies.len();
    let server = Script::new(replies).await;
    let dir = TempDir::new();
    let store = PriceStore::new(dir.0.clone(), &server.config());
    let at = epoch();
    ensure(&store, at, false).await;
    let cache = dir.pricing().join(CACHE_FILE);
    let bytes = fs::read(&cache).unwrap();
    let stale = at + chrono::TimeDelta::hours(25);
    for _ in 1..tries {
        let book = ensure(&store, stale, false).await;
        assert_eq!(book.fetched_at, Some(at));
        assert_eq!(book.cache_info(stale).cache_status, CacheStatus::Stale);
        assert_eq!((book.live_counts.openai, book.live_counts.anthropic), (3, 3));
        assert_eq!(fs::read(&cache).unwrap(), bytes);
    }
    assert_eq!(server.count(), tries);

    let cfg = server.config();
    drop(server);
    tokio::task::yield_now().await; // close the loopback listener
    let cold_dir = TempDir::new();
    let offline = PriceStore::new(cold_dir.0.clone(), &cfg);
    assert_eq!(ensure(&offline, at, false).await.lookup("openai", "gpt-6.2-sol").unwrap().source, PriceSource::Builtin);
    assert!(!cold_dir.pricing().exists());
    assert_eq!(ensure(&store, stale, true).await.fetched_at, Some(at));
}

#[tokio::test]
async fn bounded_fetch_counts_partial_bytes_and_covers_the_body_deadline() {
    let chunked = [b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1234\r\n6\r\n".as_slice(), b"567890\r\n0\r\n\r\n"].concat();
    let server = Script::new([
        Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 9000\r\n\r\n", true),
        Reply::raw(&chunked, false),
        Reply::raw(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n{", true),
        Reply::raw(b"", true),
    ])
    .await;
    let small = FetchLimits { total: Duration::from_millis(300), max_bytes: 8, ..LIMITS };
    for (kind, bytes, status) in [("size_limit", 0, Some(200)), ("size_limit", 10, Some(200)), ("timeout", 1, Some(200)), ("timeout", 0, None)] {
        let mut progress = FetchProgress::default();
        assert_eq!(fetch_feed(&server.url, small, &mut progress).await.err(), Some(kind));
        assert_eq!((progress.bytes, progress.status), (bytes, status));
    }
    assert_eq!(server.count(), 4);
}

#[test]
fn corrupt_wrong_version_and_nonregular_cache_are_misses() {
    let dir = TempDir::new();
    fs::create_dir(dir.pricing()).unwrap();
    let file = dir.pricing().join(CACHE_FILE);
    let store = PriceStore::new(dir.0.clone(), &PricingConfig::default());
    let (valid, _) = candidate(&store, epoch(), FIXTURE);
    let hash = &valid.endpoint_hash;
    assert!(read_cache(&dir.pricing(), hash).unwrap().is_none());
    let json = serde_json::to_value(&valid).unwrap();
    for (pointer, replacement, reason) in [
        ("/schemaVersion", serde_json::json!(44), "cache_version"),
        ("/endpointHash", serde_json::json!("other"), "cache_endpoint"),
        ("/fetchedAt", serde_json::json!("soon"), "cache_schema"),
        ("/models", serde_json::json!({}), "no_usable_models"),
        ("/models/openai~1gpt-6.2-sol/base/input", serde_json::json!(-1), "cache_rates"),
        ("/models/openai~1gpt-6.2-sol/tiers/0/threshold", serde_json::json!(0), "cache_rates"),
        ("/models", serde_json::json!({"other/test":{"base":{"input":1,"output":2},"tiers":[]}}), "cache_rates"),
    ] {
        let mut bad = json.clone();
        *bad.pointer_mut(pointer).unwrap() = replacement;
        fs::write(&file, bad.to_string()).unwrap();
        assert_eq!(read_cache(&dir.pricing(), hash).err(), Some(reason));
    }
    fs::write(&file, b"{bad-json").unwrap();
    assert!(matches!(read_cache(&dir.pricing(), hash), Err("cache_schema")));
    File::create(&file).unwrap().set_len(MAX_BYTES as u64 + 1).unwrap();
    assert!(matches!(read_cache(&dir.pricing(), hash), Err("size_limit")));
    fs::remove_file(&file).unwrap();
    fs::create_dir(file).unwrap();
    assert!(matches!(read_cache(&dir.pricing(), hash), Err("not_regular")));
}

#[tokio::test]
async fn cache_write_failure_does_not_discard_valid_in_memory_prices() {
    let server = Script::new([Reply::body(FIXTURE)]).await;
    let dir = TempDir::new();
    fs::write(dir.pricing(), b"not a directory").unwrap();
    let store = PriceStore::new(dir.0.clone(), &server.config());
    let at = epoch();
    assert_eq!(ensure(&store, at, false).await.fetched_at, Some(at));
    assert_eq!(ensure(&store, at, false).await.lookup("openai", "gpt-6.2-sol").unwrap().source, PriceSource::ModelsDev);
    assert_eq!(server.count(), 1);
    assert_eq!(fs::read(dir.pricing()).unwrap(), b"not a directory");
}

#[cfg(unix)]
#[test]
fn interrupted_write_and_concurrent_commits_preserve_whole_snapshots() {
    let dir = TempDir::new();
    let pricing = &dir.pricing();
    let store = PriceStore::new(dir.0.clone(), &PricingConfig::default());
    let at = epoch();
    let seconds = chrono::TimeDelta::seconds;
    let (old, counts) = candidate(&store, at, FIXTURE);
    assert!(matches!(persist_atomic(pricing, &old, &counts, &|| at), Ok(Commit::Written)));
    let hash = old.endpoint_hash.as_str();
    fs::write(pricing.join(".models-dev-interrupted.tmp"), b"{\"schemaVersion\":1,").unwrap();
    let (new, _) = candidate(&store, at + seconds(1), FIXTURE);
    {
        let _lock = try_lock(pricing).unwrap();
        assert!(matches!(persist_atomic(pricing, &new, &counts, &|| at + seconds(2)), Err("cache_lock_busy")));
        assert_eq!(read_cache(pricing, hash).unwrap().unwrap().fetched_at, at);
        assert_eq!(fs::read_dir(pricing).unwrap().count(), 3); // no temp left by the busy writer
    }
    let mut too_large = serde_json::from_slice::<CacheV1>(&serde_json::to_vec(&old).unwrap()).unwrap();
    too_large.models.insert(format!("openai/{}", "x".repeat(MAX_BYTES)), old.models.values().next().unwrap().clone());
    assert!(matches!(persist_atomic(pricing, &too_large, &counts, &|| at), Err("cache_encode")));
    assert_eq!(fs::read_dir(pricing).unwrap().count(), 3);
    assert_eq!(read_cache(pricing, hash).unwrap().unwrap().fetched_at, at);

    std::thread::scope(|scope| {
        for n in 1..=10 {
            let (next, _) = candidate(&store, at + seconds(n), FIXTURE);
            scope.spawn(move || {
                let result = persist_atomic(pricing, &next, &counts, &|| at + seconds(100));
                assert!(matches!(result, Ok(Commit::Written | Commit::KeptNewer(_)) | Err("cache_lock_busy")));
            });
        }
        for _ in 0..30 {
            let disk = read_cache(pricing, hash).unwrap().unwrap();
            assert_eq!((disk.counts.openai, disk.counts.anthropic), (3, 3));
        }
    });
    assert!(matches!(persist_atomic(pricing, &old, &counts, &|| at + seconds(100)), Ok(Commit::KeptNewer(_))));
    let (partial, partial_counts) = candidate(&store, at + seconds(100), OPENAI_ONLY);
    assert!(matches!(persist_atomic(pricing, &partial, &partial_counts, &|| at + seconds(100)), Ok(Commit::KeptProvider(_))));
}

#[tokio::test(start_paused = true)]
async fn worker_checks_on_tick_adopts_cli_cache_and_cancels_a_blocked_refresh() {
    async fn until(mut ready: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ready() {
            assert!(start.elapsed() < Duration::from_secs(5), "worker did not progress");
            // Keep the runtime runnable; time should advance only where the test says so.
            tokio::task::yield_now().await;
        }
    }

    let server = Script::new([Reply::body(FIXTURE), Reply::body(FIXTURE), Reply::raw(b"", true)]).await;
    let dir = TempDir::new();
    let cfg = PricingConfig { refresh_hours: 1, ..server.config() };
    let store = Arc::new(PriceStore::new(dir.0.clone(), &cfg));
    let start = tokio::time::Instant::now();
    let reads = Arc::new(AtomicUsize::new(0));
    let clock = {
        let reads = reads.clone();
        move || {
            reads.fetch_add(1, Ordering::Relaxed);
            epoch() + chrono::TimeDelta::from_std(start.elapsed()).unwrap()
        }
    };
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let (store, stop, clock) = (store.clone(), stop.clone(), clock.clone());
        async move { store.worker(stop, CHECK_INTERVAL, LIMITS, &clock).await }
    });
    until(|| store.snapshot().fetched_at.is_some()).await;
    assert_eq!(server.count(), 1); // immediate cold tick

    let tick = |reads: &AtomicUsize| reads.load(Ordering::Relaxed);
    let previous = tick(&reads);
    tokio::time::advance(CHECK_INTERVAL).await;
    until(|| tick(&reads) > previous).await;
    assert_eq!(server.count(), 1); // fresh, no GET

    let cli = PriceStore::new(dir.0.clone(), &cfg);
    let cli_at = clock();
    // Keep paused time fixed while the loopback transfer runs.
    let cli_task = tokio::spawn(async move { ensure(&cli, cli_at, true).await });
    until(|| cli_task.is_finished()).await;
    assert_eq!(cli_task.await.unwrap().fetched_at, Some(cli_at));
    assert_eq!(server.count(), 2);
    tokio::time::advance(CHECK_INTERVAL).await;
    until(|| store.snapshot().fetched_at == Some(cli_at)).await;
    for _ in 0..2 {
        let previous = tick(&reads);
        tokio::time::advance(CHECK_INTERVAL).await;
        until(|| tick(&reads) > previous).await;
        assert_eq!(server.count(), 2);
    }
    tokio::time::advance(CHECK_INTERVAL).await;
    until(|| server.count() == 3).await; // now stale at exactly one hour
    assert_eq!(store.snapshot().fetched_at, Some(cli_at));
    stop.cancel();
    until(|| task.is_finished()).await;
    task.await.unwrap();
    assert_eq!(store.snapshot().fetched_at, Some(cli_at));
    assert_eq!(server.count(), 3);
}
