//! 用量日志、聚合、估测费用与 RecordingProvider 的测试。
use super::{
    catalog::{PriceBook, PriceSource},
    models_dev::{compile_row, parse_models_dev},
    prices::{self, ModelPrice, PricingConfig, PricingSource, UsageEstimateConfig},
    report::{aggregate, build, Filters, GroupBy, Query, RangeSpec},
    Journal, RecordingProvider, Tokens, UsageMeta, UsageRecord, RECORD_VERSION, USAGE_DIR,
};
use crate::accounts::AccountKey;
use agent_ai::{AssistantEvent, AssistantMessage, EventStream, ModelInfo, Provider, Request, StopReason, Usage};
use chrono::{FixedOffset, TimeZone};
use futures::StreamExt;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::SystemTime};
use tokio_util::sync::CancellationToken;

fn tmp() -> PathBuf {
    let path = std::env::temp_dir().join(format!("sub-bot-usage-{}-{}", unix_ms(), uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis()
}

fn record(ts: i64, day: &str, provider: &str, account: &str, model: &str, tokens: Tokens) -> UsageRecord {
    rec(ts, day, provider, account, model, tokens, 1, false, false, Some("coder"), Some("sess-1"), "local")
}

#[allow(clippy::too_many_arguments)]
fn rec(
    ts: i64,
    day: &str,
    provider: &str,
    account: &str,
    model: &str,
    tokens: Tokens,
    attempt: u32,
    discarded: bool,
    failed: bool,
    profile: Option<&str>,
    session_id: Option<&str>,
    caller: &str,
) -> UsageRecord {
    UsageRecord {
        v: RECORD_VERSION,
        ts,
        time: String::new(),
        day: day.into(),
        provider: provider.into(),
        account: account.into(),
        model: model.into(),
        profile: profile.map(str::to_string),
        session_id: session_id.map(str::to_string),
        caller: caller.into(),
        attempt,
        retry: attempt > 1,
        discarded,
        failed,
        tokens,
    }
}

fn toks(input: u64, output: u64, cache_read: u64, cache_write: u64, reasoning: u64) -> Tokens {
    let mut t = Tokens { input, output, cache_read, cache_write, cache_write_5m: cache_write, reasoning, total_tokens: input + output + cache_read + cache_write, ..Default::default() };
    t.normalize(true);
    t
}

fn toks_openai(input: u64, output: u64, cached: u64, write_30m: u64, write_12h: u64, reasoning: u64) -> Tokens {
    Tokens {
        input,
        output,
        cache_read: cached,
        cache_write: write_30m + write_12h,
        cache_write_30m: write_30m,
        cache_write_12h: write_12h,
        reasoning,
        // OpenAI total is prompt + completion; prompt already includes cache read/write.
        total_tokens: input + output,
        ..Default::default()
    }
}

fn offset_hours(h: i32) -> FixedOffset {
    FixedOffset::east_opt(h * 3600).unwrap()
}

#[test]
fn journal_appends_survives_reopen_and_ignores_torn_and_corrupt_lines() {
    let dir = tmp();
    let journal = Journal::new(dir.join(USAGE_DIR));
    let a = record(1, "2026-11-16", "anthropic", "work", "claude-opus-5-5", toks(10, 2, 0, 0, 0));
    let b = record(2, "2026-11-16", "openai", "default", "gpt-6.1-sol", toks_openai(3, 4, 1, 0, 0, 2));
    journal.append(&a).unwrap();
    journal.append(&b).unwrap();

    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert_eq!(loaded.records, vec![a.clone(), b.clone()]);
    assert_eq!((loaded.files, loaded.corrupt_lines, loaded.torn_lines), (1, 0, 0));

    // 再打开同一目录，追加第三条，历史仍在
    let journal2 = Journal::new(journal.dir().to_path_buf());
    let c = record(3, "2026-11-16", "anthropic", "my", "claude-sonnet-5-5", toks(1, 1, 0, 0, 0));
    journal2.append(&c).unwrap();
    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert_eq!(loaded.records, vec![a.clone(), b.clone(), c.clone()]);

    // 人为截断最后一行；读取忽略半行，已有记录仍在
    let file = journal.dir().join("2026-11-16.jsonl");
    let mut bytes = fs::read(&file).unwrap();
    // 在完整记录之后追加半行（不要破坏已有记录）
    bytes.extend_from_slice(b"{\"v\":1,\"ts\":99"); // 半行
    fs::write(&file, &bytes).unwrap();
    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert_eq!(loaded.records, vec![a.clone(), b, c.clone()]);
    assert_eq!(loaded.torn_lines, 1);

    // 追加时会先给半行补换行（成为损坏完整行），再写新记录
    let d = record(4, "2026-11-16", "anthropic", "work", "claude-opus-5-5", toks(8, 1, 0, 0, 0));
    journal.append(&d).unwrap();
    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert!(loaded.records.contains(&a));
    assert!(loaded.records.contains(&c));
    assert!(loaded.records.contains(&d));
    assert!(loaded.corrupt_lines >= 1);

    // 损坏的完整行不阻断后面的记录
    let file = journal.dir().join("2026-11-17.jsonl");
    fs::write(&file, b"{\"not\":\"usage\"}\n{\"v\":1,\"ts\":5,\"time\":\"\",\"day\":\"2026-11-17\",\"provider\":\"anthropic\",\"account\":\"work\",\"model\":\"m\",\"caller\":\"x\",\"tokens\":{\"input\":1,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"reasoning\":0,\"totalTokens\":1}}\n").unwrap();
    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert!(loaded.records.iter().any(|r| r.ts == 5 && r.tokens.input == 1));
    assert!(loaded.corrupt_lines >= 1);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn journal_permissions_and_missing_dir() {
    let dir = tmp();
    let journal = Journal::new(dir.join(USAGE_DIR));
    assert!(super::load(journal.dir(), None, None).unwrap().records.is_empty());
    journal.append(&record(1, "2026-11-16", "anthropic", "work", "m", toks(1, 0, 0, 0, 0))).unwrap();
    let usage = journal.dir();
    let mode = fs::metadata(usage).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "dir mode {mode:o}");
    let file_mode = fs::metadata(usage.join("2026-11-16.jsonl")).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "file mode {file_mode:o}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_writers_do_not_lose_or_tear_records() {
    let dir = tmp();
    let path = dir.join(USAGE_DIR);
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let path = path.clone();
            std::thread::spawn(move || {
                let journal = Journal::new(path);
                for i in 0..50 {
                    let r = record(w * 100 + i, "2026-11-16", "anthropic", if w == 0 { "work" } else { "my" }, "m", toks(1, 0, 0, 0, 0));
                    journal.append(&r).unwrap();
                }
            })
        })
        .collect();
    for h in writers {
        h.join().unwrap();
    }
    let loaded = super::load(&path, None, None).unwrap();
    assert_eq!(loaded.records.len(), 100, "torn={} corrupt={}", loaded.torn_lines, loaded.corrupt_lines);
    assert_eq!(loaded.torn_lines, 0);
    assert_eq!(loaded.corrupt_lines, 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ranges_use_local_timezone_boundaries() {
    let tz = offset_hours(8);
    let now = tz.with_ymd_and_hms(2026, 11, 16, 15, 0, 0).unwrap();
    let today = RangeSpec::Today.resolve(&now).unwrap();
    assert_eq!(today.since.as_deref(), Some("2026-11-16T00:00:00+08:00"));
    assert_eq!(today.until.as_deref(), Some("2026-11-17T00:00:00+08:00"));
    // 15:00 +08 is 07:00 UTC; 23:30 +08 still today; 16:00 UTC is tomorrow in +08
    let start = today.since_ms.unwrap();
    let end = today.until_ms.unwrap();
    let inside = tz.with_ymd_and_hms(2026, 11, 16, 23, 30, 0).unwrap().timestamp_millis();
    let before = tz.with_ymd_and_hms(2026, 11, 15, 23, 59, 59).unwrap().timestamp_millis();
    let after = tz.with_ymd_and_hms(2026, 11, 17, 0, 0, 0).unwrap().timestamp_millis();
    assert!(today.contains(start));
    assert!(today.contains(inside));
    assert!(!today.contains(before));
    assert!(!today.contains(after));
    assert_eq!(end, after);

    let month = RangeSpec::Month.resolve(&now).unwrap();
    assert_eq!(month.since.as_deref(), Some("2026-11-01T00:00:00+08:00"));
    assert_eq!(month.until.as_deref(), Some("2026-12-01T00:00:00+08:00"));

    let days = RangeSpec::Days(3).resolve(&now).unwrap();
    assert_eq!(days.since.as_deref(), Some("2026-11-14T00:00:00+08:00"));
    assert_eq!(days.until.as_deref(), Some("2026-11-17T00:00:00+08:00"));
    assert_eq!(days.days, Some(3));

    let between = RangeSpec::Between { since: Some("2026-11-16".into()), until: Some("2026-11-16".into()) }.resolve(&now).unwrap();
    assert_eq!(between.since.as_deref(), Some("2026-11-16T00:00:00+08:00"));
    assert_eq!(between.until.as_deref(), Some("2026-11-17T00:00:00+08:00"));

    let rfc = RangeSpec::Between { since: Some("2026-11-16T08:00:00+08:00".into()), until: Some("2026-11-16T09:00:00+08:00".into()) }.resolve(&now).unwrap();
    assert!(rfc.contains(tz.with_ymd_and_hms(2026, 11, 16, 8, 30, 0).unwrap().timestamp_millis()));
    assert!(!rfc.contains(tz.with_ymd_and_hms(2026, 11, 16, 9, 0, 0).unwrap().timestamp_millis()));

    assert!(RangeSpec::from_flags(true, true, None, None, None).is_err());
    assert!(RangeSpec::from_flags(false, false, Some(0), None, None).is_err());
    assert!(RangeSpec::Between { since: Some("2026-11-17".into()), until: Some("2026-11-16".into()) }.resolve(&now).is_err());
}

fn builtin_config() -> PricingConfig {
    PricingConfig { source: PricingSource::Builtin, ..PricingConfig::default() }
}

fn builtin_book() -> PriceBook {
    PriceBook::build(&builtin_config())
}

#[test]
fn aggregation_groups_filters_and_empty_range() {
    let tz = offset_hours(8);
    let now = tz.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap();
    let day = "2026-11-16";
    let t1 = tz.with_ymd_and_hms(2026, 11, 16, 1, 0, 0).unwrap().timestamp_millis();
    let t2 = tz.with_ymd_and_hms(2026, 11, 16, 2, 0, 0).unwrap().timestamp_millis();
    let t3 = tz.with_ymd_and_hms(2026, 11, 15, 23, 0, 0).unwrap().timestamp_millis();
    let records = vec![
        rec(t1, day, "anthropic", "work", "claude-opus-5-5", toks(10, 5, 20, 3, 0), 1, false, false, Some("coder"), Some("s1"), "bot-a"),
        rec(t2, day, "anthropic", "my", "claude-opus-5-5", toks(2, 1, 0, 0, 0), 2, true, false, Some("coder"), Some("s2"), "bot-a"),
        rec(t2 + 1, day, "openai", "default", "gpt-6.1-sol", toks_openai(8, 8, 4, 0, 0, 3), 1, false, true, Some("reviewer"), Some("s3"), "bot-b"),
        rec(t3, "2026-11-15", "anthropic", "work", "claude-opus-5-5", toks(100, 0, 0, 0, 0), 1, false, false, Some("coder"), Some("s1"), "bot-a"),
    ];
    let range = RangeSpec::Today.resolve(&now).unwrap();
    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Account], filters: Filters::default() };
    let book = builtin_book();
    let (groups, total) = aggregate(&records, &range, &query, &tz, &book);
    assert_eq!(total.requests, 3);
    assert_eq!(total.tokens.input, 20);
    assert_eq!(total.retried, 1);
    assert_eq!(total.failed, 1);
    assert_eq!(groups.len(), 3);
    assert!(total.est_cost.is_some(), "official table should price opus-5-5 and gpt-6.1-sol");
    assert_eq!(total.unpriced_requests, Some(0));
    // uncached: anthro 10+2, openai 8-4=4 → 16; cacheRead 20+0+4=24; cacheWrite 3; hit = 24/43
    assert!((total.cache_hit_rate.unwrap() - 24.0 / 43.0).abs() < 1e-12);

    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Account, GroupBy::Model], filters: Filters { account: Some("anthropic/work".into()), ..Default::default() } };
    let range = query.range.resolve(&now).unwrap();
    let (groups, total) = aggregate(&records, &range, &query, &tz, &book);
    assert_eq!(total.requests, 1);
    assert_eq!(groups[0].key.get("account").unwrap(), "anthropic/work");
    assert_eq!(total.tokens.input, 10);

    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Day], filters: Filters::default() };
    let report = build(&query, &now, "Asia/Shanghai", None, &book).unwrap();
    assert_eq!(report.total.requests, 0);
    assert_eq!(report.timezone, "Asia/Shanghai");
    assert_eq!(report.groups.len(), 0);
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["total"].get("priceSource"), Some(&serde_json::Value::Null));
    assert_eq!(json["total"]["priceSources"], serde_json::json!({}));
    assert!(json["total"].get("estCost").is_none());
}

