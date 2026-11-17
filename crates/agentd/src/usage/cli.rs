//! CLI `sub-bot usage` / `pricing` use partial config; no Hub or provider credentials.
//! `GET /v1/usage` uses the same report and a read-only snapshot from the Hub.
use super::catalog::{CacheInfo, ContextBand, PriceBook, PriceSource, RateBand};
use super::price_store::{PriceStore, RefreshUse};
use super::prices::split_price_key;
use super::report::{build_local, Filters, GroupBy, PricingConfig, Query, RangeSpec, Report, Totals, UsageEstimateConfig, MAX_DAYS};
use anyhow::{Context, Result};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `sub-bot usage` 参数。
#[derive(Debug, clap::Args)]
pub struct Usage {
    /// 今天（本机时区；默认）
    #[arg(long)]
    pub today: bool,
    /// 本月（本机时区）
    #[arg(long)]
    pub month: bool,
    /// 最近 N 个本机自然日（含今天）
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=MAX_DAYS as i64))]
    pub days: Option<u32>,
    /// 起始：RFC 3339 或本机日期 YYYY-MM-DD
    #[arg(long)]
    pub since: Option<String>,
    /// 结束：RFC 3339，或本机日期 YYYY-MM-DD（含当天）
    #[arg(long)]
    pub until: Option<String>,
    /// 分组：account（provider/account）、model、profile、session、day、caller。可重复或逗号分隔；默认按账号
    #[arg(long, value_name = "DIM")]
    pub by: Vec<String>,
    /// 输出 JSON
    #[arg(long)]
    pub json: bool,
    /// 强制尝试刷新公开价格目录（仍遵守 pricing 设置）
    #[arg(long)]
    pub refresh_prices: bool,
    /// 过滤账号（`provider/account` 或账号名）
    #[arg(long)]
    pub account: Option<String>,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub session: Option<String>,
    /// 调用方 client 名
    #[arg(long)]
    pub caller: Option<String>,
}

impl Usage {
    pub fn query(&self) -> Result<Query> {
        query(
            self.today,
            self.month,
            self.days,
            self.since.clone(),
            self.until.clone(),
            &self.by,
            filters(self.account.clone(), self.provider.clone(), self.model.clone(), self.profile.clone(), self.session.clone(), self.caller.clone()),
        )
    }
}

#[derive(Debug, clap::Args)]
pub struct Pricing {
    /// 查看一个 provider/model；省略则列出整个价格目录
    #[arg(value_name = "PROVIDER/MODEL", value_parser = parse_price_key)]
    pub key: Option<String>,
    /// 忽略缓存的新鲜度，尝试重新下载（builtin 模式仍离线）
    #[arg(long)]
    pub refresh: bool,
    /// 输出 JSON
    #[arg(long)]
    pub json: bool,
}

fn parse_price_key(key: &str) -> Result<String, &'static str> {
    split_price_key(key).map(|_| key.to_owned()).ok_or("expected provider/model")
}

fn filters(account: Option<String>, provider: Option<String>, model: Option<String>, profile: Option<String>, session: Option<String>, caller: Option<String>) -> Filters {
    Filters { account, provider, model, profile, session, caller }
}

fn query(today: bool, month: bool, days: Option<u32>, since: Option<String>, until: Option<String>, by: &[String], filters: Filters) -> Result<Query> {
    Ok(Query { range: RangeSpec::from_flags(today, month, days, since, until)?, by: GroupBy::parse_list(by)?, filters })
}

/// 只读配置里的 dataDir 与 pricing，不解析密钥文件、不校验整个配置。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageFile {
    data_dir: Option<PathBuf>,
    #[serde(default)]
    pricing: PricingConfig,
    #[serde(default)]
    usage_estimate: UsageEstimateConfig,
}

/// 解析 dataDir（`--data-dir` > 配置 `dataDir` > 默认数据目录）与估测配置。
pub fn resolve(config_path: Option<&Path>, data_dir: Option<PathBuf>) -> Result<(PathBuf, PricingConfig)> {
    let file = match config_path {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("cannot read configuration {}", path.display()))?;
            let file: UsageFile = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid configuration: {e}"))?;
            file.pricing.validate("pricing")?;
            PricingConfig::validate_legacy(&file.usage_estimate, "usageEstimate.pricesUsdPerMillion")?;
            file
        }
        None => UsageFile::default(),
    };
    let dir = data_dir.or(file.data_dir).unwrap_or_else(crate::config::Config::default_data_dir);
    Ok((dir, file.pricing.merge_legacy(&file.usage_estimate)))
}

