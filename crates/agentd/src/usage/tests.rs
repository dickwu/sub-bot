//! 用量日志、聚合、估测费用与 RecordingProvider 的测试。
use super::{
    report::{aggregate, build, Filters, GroupBy, ModelPrice, Query, RangeSpec, UsageEstimateConfig},
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
    Tokens { input, output, cache_read, cache_write, reasoning, total_tokens: input + output + cache_read + cache_write }
}

fn offset_hours(h: i32) -> FixedOffset {
    FixedOffset::east_opt(h * 3600).unwrap()
}

#[test]
fn journal_appends_survives_reopen_and_ignores_torn_and_corrupt_lines() {
    let dir = tmp();
    let journal = Journal::new(dir.join(USAGE_DIR));
    let a = record(1, "2026-11-16", "anthropic", "work", "claude-opus-5-5", toks(10, 2, 0, 0, 0));
    let b = record(2, "2026-11-16", "openai", "default", "gpt-6.1-sol", toks(3, 4, 1, 0, 2));
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
        rec(t2 + 1, day, "openai", "default", "gpt-6.1-sol", toks(8, 8, 4, 0, 3), 1, false, true, Some("reviewer"), Some("s3"), "bot-b"),
        rec(t3, "2026-11-15", "anthropic", "work", "claude-opus-5-5", toks(100, 0, 0, 0, 0), 1, false, false, Some("coder"), Some("s1"), "bot-a"),
    ];
    let range = RangeSpec::Today.resolve(&now).unwrap();
    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Account], filters: Filters::default() };
    let (groups, total) = aggregate(&records, &range, &query, &tz, &UsageEstimateConfig::default());
    assert_eq!(total.requests, 3);
    assert_eq!(total.tokens.input, 20);
    assert_eq!(total.retried, 1);
    assert_eq!(total.failed, 1);
    assert_eq!(groups.len(), 3);
    assert_eq!(total.estimated_cost_usd, None);

    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Account, GroupBy::Model], filters: Filters { account: Some("anthropic/work".into()), ..Default::default() } };
    let range = query.range.resolve(&now).unwrap();
    let (groups, total) = aggregate(&records, &range, &query, &tz, &UsageEstimateConfig::default());
    assert_eq!(total.requests, 1);
    assert_eq!(groups[0].key.get("account").unwrap(), "anthropic/work");
    assert_eq!(total.tokens.input, 10);

    let query = Query { range: RangeSpec::Today, by: vec![GroupBy::Day], filters: Filters::default() };
    let report = build(&query, &now, "Asia/Shanghai", None, &UsageEstimateConfig::default()).unwrap();
    assert_eq!(report.total.requests, 0);
    assert_eq!(report.timezone, "Asia/Shanghai");
    assert_eq!(report.groups.len(), 0);
}

#[test]
fn estimate_costs_and_config_validation() {
    let mut prices = std::collections::BTreeMap::new();
    prices.insert("anthropic/claude-opus-5-5".into(), ModelPrice { input: 5.0, output: 25.0, cache_read: Some(0.5), cache_write: Some(6.25), reasoning: None });
    prices.insert("openai/gpt-6.1-sol".into(), ModelPrice { input: 2.0, output: 10.0, cache_read: Some(0.2), cache_write: None, reasoning: Some(10.0) });
    let estimate = UsageEstimateConfig { enabled: true, prices_usd_per_million: prices };
    estimate.validate("usageEstimate").unwrap();

    // Anthropic: input excludes cache; 100 uncached * 5 + 50 cacheRead * 0.5 + 20 cacheWrite * 6.25 + 10 output * 25
    // = 500 + 25 + 125 + 250 = 900 / 1e6
    let anthro = record(1, "2026-11-16", "anthropic", "work", "claude-opus-5-5", toks(100, 10, 50, 20, 0));
    assert!((estimate.cost(&anthro).unwrap() - 0.0009).abs() < 1e-12);

    // OpenAI: input includes cache; uncached = 100-40=60 * 2 + 40*0.2 + (80-30)*10 + 30*10
    // = 120 + 8 + 500 + 300 = 928 / 1e6
    let openai = record(1, "2026-11-16", "openai", "default", "gpt-6.1-sol", toks(100, 80, 40, 0, 30));
    assert!((estimate.cost(&openai).unwrap() - 0.000928).abs() < 1e-12);

    let disabled = UsageEstimateConfig { enabled: false, prices_usd_per_million: estimate.prices_usd_per_million.clone() };
    assert!(disabled.cost(&anthro).is_none());

    let bad = UsageEstimateConfig { enabled: false, prices_usd_per_million: [("opus".into(), ModelPrice { input: 1.0, output: 1.0, ..Default::default() })].into() };
    assert!(bad.validate("usageEstimate").unwrap_err().to_string().contains("provider/model"));
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
    Usage { input, output, cache_read, cache_write, total_tokens: input + output + cache_read + cache_write, reasoning: 0 }
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
    let report = build(&query, &now, "America/Winnipeg", None, &UsageEstimateConfig::default()).unwrap();
    assert_eq!(report.total.requests, 0);
    assert_eq!(report.groups.len(), 0);
}
