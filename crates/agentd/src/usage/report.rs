//! 用量报表：时间范围、过滤、分组聚合与可选的估测费用。CLI `sub-bot usage` 与 HTTP `GET /v1/usage` 共用，
//! 输出同一个 JSON 结构（[`Report`]）。时间计算对时区泛型（生产用 `chrono::Local`，测试用固定偏移）。
use super::{Tokens, UsageRecord};
use anyhow::{bail, ensure, Result};
use chrono::{DateTime, Datelike, Days, Months, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `--days` / `days` 的上限（约 100 年）。
pub const MAX_DAYS: u32 = 36_600;

/// 时间范围选择（互斥）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RangeSpec {
    /// 本地今天（默认）
    #[default]
    Today,
    /// 本地本月
    Month,
    /// 最近 N 个本地自然日（含今天）
    Days(u32),
    /// RFC 3339 或 `YYYY-MM-DD`（本地日期；until 的日期包含当天）；省略 = 不设边界
    Between { since: Option<String>, until: Option<String> },
}

impl RangeSpec {
    /// 从各个开关组合；多于一种 ⇒ 错误。
    pub fn from_flags(today: bool, month: bool, days: Option<u32>, since: Option<String>, until: Option<String>) -> Result<Self> {
        let between = since.is_some() || until.is_some();
        let chosen = usize::from(today) + usize::from(month) + usize::from(days.is_some()) + usize::from(between);
        ensure!(chosen <= 1, "conflicting time ranges: choose only one of today, month, days, since/until");
        if let Some(n) = days {
            ensure!((1..=MAX_DAYS).contains(&n), "days must be between 1 and {MAX_DAYS}");
            return Ok(Self::Days(n));
        }
        Ok(match (today, month, between) {
            (_, true, _) => Self::Month,
            (_, _, true) => Self::Between { since, until },
            _ => Self::Today,
        })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Month => "month",
            Self::Days(_) => "days",
            Self::Between { .. } => "between",
        }
    }
}

/// 解析后的范围：`[since, until)`，Unix 毫秒；None = 不设边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub kind: &'static str,
    pub days: Option<u32>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// RFC 3339（报表时区）
    pub since: Option<String>,
    pub until: Option<String>,
}

/// 本地日期的开始时刻。午夜不存在（夏令时跳变）时取当天最早的有效整点。
pub fn day_start<Tz: TimeZone>(tz: &Tz, day: NaiveDate) -> DateTime<Tz> {
    (0..=3).find_map(|h| day.and_hms_opt(h, 0, 0).and_then(|t| tz.from_local_datetime(&t).earliest())).unwrap_or_else(|| tz.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight")))
}

/// RFC 3339 时刻，或 `YYYY-MM-DD` 本地日期（`end` = 该日期的次日开始，使 until 包含整天）。
fn parse_bound<Tz: TimeZone>(tz: &Tz, name: &str, value: &str, end: bool) -> Result<i64> {
    let value = value.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(value) {
        return Ok(t.timestamp_millis());
    }
    match NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        Ok(day) if value.len() == 10 => {
            let day = if end { day.checked_add_days(Days::new(1)).ok_or_else(|| anyhow::anyhow!("{name} is out of range"))? } else { day };
            Ok(day_start(tz, day).timestamp_millis())
        }
        _ => bail!("invalid {name} '{value}': expected RFC 3339 (e.g. 2026-11-16T08:00:00+08:00) or a local date YYYY-MM-DD"),
    }
}

fn rfc3339<Tz: TimeZone>(tz: &Tz, ms: i64) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    tz.timestamp_millis_opt(ms).single().map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