#[test]
fn estimate_costs_official_table_overrides_and_validation() {
    builtin_config().validate().unwrap();
    let pricing = builtin_book();
    let cost_of = |record| pricing.cost(record).1;

    // Official Opus 5.5 (2026-11-16): input $4, output $20, cacheRead $0.20, 5m write $5.
    // 100*4 + 50*0.20 + 20*5 + 10*20 = 400 + 10 + 100 + 200 = 710 / 1e6
    let anthro_toks = toks(100, 10, 50, 20, 0);
    assert_eq!(anthro_toks.cache_write_5m, 20);
    let anthro = record(1, "2026-11-16", "anthropic", "work", "claude-opus-5-5", anthro_toks);
    let cost = cost_of(&anthro).unwrap();
    assert!((cost.input_cost - 0.0004).abs() < 1e-12);
    assert!((cost.cache_read_cost - 0.000010).abs() < 1e-12);
    assert!((cost.cache_write_cost - 0.0001).abs() < 1e-12);
    assert!((cost.output_cost - 0.0002).abs() < 1e-12);
    assert!((cost.total() - 0.00071).abs() < 1e-12);

    // 1h writes use the 1h rate ($8 / MTok on opus 5.5).
    let split = Tokens { input: 0, output: 0, cache_write_5m: 0, cache_write_1h: 20, cache_write: 20, total_tokens: 20, ..Default::default() };
    let anthro_1h = record(1, "2026-11-16", "anthropic", "work", "claude-opus-5-5", split);
    assert!((cost_of(&anthro_1h).unwrap().cache_write_cost - 0.00016).abs() < 1e-12);

    // Official gpt-6.1-sol: input $2, cached $0.10, output $10. prompt=100 includes 40 cached, 0 writes.
    // uncached 60*2 + 40*0.10 + 80*10 = 120 + 4 + 800 = 924 / 1e6
    let openai = record(1, "2026-11-16", "openai", "default", "gpt-6.1-sol", toks_openai(100, 80, 40, 0, 0, 30));
    let cost = cost_of(&openai).unwrap();
    assert!((cost.input_cost - 0.00012).abs() < 1e-12);
    assert!((cost.cache_read_cost - 0.000004).abs() < 1e-12);
    assert!((cost.output_cost - 0.0008).abs() < 1e-12);
    assert!((cost.total() - 0.000924).abs() < 1e-12);
    // reasoning is already in output and is billed at the output rate (not added again).
    assert_eq!(cost.output_cost, 80.0 * 10.0 / 1e6);

    // OpenAI writes without a 30m/12h split cannot be priced (would invent a TTL).
    let unsplit = Tokens { input: 100, output: 0, cache_write: 10, total_tokens: 100, ..Default::default() };
    let openai_unsplit = record(1, "2026-11-16", "openai", "default", "gpt-6.1-sol", unsplit);
    assert!(cost_of(&openai_unsplit).is_none());

    // OpenAI 30m writes at $2.50 / MTok (gpt-6.1-sol short context).
    let with_write = record(1, "2026-11-16", "openai", "default", "gpt-6.1-sol", toks_openai(100, 0, 0, 20, 0, 0));
    // uncached = 100-20=80 * 2, write 20*2.50 → 160 + 50 = 210 / 1e6
    assert!((cost_of(&with_write).unwrap().total() - 0.000210).abs() < 1e-12);

    // Haiku 5.5 long-context tier: prompt over 100,000 tokens uses the higher rates.
    let short = record(1, "2026-11-16", "anthropic", "my", "claude-haiku-5-5", toks(100_000, 0, 0, 0, 0));
    assert!((cost_of(&short).unwrap().input_cost - 0.01).abs() < 1e-12); // 100k * 0.10 / 1e6
    let long = record(1, "2026-11-16", "anthropic", "my", "claude-haiku-5-5", toks(100_001, 0, 0, 0, 0));
    assert!((cost_of(&long).unwrap().input_cost - 0.0500005).abs() < 1e-12); // 100001 * 0.50 / 1e6

    // Snapshot suffix still matches the official row.
    let snap = record(1, "2026-11-16", "anthropic", "my", "claude-opus-5-5-20261101", toks(1_000_000, 0, 0, 0, 0));
    assert!((cost_of(&snap).unwrap().input_cost - 4.0).abs() < 1e-12);

    // Unknown model is unpriced (never invent a rate).
    let unknown = record(1, "2026-11-16", "anthropic", "my", "claude-secret-model", toks(10, 0, 0, 0, 0));
    assert!(cost_of(&unknown).is_none());

    // User override replaces the official row.
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "anthropic/claude-opus-5-5".into(),
        ModelPrice {
            input: 1.0,
            output: 2.0,
            cache_read: Some(0.1),
            cache_write_5m: Some(1.25),
            cache_write_1h: None,
            cache_write_30m: None,
            cache_write_12h: None,
            reasoning: None,
            long_context: None,
        },
    );
    let overridden = PriceBook::build(&PricingConfig { models, ..builtin_config() });
    let cost = overridden.cost(&anthro).1.unwrap();
    // 100*1 + 50*0.1 + 20*1.25 + 10*2 = 100 + 5 + 25 + 20 = 150 / 1e6
    assert!((cost.total() - 0.00015).abs() < 1e-12);

    let disabled = PriceBook::build(&PricingConfig { enabled: false, ..builtin_config() });
    assert!(disabled.cost(&anthro).1.is_none());

    let bad = UsageEstimateConfig {
        enabled: None,
        prices_usd_per_million: [(
            "opus".into(),
            ModelPrice { input: 1.0, output: 1.0, cache_read: None, cache_write_5m: None, cache_write_1h: None, cache_write_30m: None, cache_write_12h: None, reasoning: None, long_context: None },
        )]
        .into(),
    };
    assert!(PricingConfig::validate_legacy(&bad).unwrap_err().to_string().contains("provider/model"));

    // cache hit rate: Anthropic 50 / (100+50+20) = 50/170; OpenAI 40 / (60+40+0) = 40/100
    assert!((prices::cache_hit_rate("anthropic", &anthro.tokens).unwrap() - 50.0 / 170.0).abs() < 1e-12);
    assert!((prices::cache_hit_rate("openai", &openai.tokens).unwrap() - 0.4).abs() < 1e-12);
}