pub async fn run(args: &Usage, config_path: Option<&Path>, data_dir: Option<PathBuf>) -> Result<()> {
    let query = args.query()?;
    query.filters.validate()?;
    query.range.resolve(&chrono::Local::now())?;
    let (data_dir, cfg) = resolve(config_path, data_dir)?;
    let usage_dir = data_dir.join(super::USAGE_DIR);
    tracing::debug!(range = query.range.kind(), by = ?query.by.iter().map(|b| b.as_str()).collect::<Vec<_>>(), "usage report");
    let pricing = PriceStore::new(data_dir, &cfg).ensure_current(RefreshUse::Usage, args.refresh_prices).await;
    let report = build_local(&query, usage_dir.is_dir().then_some(usage_dir.as_path()), &pricing)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
}

pub async fn run_pricing(args: &Pricing, config_path: Option<&Path>, data_dir: Option<PathBuf>) -> Result<()> {
    let (data_dir, cfg) = resolve(config_path, data_dir)?;
    let book = PriceStore::new(data_dir, &cfg).ensure_current(RefreshUse::Pricing, args.refresh).await;
    let keys = args.key.as_deref().map_or_else(|| book.known_keys().into_iter().collect::<Vec<_>>(), |key| vec![key]);
    let report = PricingReport { currency: "USD", unit: "perMillionTokens", cache: book.cache_info(chrono::Utc::now()), models: keys.into_iter().map(|key| PricingRow::lookup(key, &book)).collect() };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_pricing(&report);
    }
    Ok(())
}

#[derive(Serialize)]
struct PricingReport<'a> {
    currency: &'static str,
    unit: &'static str,
    #[serde(flatten)]
    cache: CacheInfo,
    models: Vec<PricingRow<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PricingRow<'a> {
    key: &'a str,
    matched_key: Option<&'a str>,
    price_source: PriceSource,
    base: Option<&'a RateBand>,
    tiers: &'a [ContextBand],
    builtin_ttl_fields: &'a [&'static str],
}

impl<'a> PricingRow<'a> {
    fn lookup(key: &'a str, book: &'a PriceBook) -> Self {
        match split_price_key(key).and_then(|(provider, model)| book.lookup(provider, model)) {
            Some(price) => Self {
                key,
                matched_key: Some(price.matched_key),
                price_source: price.source,
                base: Some(&price.bands.base),
                tiers: &price.bands.tiers,
                builtin_ttl_fields: &price.bands.builtin_ttl_fields,
            },
            None => Self { key, matched_key: None, price_source: PriceSource::Unknown, base: None, tiers: &[], builtin_ttl_fields: &[] },
        }
    }
}

fn print_pricing(report: &PricingReport<'_>) {
    for model in &report.models {
        print!("{} [{}]", model.key, model.price_source.as_str());
        if let Some(matched) = model.matched_key.filter(|key| *key != model.key) {
            print!(" (matched {matched})");
        }
        println!();
        if let Some(base) = model.base {
            println!("  band           input     output  reasoning  cacheRead    write5m    write1h   write30m   write12h");
            print_band("base", base);
            for tier in model.tiers {
                print_band(&format!(">{}", tier.threshold), &tier.rates);
            }
        }
        if !model.builtin_ttl_fields.is_empty() {
            println!("  Builtin long-TTL rates: {}", model.builtin_ttl_fields.join(", "));
        }
    }
    println!("Standard rates in USD per million tokens (estimate / 估测, not a bill; '-' = no rate). {}", freshness(report.cache));
}

fn print_band(band: &str, r: &RateBand) {
    let rate = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
    println!(
        "  {band:<10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        r.input,
        r.output,
        r.reasoning,
        rate(r.cache_read),
        rate(r.cache_write_5m),
        rate(r.cache_write_1h),
        rate(r.cache_write_30m),
        rate(r.cache_write_12h)
    );
}

fn freshness(cache: CacheInfo) -> String {
    let at = cache.fetched_at.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)).unwrap_or_else(|| "-".into());
    format!("models.dev cache: {} (fetchedAt: {at}).", cache.cache_status.as_str())
}

