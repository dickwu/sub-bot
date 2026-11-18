//! Config overrides, builtin USD-per-million-token prices, and per-record estimate math.
//!
//! Most builtin rows are from the official API pricing pages, checked 2026-11-16
//! (America/Winnipeg):
//! - Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - OpenAI: <https://developers.openai.com/api/docs/pricing>
//!
//! The GPT-6.2 rows are from the 2026-11-17 models.dev sample. These are standard
//! token rates, not batch/flex/fast rates or a bill. OpenAI's 30m/12h cache-write
//! TTLs are separate from Anthropic's 5m/1h and from long-context surcharges.

use super::catalog::{ContextBand, PriceBands, RateBand};
use super::Tokens;
use crate::config::ConfigRule;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const MODELS_DEV_URL: &str = "https://models.dev/api.json?type=all";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum PricingSource {
    #[default]
    #[serde(rename = "models.dev")]
    ModelsDev,
    #[serde(rename = "builtin")]
    Builtin,
}

/// Config `pricing` section. `enabled` controls estimates, `source` controls live I/O.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PricingConfig {
    pub enabled: bool,
    pub source: PricingSource,
    pub models_dev_url: String,
    pub refresh_hours: u32,
    /// Overrides and additions keyed by `provider/model`. Each replaces a whole row.
    pub models: BTreeMap<String, ModelPrice>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self { enabled: true, source: PricingSource::ModelsDev, models_dev_url: MODELS_DEV_URL.into(), refresh_hours: 24, models: BTreeMap::new() }
    }
}

/// Deprecated `usageEstimate` shape from v0.11.0. Prices here are treated as overrides;
/// `enabled` is ignored; only `pricing.enabled` controls estimates.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageEstimateConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub prices_usd_per_million: BTreeMap<String, ModelPrice>,
}

impl PricingConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.models_dev_endpoint().is_some(), ConfigRule("pricing.modelsDevUrl must be an absolute HTTP(S) URL without userinfo or a fragment"));
        ensure!((1..=8760).contains(&self.refresh_hours), ConfigRule("pricing.refreshHours must be in 1..=8760"));
        validate_map(&self.models, "pricing.models")
    }

    pub(super) fn models_dev_endpoint(&self) -> Option<reqwest::Url> {
        let text = self.models_dev_url.as_str();
        // Require an explicit authority, and reject even empty userinfo before URL normalization.
        let (scheme, rest) = text.split_once("://")?;
        if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) || text.chars().any(char::is_control) {
            return None;
        }
        let authority = rest.split(['/', '?', '#', '\\']).next()?;
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        reqwest::Url::parse(text).ok().filter(|url| url.host_str().is_some() && url.username().is_empty() && url.password().is_none() && url.fragment().is_none())
    }

    /// Fold legacy `usageEstimate.pricesUsdPerMillion` into `models` (caller overrides still win).
    pub fn merge_legacy(&self, legacy: &UsageEstimateConfig) -> Self {
        let mut models = legacy.prices_usd_per_million.clone();
        for (k, v) in &self.models {
            models.insert(k.clone(), v.clone());
        }
        Self { models, ..self.clone() }
    }

    pub fn validate_legacy(legacy: &UsageEstimateConfig) -> Result<()> {
        validate_map(&legacy.prices_usd_per_million, "usageEstimate.pricesUsdPerMillion")
    }
}

fn validate_map(map: &BTreeMap<String, ModelPrice>, at: &str) -> Result<()> {
    for (key, price) in map {
        ensure!(split_price_key(key).is_some(), ConfigRule("price keys must be 'provider/model'").at(at));
        price.validate(&format!("{at}.{key}"))?;
    }
    Ok(())
}

pub(super) fn split_price_key(key: &str) -> Option<(&str, &str)> {
    let (provider, model) = key.split_once('/')?;
    (valid_key_part(provider) && valid_key_part(model)).then_some((provider, model))
}

pub(super) fn valid_key_part(part: &str) -> bool {
    !part.trim().is_empty() && !part.chars().any(char::is_control)
}