impl RangeSpec {
    /// 以 `now`（及其时区）解析为具体区间。
    pub fn resolve<Tz: TimeZone>(&self, now: &DateTime<Tz>) -> Result<Range>
    where
        Tz::Offset: std::fmt::Display,
    {
        let tz = now.timezone();
        let today = now.date_naive();
        let tomorrow = today.checked_add_days(Days::new(1)).ok_or_else(|| anyhow::anyhow!("date out of range"))?;
        let (since_ms, until_ms, days) = match self {
            Self::Today => (Some(day_start(&tz, today).timestamp_millis()), Some(day_start(&tz, tomorrow).timestamp_millis()), None),
            Self::Month => {
                let first = today.with_day(1).expect("first day of month");
                let next = first.checked_add_months(Months::new(1)).ok_or_else(|| anyhow::anyhow!("date out of range"))?;
                (Some(day_start(&tz, first).timestamp_millis()), Some(day_start(&tz, next).timestamp_millis()), None)
            }
            Self::Days(n) => {
                let first = today.checked_sub_days(Days::new(u64::from(n.saturating_sub(1)))).ok_or_else(|| anyhow::anyhow!("days is out of range"))?;
                (Some(day_start(&tz, first).timestamp_millis()), Some(day_start(&tz, tomorrow).timestamp_millis()), Some(*n))
            }
            Self::Between { since, until } => {
                let since_ms = since.as_deref().map(|v| parse_bound(&tz, "since", v, false)).transpose()?;
                let until_ms = until.as_deref().map(|v| parse_bound(&tz, "until", v, true)).transpose()?;
                if let (Some(s), Some(u)) = (since_ms, until_ms) {
                    ensure!(s < u, "since must be earlier than until");
                }
                (since_ms, until_ms, None)
            }
        };
        Ok(Range { kind: self.kind(), days, since_ms, until_ms, since: since_ms.and_then(|ms| rfc3339(&tz, ms)), until: until_ms.and_then(|ms| rfc3339(&tz, ms)) })
    }
}

impl Range {
    pub fn contains(&self, ts: i64) -> bool {
        self.since_ms.is_none_or(|s| ts >= s) && self.until_ms.is_none_or(|u| ts < u)
    }

    /// 需要读取的日文件范围（按报表时区的日期 ±1 天，容忍写入时与现在的时区不同；记录再按时间戳精确过滤）。
    pub fn file_days<Tz: TimeZone>(&self, tz: &Tz) -> (Option<NaiveDate>, Option<NaiveDate>) {
        let day = |ms: i64| tz.timestamp_millis_opt(ms).single().map(|t| t.date_naive());
        let from = self.since_ms.and_then(day).and_then(|d| d.checked_sub_days(Days::new(1)));
        let to = self.until_ms.and_then(day).and_then(|d| d.checked_add_days(Days::new(1)));
        (from, to)
    }
}

/// 分组维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    /// `provider/account`
    Account,
    Model,
    Profile,
    Session,
    /// 报表时区的本地日期
    Day,
    /// client 名
    Caller,
}

impl GroupBy {
    pub const NAMES: &'static str = "account, model, profile, session, day, caller";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Model => "model",
            Self::Profile => "profile",
            Self::Session => "session",
            Self::Day => "day",
            Self::Caller => "caller",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "account" => Self::Account,
            "model" => Self::Model,
            "profile" => Self::Profile,
            "session" => Self::Session,
            "day" => Self::Day,
            "caller" => Self::Caller,
            other => bail!("invalid by '{other}': expected one of {}", Self::NAMES),
        })
    }

    /// 可重复、可逗号分隔；去重保序。未指定 ⇒ 默认按账号。
    pub fn parse_list<S: AsRef<str>>(values: &[S]) -> Result<Vec<Self>> {
        let mut out = vec![];
        for part in values.iter().flat_map(|v| v.as_ref().split(',')).map(str::trim) {
            ensure!(!part.is_empty(), "by must not contain empty entries");
            let by = Self::parse(part)?;
            if !out.contains(&by) {
                out.push(by);
            }
        }
        if out.is_empty() {
            out.push(Self::Account);
        }
        Ok(out)
    }

    fn key<Tz: TimeZone>(self, record: &UsageRecord, tz: &Tz) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        match self {
            Self::Account => record.account_label(),
            Self::Model => record.model.clone(),
            Self::Profile => record.profile.clone().unwrap_or_else(|| "-".into()),
            Self::Session => record.session_id.clone().unwrap_or_else(|| "-".into()),
            Self::Day => tz.timestamp_millis_opt(record.ts).single().map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "-".into()),
            Self::Caller => record.caller.clone(),
        }
    }
}

