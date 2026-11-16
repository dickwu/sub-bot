//! CLI `sub-bot usage` 与 HTTP `GET /v1/usage`：解析时间范围 / 分组 / 过滤，生成同一份 [`super::report::Report`]。
//! CLI 不构造 Hub、不加载 provider 或 webhook 的密钥文件，只读 dataDir 与 `pricing`（及已弃用的 `usageEstimate` 覆盖）。
use super::report::{build_local, Filters, GroupBy, PricingConfig, Query, RangeSpec, Report, Totals, UsageEstimateConfig, MAX_DAYS};
use anyhow::{Context, Result};
use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
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
            file.pricing.validate("pricing.models")?;
            PricingConfig::validate_legacy(&file.usage_estimate, "usageEstimate.pricesUsdPerMillion")?;
            file
        }
        None => UsageFile::default(),
    };
    let dir = data_dir.or(file.data_dir).unwrap_or_else(crate::config::Config::default_data_dir);
    Ok((dir, file.pricing.merge_legacy(&file.usage_estimate)))
}

pub fn run(args: &Usage, config_path: Option<&Path>, data_dir: Option<PathBuf>) -> Result<()> {
    let query = args.query()?;
    let (data_dir, estimate) = resolve(config_path, data_dir)?;
    let usage_dir = data_dir.join(super::USAGE_DIR);
    let dir = usage_dir.is_dir().then_some(usage_dir.as_path());
    tracing::debug!(data_dir = %data_dir.display(), range = query.range.kind(), by = ?query.by.iter().map(|b| b.as_str()).collect::<Vec<_>>(), "usage report");
    let report = build_local(&query, dir, &estimate)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
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
    build_local(&query, dir.is_dir().then_some(dir.as_path()), &hub.config.effective_pricing())
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
        println!("estCost is an estimate (估测) in {}, not a bill. {}", note.currency, note.note);
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