/// HTTP `GET /v1/usage` 查询参数（与 CLI 同一套过滤）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageParams {
    pub today: bool,
    pub month: bool,
    pub days: Option<u32>,
    pub since: Option<String>,
    pub until: Option<String>,
    #[serde(default, deserialize_with = "deserialize_by")]
    pub by: Vec<String>,
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub session: Option<String>,
    pub caller: Option<String>,
}

/// `by` 可以是逗号分隔的一个参数，或重复的 query key（`by=account&by=model`）。
fn deserialize_by<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    struct ByVisitor;
    impl<'de> Visitor<'de> for ByVisitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a comma-separated list or a repeated query parameter")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_string()])
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![v])
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                out.push(v);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_any(ByVisitor)
}

impl UsageParams {
    pub fn query(&self) -> Result<Query> {
        query(
            self.today,
            self.month,
            self.days,
            self.since.clone(),
            self.until.clone(),
            &self.by,
            filters(self.account.clone(), self.provider.clone(), self.model.clone(), self.profile.clone(), self.session.clone(), self.caller.clone()),
        )
    }
}

/// 已运行的服务：用配置的 dataDir 与 pricing 出报表。
pub fn report_for_hub(hub: &crate::hub::Hub, params: &UsageParams) -> Result<Report> {
    let query = params.query()?;
    let dir = hub.config.data_dir().join(super::USAGE_DIR);
    tracing::info!(
        range = query.range.kind(),
        by = ?query.by.iter().map(|b| b.as_str()).collect::<Vec<_>>(),
        account = query.filters.account.as_deref().unwrap_or("-"),
        provider = query.filters.provider.as_deref().unwrap_or("-"),
        model = query.filters.model.as_deref().unwrap_or("-"),
        "usage report requested"
    );
    let pricing = hub.pricing.snapshot();
    build_local(&query, dir.is_dir().then_some(dir.as_path()), &pricing)
}