/// 精确匹配过滤（全部满足）。`account` 可写 `provider/account` 或只写账号名。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

impl Filters {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [("account", &self.account), ("provider", &self.provider), ("model", &self.model), ("profile", &self.profile), ("session", &self.session), ("caller", &self.caller)] {
            if let Some(v) = value {
                ensure!(!v.trim().is_empty(), "{name} filter must not be empty");
            }
        }
        Ok(())
    }

    pub fn matches(&self, r: &UsageRecord) -> bool {
        let eq = |filter: &Option<String>, value: &str| filter.as_deref().is_none_or(|f| f == value);
        let account = self.account.as_deref().is_none_or(|f| if f.contains('/') { f == r.account_label() } else { f == r.account });
        account
            && eq(&self.provider, &r.provider)
            && eq(&self.model, &r.model)
            && self.profile.as_deref().is_none_or(|f| r.profile.as_deref() == Some(f))
            && self.session.as_deref().is_none_or(|f| r.session_id.as_deref() == Some(f))
            && eq(&self.caller, &r.caller)
    }
}

/// 配置 `usageEstimate`：按 `provider/model` 的单价（美元 / 百万 token）估测费用。默认关闭。只是估测，
/// 不代表账单（不含批量/区域/阶梯定价、长上下文加价等）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageEstimateConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub prices_usd_per_million: BTreeMap<String, ModelPrice>,
}

/// 单价（美元 / 百万 token）。`input` / `output` 必填；`cacheRead`（别名 `cached`）与 `cacheWrite` 默认按 `input`，
/// `reasoning` 默认按 `output`（推理 token 包含在 output 里，设置后这部分改按该价计）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, alias = "cached")]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub reasoning: Option<f64>,
}

impl UsageEstimateConfig {
    pub fn validate(&self, at: &str) -> Result<()> {
        for (key, price) in &self.prices_usd_per_million {
            let valid_key = key.split_once('/').is_some_and(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty());
            ensure!(valid_key, "{at}.pricesUsdPerMillion: key '{key}' must be 'provider/model'");
            for (name, value) in [("input", Some(price.input)), ("output", Some(price.output)), ("cacheRead", price.cache_read), ("cacheWrite", price.cache_write), ("reasoning", price.reasoning)] {
                if let Some(v) = value {
                    ensure!(v.is_finite() && v >= 0.0, "{at}.pricesUsdPerMillion.{key}.{name} must be a non-negative number");
                }
            }
        }
        Ok(())
    }

    /// 一条记录的估测费用（美元）；关闭或该模型没有单价 ⇒ None。
    pub fn cost(&self, r: &UsageRecord) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        let price = self.prices_usd_per_million.get(&format!("{}/{}", r.provider, r.model))?;
        let t = &r.tokens;
        // OpenAI 的 input（prompt_tokens）包含缓存命中部分；Anthropic 的 input_tokens 不含缓存读写。
        let uncached = if r.provider == "anthropic" { t.input } else { t.input.saturating_sub(t.cache_read) };
        let reasoning = t.reasoning.min(t.output);
        let cost = uncached as f64 * price.input
            + t.cache_read as f64 * price.cache_read.unwrap_or(price.input)
            + t.cache_write as f64 * price.cache_write.unwrap_or(price.input)
            + (t.output - reasoning) as f64 * price.output
            + reasoning as f64 * price.reasoning.unwrap_or(price.output);
        Some(cost / 1_000_000.0)
    }
}

/// 一组（或总计）的合计。`requests` = 记录的尝试数（含被丢弃的重试尝试）；`retried` = 其中被丢弃并重试的；
/// `failed` = 以错误结束的。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    #[serde(flatten)]
    pub tokens: Tokens,
    pub retried: u64,
    pub failed: u64,
    /// 估测费用（美元）；只在 usageEstimate 开启且本组每条记录的模型都有单价时出现
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    /// usageEstimate 开启时：本组没有单价的记录数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpriced_requests: Option<u64>,
    #[serde(skip)]
    cost: f64,
    #[serde(skip)]
    unpriced: u64,
}