/// USD per million tokens. `cacheRead` also accepts the alias `cached` (OpenAI cached input).
/// `cacheWrite` (v0.11.0) is accepted as an alias of `cacheWrite5m`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, alias = "cached", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, alias = "cacheWrite", skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_30m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_12h: Option<f64>,
    /// Optional override; official pages bill reasoning as output, so this defaults to `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContextPrice>,
}

/// Long-context rates. Applied to the **whole request** when billed prompt tokens are
/// strictly greater than `threshold`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LongContextPrice {
    pub threshold: u64,
    pub input: f64,
    pub output: f64,
    #[serde(default, alias = "cached", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, alias = "cacheWrite", skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_30m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_12h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
}

impl ModelPrice {
    fn validate(&self, at: &str) -> Result<()> {
        for (name, value) in self.rates() {
            ensure!(value.is_finite() && value >= 0.0, ConfigRule("price rates must be non-negative numbers").at(format_args!("{at}.{name}")));
        }
        if let Some(lc) = &self.long_context {
            ensure!(lc.threshold > 0, ConfigRule("price longContext.threshold must be positive").at(at));
            for (name, value) in lc.rates() {
                ensure!(value.is_finite() && value >= 0.0, ConfigRule("price rates must be non-negative numbers").at(format_args!("{at}.longContext.{name}")));
            }
        }
        Ok(())
    }

    fn rates(&self) -> Vec<(&'static str, f64)> {
        let mut v = vec![("input", self.input), ("output", self.output)];
        for (name, value) in [
            ("cacheRead", self.cache_read),
            ("cacheWrite5m", self.cache_write_5m),
            ("cacheWrite1h", self.cache_write_1h),
            ("cacheWrite30m", self.cache_write_30m),
            ("cacheWrite12h", self.cache_write_12h),
            ("reasoning", self.reasoning),
        ] {
            if let Some(x) = value {
                v.push((name, x));
            }
        }
        v
    }

    pub(super) fn bands(&self) -> PriceBands {
        let base = RateBand {
            input: self.input,
            output: self.output,
            cache_read: Some(self.cache_read.unwrap_or(self.input)),
            cache_write_5m: self.cache_write_5m,
            cache_write_1h: self.cache_write_1h,
            cache_write_30m: self.cache_write_30m,
            cache_write_12h: self.cache_write_12h,
            reasoning: self.reasoning.unwrap_or(self.output),
        };
        let tiers = self
            .long_context
            .iter()
            .map(|lc| ContextBand {
                threshold: lc.threshold,
                rates: RateBand {
                    input: lc.input,
                    output: lc.output,
                    cache_read: Some(lc.cache_read.or(self.cache_read).unwrap_or(lc.input)),
                    cache_write_5m: lc.cache_write_5m.or(self.cache_write_5m),
                    cache_write_1h: lc.cache_write_1h.or(self.cache_write_1h),
                    cache_write_30m: lc.cache_write_30m.or(self.cache_write_30m),
                    cache_write_12h: lc.cache_write_12h.or(self.cache_write_12h),
                    reasoning: lc.reasoning.or(self.reasoning).unwrap_or(lc.output),
                },
            })
            .collect();
        PriceBands { base, tiers, builtin_ttl_fields: Vec::new() }
    }
}

impl LongContextPrice {
    fn rates(&self) -> Vec<(&'static str, f64)> {
        let mut v = vec![("input", self.input), ("output", self.output)];
        for (name, value) in [
            ("cacheRead", self.cache_read),
            ("cacheWrite5m", self.cache_write_5m),
            ("cacheWrite1h", self.cache_write_1h),
            ("cacheWrite30m", self.cache_write_30m),
            ("cacheWrite12h", self.cache_write_12h),
            ("reasoning", self.reasoning),
        ] {
            if let Some(x) = value {
                v.push((name, x));
            }
        }
        v
    }
}

/// Estimated USD cost broken into the four buckets shown in `--json` / `GET /v1/usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_read_cost: f64,
    pub cache_write_cost: f64,
}

impl CostBreakdown {
    pub fn total(self) -> f64 {
        self.input_cost + self.output_cost + self.cache_read_cost + self.cache_write_cost
    }