#[test]
fn old_records_without_cache_write_split_count_as_5m() {
    let dir = tmp();
    let path = dir.join(USAGE_DIR);
    std::fs::create_dir_all(&path).unwrap();
    // Pre-0.12.0 line: cacheWrite only.
    std::fs::write(path.join("2026-11-16.jsonl"), concat!(r#"{"v":1,"ts":1,"time":"","day":"2026-11-16","provider":"anthropic","account":"my","model":"claude-opus-5-5","caller":"local","attempt":1,"retry":false,"discarded":false,"failed":false,"tokens":{"input":0,"output":0,"cacheRead":0,"cacheWrite":15,"reasoning":0,"totalTokens":15}}"#, "\n")).unwrap();
    let loaded = super::load(&path, None, None).unwrap();
    assert_eq!(loaded.records[0].tokens.cache_write, 15);
    assert_eq!(loaded.records[0].tokens.cache_write_5m, 15);
    assert_eq!(loaded.records[0].tokens.cache_write_1h, 0);
    assert_eq!(builtin_book().cost(&loaded.records[0]).1.unwrap().cache_write_cost, 0.000075);
    fs::remove_dir_all(dir).unwrap();
}

struct Scripted {
    events: Vec<AssistantEvent>,
}

impl Provider for Scripted {
    fn id(&self) -> &str {
        "anthropic"
    }
    fn list_models(&self) -> futures::future::BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn stream(&self, _req: Request, _cancel: CancellationToken) -> EventStream {
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage { input, output, cache_read, cache_write, cache_write_5m: cache_write, total_tokens: input + output + cache_read + cache_write, reasoning: 0, ..Default::default() }
}

fn done(model: &str, u: Usage, failed: bool) -> AssistantEvent {
    let mut message = AssistantMessage::empty(model);
    message.usage = u;
    if failed {
        message.stop_reason = StopReason::Error;
        return AssistantEvent::Error { message };
    }
    AssistantEvent::Done { message }
}

fn req(model: &str) -> Request {
    Request {
        model: model.into(),
        system_prompt: None,
        messages: vec![],
        tools: vec![],
        reasoning: None,
        raw_effort: false,
        thinking: None,
        api: Default::default(),
        max_tokens: None,
        llm_max_retries: None,
        stream_idle_timeout: None,
    }
}

#[tokio::test]
async fn recording_provider_writes_retry_failure_and_skips_empty_usage() {
    let dir = tmp();
    let journal = Arc::new(Journal::new(dir.join(USAGE_DIR)));
    let meta = Arc::new(UsageMeta { caller: "local".into(), profile: Some("coder".into()), session_id: Some("sess-9".into()) });
    let discarded = usage(4, 1, 2, 3);
    let success = usage(10, 5, 0, 0);
    let failed = usage(7, 0, 0, 0);
    let events = vec![
        AssistantEvent::Start,
        AssistantEvent::Retry {
            attempt: 1,
            reason: "stream idle timeout after 1s".into(),
            max_retries: 3,
            delay_ms: 10,
            kind: "idle_timeout".into(),
            discarded_tool_calls: 1,
            usage: Some(discarded.clone()),
        },
        done("claude-opus-5-5", success.clone(), false),
    ];
    let provider = RecordingProvider::new(Arc::new(Scripted { events }), AccountKey::new("anthropic", "work"), meta.clone(), journal.clone());
    let mut stream = provider.stream(req("claude-opus-5-5"), CancellationToken::new());
    while stream.next().await.is_some() {}

    // 第二次调用：失败但有用量；第三次：没有用量（不记）
    let provider = RecordingProvider::new(
        Arc::new(Scripted {
            events: vec![
                done("claude-opus-5-5", failed.clone(), true),
                AssistantEvent::Retry { attempt: 1, reason: "connect".into(), max_retries: 1, delay_ms: 0, kind: "connect".into(), discarded_tool_calls: 0, usage: None },
                done("claude-opus-5-5", Usage::default(), false),
            ],
        }),
        AccountKey::new("anthropic", "my"),
        meta,
        journal.clone(),
    );
    let mut stream = provider.stream(req("claude-opus-5-5"), CancellationToken::new());
    while stream.next().await.is_some() {}

    let loaded = super::load(journal.dir(), None, None).unwrap();
    assert_eq!(loaded.records.len(), 3, "{:?}", loaded.records);
    let work: Vec<_> = loaded.records.iter().filter(|r| r.account == "work").collect();
    assert_eq!(work.len(), 2);
    assert!(work[0].discarded && work[0].attempt == 1 && work[0].tokens.input == 4 && work[0].tokens.cache_write == 3);
    assert!(!work[1].discarded && !work[1].failed && work[1].attempt == 2 && work[1].retry && work[1].tokens.input == 10);
    assert_eq!(work[0].session_id.as_deref(), Some("sess-9"));
    assert_eq!(work[0].caller, "local");
    assert_eq!(work[0].account_label(), "anthropic/work");
    let mine: Vec<_> = loaded.records.iter().filter(|r| r.account == "my").collect();
    assert_eq!(mine.len(), 1);
    assert!(mine[0].failed && mine[0].tokens.input == 7);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_usage_dir_builds_empty_report() {
    let tz = offset_hours(-6);
    let now = tz.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap();
    let query = Query { range: RangeSpec::Today, by: GroupBy::parse_list(&["account"]).unwrap(), filters: Filters::default() };
    let report = build(&query, &now, "America/Winnipeg", None, &builtin_book()).unwrap();
    assert_eq!(report.total.requests, 0);
    assert_eq!(report.groups.len(), 0);
}

fn with_live(pricing: &PricingConfig, feed: &[u8]) -> PriceBook {
    let builtins = prices::official_prices();
    let rows = parse_models_dev(feed)
        .unwrap()
        .models
        .into_iter()
        .map(|(key, row)| {
            let bands = compile_row(&builtins, &key, &row).unwrap();
            (key, bands)
        })
        .collect();
    PriceBook::build(pricing).with_live("2026-11-17T12:00:00Z".parse().unwrap(), rows)
}

#[test]
fn resolver_prioritizes_source_before_snapshot_alias() {
    let cfg = serde_json::from_str(r#"{"models":{"openai/foo":{"input":2,"output":3}}}"#).unwrap();
    let pricing = with_live(
        &cfg,
        br#"{"openai":{"models":{
      "foo-20261117":{"cost":{"input":9,"output":10,"cache_write":11}},
      "bar":{"cost":{"input":2,"output":3}},
      "bar-20261117":{"cost":{"input":7,"output":8}},
      "bar-2026-11-17":{"cost":{"input":8,"output":10}},
      "literal-alias":{"id":"unusable","name":"Not a model id","cost":{"input":1,"output":2}}
    }}}"#,
    );
    for (model, stored, source, price) in [
        ("foo-20261117", "openai/foo", PriceSource::Override, 2.0),
        ("bar-20261117", "openai/bar-20261117", PriceSource::ModelsDev, 7.0),
        ("bar-20261118", "openai/bar", PriceSource::ModelsDev, 2.0),
        ("bar-2026-11-17", "openai/bar-2026-11-17", PriceSource::ModelsDev, 8.0),
        ("literal-alias", "openai/literal-alias", PriceSource::ModelsDev, 1.0),
        ("gpt-6.2-sol-20261117", "openai/gpt-6.2-sol", PriceSource::Builtin, 2.0),
    ] {
        let found = pricing.lookup("openai", model).unwrap();
        assert_eq!((found.matched_key, found.source, found.bands.base.input), (stored, source, price));
    }
    for unknown in ["unusable", "Not a model id", "bar-2026-11-18", "bar-1117", "bar-２０２６１１１７", "latest:bar"] {
        assert!(pricing.lookup("openai", unknown).is_none(), "{unknown}");
    }
    assert!(pricing.lookup("anthropic", "bar").is_none());
    let used = |model| record(1, "2026-11-17", "openai", "a", model, toks_openai(100, 5, 0, 1, 0, 0));
    assert_eq!(pricing.cost(&used("foo-20261117")), (PriceSource::Override, None)); // no live field merge
    assert_eq!(pricing.cost(&used("no-model")), (PriceSource::Unknown, None));
}

#[test]
fn context_cutoffs_are_strict_and_apply_per_call_to_billed_prompt_only() {
    let pricing = with_live(&PricingConfig::default(), include_bytes!("../../tests/fixtures/models-dev-pricing.json"));
    let at = |provider, model, tokens| record(1, "2026-11-17", provider, "a", model, tokens);
    let near = |actual: f64, expected: f64| assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    for prompt in [99_999, 100_000, 100_001] {
        let (input, output, cache_read, write) = if prompt > 100_000 { (0.5, 2.5, 0.05, 0.625) } else { (0.1, 0.5, 0.01, 0.125) };
        let r = at("anthropic", "claude-haiku-5-5", toks(prompt - 2, 8, 1, 1, 0));
        let (source, cost) = pricing.cost(&r);
        let cost = cost.unwrap();
        assert_eq!(source, PriceSource::ModelsDev);
        near(cost.input_cost, (prompt - 2) as f64 * input / 1e6);
        near(cost.output_cost, 8.0 * output / 1e6);
        near(cost.cache_read_cost, cache_read / 1e6);
        near(cost.cache_write_cost, write / 1e6);
    }
    for prompt in [271_999, 272_000, 272_001] {
        let (input, output, cache_read, write) = if prompt > 272_000 { (4., 15., 0.2, 5.) } else { (2., 10., 0.1, 2.5) };
        let r = at("openai", "gpt-6.2-sol", toks_openai(prompt, 300_000, 1, 1, 0, 400_000));
        let (source, cost) = pricing.cost(&r);
        let cost = cost.unwrap();
        assert_eq!(source, PriceSource::ModelsDev);
        near(cost.input_cost, (prompt - 2) as f64 * input / 1e6);
        near(cost.output_cost, 300_000.0 * output / 1e6);
        near(cost.cache_read_cost, cache_read / 1e6);
        near(cost.cache_write_cost, write / 1e6);
    }

    let tz = offset_hours(0);
    let now = tz.timestamp_millis_opt(1).unwrap();
    let query = Query { by: vec![GroupBy::Model], ..Default::default() };
    let range = query.range.resolve(&now).unwrap();
    let short = at("openai", "gpt-6.2-sol", toks_openai(200_000, 0, 0, 0, 0, 0));
    let (groups, total) = aggregate(&[short.clone(), short], &range, &query, &tz, &pricing);
    assert_eq!(groups[0].totals.est_cost, Some(0.8));
    assert_eq!(total.est_cost, Some(0.8));
}

#[test]
fn missing_buckets_never_inherit_live_rates_or_retry_a_lower_source() {
    let book = with_live(
        &PricingConfig::default(),
        br#"{"openai":{"models":{"gpt-6.2-sol":{"cost":{
      "input":2,"output":10,"cache_read":0.1,"cache_write":2.5,
      "tiers":[{"tier":{"type":"context","size":272000},"input":4,"output":15}]
    }}}}}"#,
    );
    let price = |t| book.cost(&record(1, "2026-11-17", "openai", "a", "gpt-6.2-sol", t));
    for tokens in [
        toks_openai(300_000, 0, 1, 0, 0, 0),                          // cache read absent in tier
        toks_openai(300_000, 0, 0, 1, 0, 0),                          // cache write absent in tier
        toks_openai(1, 0, 0, 0, 1, 0),                                // 12h never known for this model
        Tokens { input: 100, cache_write: 20, ..Default::default() }, // unsplit
    ] {
        assert_eq!(price(tokens), (PriceSource::ModelsDev, None));
    }
    assert_eq!(price(toks_openai(300_000, 0, 0, 0, 0, 0)).1.unwrap().total(), 1.2);
}