impl Totals {
    fn add(&mut self, r: &UsageRecord, cost: Option<f64>) {
        self.requests += 1;
        self.tokens.add(&r.tokens);
        self.retried += u64::from(r.discarded);
        self.failed += u64::from(r.failed);
        match cost {
            Some(c) => self.cost += c,
            None => self.unpriced += 1,
        }
    }

    fn finish(&mut self, estimate: bool) {
        if estimate {
            self.unpriced_requests = Some(self.unpriced);
            self.estimated_cost_usd = (self.unpriced == 0).then(|| (self.cost * 1e6).round() / 1e6);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    /// 维度名 → 值（JSON 对象按键名排序；维度顺序见报表的 `by`）
    pub key: BTreeMap<&'static str, String>,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeOut {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimateNote {
    pub currency: &'static str,
    pub note: &'static str,
}

/// CLI `--json` 与 HTTP `GET /v1/usage` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub range: RangeOut,
    /// IANA 时区名（取不到时为 UTC 偏移）
    pub timezone: String,
    pub utc_offset: String,
    pub by: Vec<GroupBy>,
    pub filters: Filters,
    pub groups: Vec<Group>,
    pub total: Totals,
    /// 跳过的损坏行数
    pub skipped_lines: usize,
    /// 估测费用说明（usageEstimate 开启时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<EstimateNote>,
}

/// 一次查询。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub range: RangeSpec,
    pub by: Vec<GroupBy>,
    pub filters: Filters,
}

/// 聚合内存中的记录（范围、过滤已在此完成）。
pub fn aggregate<Tz: TimeZone>(records: &[UsageRecord], range: &Range, query: &Query, tz: &Tz, estimate: &UsageEstimateConfig) -> (Vec<Group>, Totals)
where
    Tz::Offset: std::fmt::Display,
{
    let mut groups: BTreeMap<Vec<String>, Totals> = BTreeMap::new();
    let mut total = Totals::default();
    for r in records.iter().filter(|r| range.contains(r.ts) && query.filters.matches(r)) {
        let cost = estimate.cost(r);
        let key: Vec<String> = query.by.iter().map(|b| b.key(r, tz)).collect();
        groups.entry(key).or_default().add(r, cost);
        total.add(r, cost);
    }
    total.finish(estimate.enabled);
    let groups = groups
        .into_iter()
        .map(|(values, mut totals)| {
            totals.finish(estimate.enabled);
            Group { key: query.by.iter().map(|b| b.as_str()).zip(values).collect(), totals }
        })
        .collect();
    (groups, total)
}

/// 解析范围、读取日文件（`dir` 为 None ⇒ 没有记录）、聚合。
pub fn build<Tz: TimeZone>(query: &Query, now: &DateTime<Tz>, timezone: &str, dir: Option<&std::path::Path>, estimate: &UsageEstimateConfig) -> Result<Report>
where
    Tz::Offset: std::fmt::Display,
{
    query.filters.validate()?;
    let range = query.range.resolve(now)?;
    let tz = now.timezone();
    let loaded = match dir {
        Some(dir) => {
            let (from, to) = range.file_days(&tz);
            super::load(dir, from, to)?
        }
        None => super::Loaded::default(),
    };
    let (groups, total) = aggregate(&loaded.records, &range, query, &tz, estimate);
    Ok(Report {
        range: RangeOut { kind: range.kind, days: range.days, since: range.since.clone(), until: range.until.clone() },
        timezone: timezone.to_string(),
        utc_offset: now.offset().to_string(),
        by: query.by.clone(),
        filters: query.filters.clone(),
        groups,
        total,
        skipped_lines: loaded.corrupt_lines,
        estimate: estimate.enabled.then_some(EstimateNote { currency: "USD", note: "estimate (估测) from usageEstimate.pricesUsdPerMillion; not a bill" }),
    })
}

/// 本机时区名：IANA 名（如 Asia/Shanghai），取不到时用 UTC 偏移。
pub fn local_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| chrono::Local::now().offset().to_string())
}

/// 用本机时区生成报表。
pub fn build_local(query: &Query, dir: Option<&std::path::Path>, estimate: &UsageEstimateConfig) -> Result<Report> {
    build(query, &chrono::Local::now(), &local_timezone_name(), dir, estimate)
}