    pub fn add(&mut self, other: Self) {
        self.input_cost += other.input_cost;
        self.output_cost += other.output_cost;
        self.cache_read_cost += other.cache_read_cost;
        self.cache_write_cost += other.cache_write_cost;
    }

    pub fn rounded(self) -> Self {
        let r = |x: f64| (x * 1e9).round() / 1e9;
        Self { input_cost: r(self.input_cost), output_cost: r(self.output_cost), cache_read_cost: r(self.cache_read_cost), cache_write_cost: r(self.cache_write_cost) }
    }
}

/// Prompt size used to choose a long-context tier.
/// Anthropic `input_tokens` excludes cache reads/writes; OpenAI `prompt_tokens` / `input_tokens` already include them.
pub fn billed_prompt_tokens(provider: &str, t: &Tokens) -> u64 {
    if provider == "anthropic" {
        t.input.saturating_add(t.cache_read).saturating_add(t.cache_write_total())
    } else {
        t.input
    }
}

/// Uncached input tokens, without double-counting provider-specific cache fields.
pub fn uncached_input(provider: &str, t: &Tokens) -> u64 {
    if provider == "anthropic" {
        t.input
    } else {
        t.input.saturating_sub(t.cache_read).saturating_sub(t.cache_write_total())
    }
}

/// `cacheRead / (uncached input + cacheRead + cacheWrite)`. `None` when the denominator is 0.
#[allow(dead_code)] // used by tests; kept as the documented per-record formula
pub fn cache_hit_rate(provider: &str, t: &Tokens) -> Option<f64> {
    cache_hit_rate_from_parts(uncached_input(provider, t), t.cache_read, t.cache_write_total())
}

pub fn cache_hit_rate_from_parts(uncached: u64, cache_read: u64, cache_write: u64) -> Option<f64> {
    let den = uncached.saturating_add(cache_read).saturating_add(cache_write);
    (den > 0).then(|| cache_read as f64 / den as f64)
}

/// Look up `provider/model`, then the same id with a trailing `-YYYYMMDD` snapshot suffix stripped.
pub(super) fn lookup<'a, T>(prices: &'a BTreeMap<String, T>, provider: &str, model: &str) -> Option<(&'a str, &'a T)> {
    let key = format!("{provider}/{model}");
    prices.get_key_value(&key).or_else(|| strip_snapshot(model).and_then(|base| prices.get_key_value(&format!("{provider}/{base}")))).map(|(key, price)| (key.as_str(), price))
}

fn strip_snapshot(model: &str) -> Option<&str> {
    let (base, date) = model.rsplit_once('-')?;
    (date.len() == 8 && date.chars().all(|c| c.is_ascii_digit())).then_some(base)
}

/// Apply the selected context band to the whole call. `None` if a used cache rate is absent.
pub(super) fn estimate_record(price: &PriceBands, provider: &str, tokens: &Tokens) -> Option<CostBreakdown> {
    let prompt = billed_prompt_tokens(provider, tokens);
    let rates = price.tiers.iter().rev().find(|tier| prompt > tier.threshold).map_or(&price.base, |tier| &tier.rates);
    let writes = tokens.write_buckets();
    // OpenAI writes with no TTL split cannot be priced without guessing 30m vs 12h.
    if provider != "anthropic" && writes.unsplit > 0 {
        return None;
    }
    let need = |count: u64, rate: Option<f64>| if count == 0 { Some(0.0) } else { rate.map(|r| count as f64 * r) };
    let write_cost = need(writes.t5m, rates.cache_write_5m)? + need(writes.t1h, rates.cache_write_1h)? + need(writes.t30m, rates.cache_write_30m)? + need(writes.t12h, rates.cache_write_12h)?;
    let uncached = uncached_input(provider, tokens);
    let reasoning = tokens.reasoning.min(tokens.output);
    let output_non_reasoning = tokens.output.saturating_sub(reasoning);
    let mtok = 1_000_000.0;
    Some(
        CostBreakdown {
            input_cost: uncached as f64 * rates.input / mtok,
            output_cost: (output_non_reasoning as f64 * rates.output + reasoning as f64 * rates.reasoning) / mtok,
            cache_read_cost: need(tokens.cache_read, rates.cache_read)? / mtok,
            cache_write_cost: write_cost / mtok,
        }
        .rounded(),
    )
}