fn print_table(report: &Report) {
    let estimate = report.estimate.is_some();
    let show_unpriced = estimate && report.total.unpriced_requests.unwrap_or(0) > 0;
    let mut cols: Vec<String> = report.by.iter().map(|b| b.as_str().to_string()).collect();
    cols.extend(["requests", "input", "output", "cacheRead", "cacheWrite", "cacheHit", "reasoning", "total", "retried", "failed"].iter().map(|s| (*s).to_string()));
    if estimate {
        cols.push("estCost".into());
        if show_unpriced {
            cols.push("unpriced".into());
        }
    }

    let mut rows: Vec<Vec<String>> = vec![];
    for group in &report.groups {
        let mut row: Vec<String> = report.by.iter().map(|b| group.key.get(b.as_str()).cloned().unwrap_or_else(|| "-".into())).collect();
        row.extend(totals_cells(&group.totals, estimate, show_unpriced));
        rows.push(row);
    }
    let mut total_row: Vec<String> = report.by.iter().enumerate().map(|(i, _)| if i == 0 { "total".into() } else { "".into() }).collect();
    if report.by.is_empty() {
        total_row.push("total".into());
    }
    total_row.extend(totals_cells(&report.total, estimate, show_unpriced));

    let mut widths: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    for row in rows.iter().chain(std::iter::once(&total_row)) {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let range = format!("{} .. {}", report.range.since.as_deref().unwrap_or("-"), report.range.until.as_deref().unwrap_or("-"));
    let days = report.range.days.map(|n| format!(", days={n}")).unwrap_or_default();
    println!("Usage {} ({}, {}{days})", report.range.kind, report.timezone, range);
    if report.skipped_lines > 0 {
        println!("skipped {} corrupt line(s)", report.skipped_lines);
    }
    print_row(&cols, &widths);
    for row in &rows {
        print_row(row, &widths);
    }
    if !rows.is_empty() {
        println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
    }
    print_row(&total_row, &widths);
    if let Some(note) = &report.estimate {
        let sources = report.total.sources.as_ref().expect("enabled estimates have source metadata");
        let sources = sources.price_sources.iter().map(|(source, count)| format!("{}={count}", source.as_str())).collect::<Vec<_>>().join(", ");
        let sources = if sources.is_empty() { "-" } else { &sources };
        println!("estCost ({}): {} Sources: {sources}. {}", note.currency, note.note, freshness(note.cache));
    }
}

fn format_hit_rate(rate: Option<f64>) -> String {
    rate.map(|r| format!("{:.1}%", r * 100.0)).unwrap_or_else(|| "-".into())
}

fn totals_cells(t: &Totals, estimate: bool, show_unpriced: bool) -> Vec<String> {
    let mut cells = vec![
        t.requests.to_string(),
        t.tokens.input.to_string(),
        t.tokens.output.to_string(),
        t.tokens.cache_read.to_string(),
        t.tokens.cache_write.to_string(),
        format_hit_rate(t.cache_hit_rate),
        t.tokens.reasoning.to_string(),
        t.tokens.total_tokens.to_string(),
        t.retried.to_string(),
        t.failed.to_string(),
    ];
    if estimate {
        cells.push(t.est_cost.map(|c| format!("{c:.6}")).unwrap_or_else(|| "-".into()));
        if show_unpriced {
            cells.push(t.unpriced_requests.map(|n| n.to_string()).unwrap_or_else(|| "0".into()));
        }
    }
    cells
}

fn print_row(cells: &[String], widths: &[usize]) {
    let mut out = Vec::new();
    for (cell, width) in cells.iter().zip(widths) {
        // 分组名（含 total）左对齐；纯数字 / 估测费用右对齐
        let numeric = !cell.is_empty() && cell != "total" && cell.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '%');
        if numeric {
            out.push(format!("{cell:>width$}"));
        } else {
            out.push(format!("{cell:<width$}"));
        }
    }
    println!("{}", out.join("  "));
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Parser, Debug)]
    #[command(name = "sub-bot")]
    struct Probe {
        #[command(subcommand)]
        cmd: Cmd,
    }

    #[derive(Subcommand, Debug)]
    enum Cmd {
        Usage(Usage),
    }

    fn parse(args: &[&str]) -> Result<Usage, String> {
        let mut argv = vec!["sub-bot", "usage"];
        argv.extend(args);
        match Probe::try_parse_from(argv) {
            Ok(Probe { cmd: Cmd::Usage(u) }) => Ok(u),
            Err(e) => Err(e.to_string()),
        }
    }

    #[test]
    fn default_is_today_grouped_by_account() {
        let u = parse(&[]).unwrap();
        let q = u.query().unwrap();
        assert_eq!(q.range, RangeSpec::Today);
        assert_eq!(q.by, vec![GroupBy::Account]);
    }

    #[test]
    fn refresh_flag_is_independent_of_report_query() {
        let u = parse(&["--refresh-prices", "--json", "--days", "2"]).unwrap();
        assert!(u.refresh_prices && u.json);
        assert_eq!(u.query().unwrap().range, RangeSpec::Days(2));
        assert!(!parse(&[]).unwrap().refresh_prices);
    }

    #[test]
    fn conflicting_ranges_are_rejected() {
        let u = parse(&["--today", "--month"]).unwrap();
        let err = u.query().unwrap_err().to_string();
        assert!(err.contains("conflicting time ranges"), "{err}");
        let u = parse(&["--days", "3", "--since", "2026-11-01"]).unwrap();
        assert!(u.query().unwrap_err().to_string().contains("conflicting time ranges"));
    }

    #[test]
    fn days_zero_is_clap_error() {
        assert!(parse(&["--days", "0"]).is_err());
    }

    #[test]
    fn by_repeated_and_comma_separated() {
        let u = parse(&["--by", "account", "--by", "model,day", "--by", "account"]).unwrap();
        assert_eq!(u.query().unwrap().by, vec![GroupBy::Account, GroupBy::Model, GroupBy::Day]);
        assert!(parse(&["--by", "nope"]).unwrap().query().unwrap_err().to_string().contains("invalid by"));
    }

    #[test]
    fn since_until_and_filters() {
        let u = parse(&["--since", "2026-11-01", "--until", "2026-11-16", "--account", "anthropic/work", "--model", "claude-opus-5-5"]).unwrap();
        let q = u.query().unwrap();
        match q.range {
            RangeSpec::Between { since, until } => {
                assert_eq!(since.as_deref(), Some("2026-11-01"));
                assert_eq!(until.as_deref(), Some("2026-11-16"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(q.filters.account.as_deref(), Some("anthropic/work"));
        assert_eq!(q.filters.model.as_deref(), Some("claude-opus-5-5"));
    }
}