pub(super) fn official_prices() -> BTreeMap<String, ModelPrice> {
    let mut m = BTreeMap::new();
    // Anthropic: https://platform.claude.com/docs/en/about-claude/pricing (2026-11-16)
    // columns: base input, 5m cache writes, 1h cache writes, cache hits and refreshes, output.
    insert(&mut m, "anthropic/claude-fable-5-2", 8.0, 40.0, 0.20, 10.0, 16.0, None);
    insert(&mut m, "anthropic/claude-fable-5-1", 10.0, 50.0, 0.25, 12.50, 20.0, None);
    insert(&mut m, "anthropic/claude-opus-5-5", 4.0, 20.0, 0.20, 5.0, 8.0, None);
    insert(&mut m, "anthropic/claude-sonnet-5-5", 2.0, 10.0, 0.10, 2.50, 4.0, None);
    // Haiku 5.5 has a listed long-context tier for prompts over 100,000 tokens.
    let mut haiku = ModelPrice {
        input: 0.10,
        output: 0.50,
        cache_read: Some(0.01),
        cache_write_5m: Some(0.125),
        cache_write_1h: Some(0.20),
        cache_write_30m: None,
        cache_write_12h: None,
        reasoning: None,
        long_context: None,
    };
    haiku.long_context = Some(LongContextPrice {
        threshold: 100_000,
        input: 0.50,
        output: 2.50,
        cache_read: Some(0.05),
        cache_write_5m: Some(0.625),
        cache_write_1h: Some(1.0),
        cache_write_30m: None,
        cache_write_12h: None,
        reasoning: None,
    });
    m.insert("anthropic/claude-haiku-5-5".into(), haiku);

    // OpenAI: https://developers.openai.com/api/docs/pricing (2026-11-16), Standard short/long context.
    insert_openai(&mut m, "openai/gpt-6.1-sol", 2.0, 10.0, 0.10, 2.50, Some(5.0), 4.0, 15.0, 0.20, 5.0, Some(10.0));
    insert_openai(&mut m, "openai/gpt-6-astra", 10.0, 50.0, 1.0, 12.50, Some(25.0), 20.0, 75.0, 2.0, 25.0, Some(50.0));
    // models.dev (2026-11-17): only the short (30m) write rate is known, in both context bands.
    insert_openai(&mut m, "openai/gpt-6.2-astra", 8.0, 40.0, 0.4, 10.0, None, 16.0, 60.0, 0.8, 20.0, None);
    insert_openai(&mut m, "openai/gpt-6.2-sol", 2.0, 10.0, 0.1, 2.5, None, 4.0, 15.0, 0.2, 5.0, None);
    m
}

#[allow(clippy::too_many_arguments)]
fn insert(map: &mut BTreeMap<String, ModelPrice>, key: &str, input: f64, output: f64, cache_read: f64, write_5m: f64, write_1h: f64, long: Option<LongContextPrice>) {
    map.insert(
        key.into(),
        ModelPrice {
            input,
            output,
            cache_read: Some(cache_read),
            cache_write_5m: Some(write_5m),
            cache_write_1h: Some(write_1h),
            cache_write_30m: None,
            cache_write_12h: None,
            reasoning: None,
            long_context: long,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn insert_openai(
    map: &mut BTreeMap<String, ModelPrice>,
    key: &str,
    input: f64,
    output: f64,
    cached: f64,
    write_30m: f64,
    write_12h: Option<f64>,
    long_input: f64,
    long_output: f64,
    long_cached: f64,
    long_30m: f64,
    long_12h: Option<f64>,
) {
    map.insert(
        key.into(),
        ModelPrice {
            input,
            output,
            cache_read: Some(cached),
            cache_write_5m: None,
            cache_write_1h: None,
            cache_write_30m: Some(write_30m),
            cache_write_12h: write_12h,
            reasoning: None,
            long_context: Some(LongContextPrice {
                threshold: 272_000,
                input: long_input,
                output: long_output,
                cache_read: Some(long_cached),
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: Some(long_30m),
                cache_write_12h: long_12h,
                reasoning: None,
            }),
        },
    );
}
