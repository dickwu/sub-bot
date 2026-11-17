//! models.dev feed facts and compilation into standard token-rate bands.
use super::catalog::{ContextBand, PriceBands, RateBand, Table};
use super::prices::{self, ModelPrice};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FeedRow {
    pub base: FeedRates,
    pub tiers: Vec<FeedTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_over_200k: Option<FeedRates>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FeedRates {
    pub input: f64,
    pub output: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl FeedRates {
    fn valid(self) -> bool {
        [Some(self.input), Some(self.output), self.cache_read, self.cache_write].into_iter().flatten().all(|v| v.is_finite() && v >= 0.0)
    }

    fn band(self, provider: &str) -> RateBand {
        RateBand {
            input: self.input,
            output: self.output,
            reasoning: self.output,
            cache_read: self.cache_read,
            cache_write_5m: if provider == "anthropic" { self.cache_write } else { None },
            cache_write_30m: if provider == "openai" { self.cache_write } else { None },
            cache_write_1h: None,
            cache_write_12h: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct FeedTier {
    pub threshold: u64,
    #[serde(flatten)]
    pub rates: FeedRates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SkipReason {
    ProviderShape,
    InvalidKey,
    MissingCost,
    BadShape,
    MissingRates,
    InvalidRate,
    InvalidTier,
    ConflictingTier,
    AmbiguousTier,
}

#[derive(Debug, Default)]
pub(super) struct ParseStats {
    pub skipped: BTreeMap<SkipReason, usize>,
}

impl ParseStats {
    pub fn skip(&mut self, reason: SkipReason) {
        *self.skipped.entry(reason).or_default() += 1;
    }
}

#[derive(Debug, Default)]
pub(super) struct ParsedFeed {
    pub models: Table<FeedRow>,
    pub stats: ParseStats,
}

#[derive(Debug)]
pub(super) struct FeedError;

#[derive(Deserialize)]
struct WireRoot<'a> {
    #[serde(borrow)]
    openai: Option<&'a RawValue>,
    #[serde(borrow)]
    anthropic: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct WireProvider<'a> {
    #[serde(borrow)]
    models: Table<&'a RawValue>,
}

#[derive(Deserialize)]
struct WireModel<'a> {
    #[serde(borrow)]
    cost: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct WireRates {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Deserialize)]
struct WireTiers<'a> {
    #[serde(borrow)]
    tiers: Option<Vec<&'a RawValue>>,
    #[serde(borrow)]
    context_over_200k: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct WireTier<'a> {
    #[serde(borrow)]
    tier: &'a RawValue,
}

#[derive(Deserialize)]
struct WireDescriptor<'a> {
    #[serde(rename = "type", borrow)]
    kind: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    size: Option<&'a RawValue>,
}

fn object<'a, T: Deserialize<'a>>(raw: &'a RawValue) -> Result<T, SkipReason> {
    if !raw.get().starts_with('{') {
        return Err(SkipReason::BadShape);
    }
    serde_json::from_str(raw.get()).map_err(|_| SkipReason::BadShape)
}

pub(super) fn parse_models_dev(body: &[u8]) -> Result<ParsedFeed, FeedError> {
    let text = std::str::from_utf8(body).map_err(|_| FeedError)?;
    if !text.trim_start().starts_with('{') {
        return Err(FeedError);
    }
    // Borrow raw JSON only for our two providers; serde skips every other root field.
    let root: WireRoot<'_> = serde_json::from_str(text).map_err(|_| FeedError)?;
    let mut parsed = ParsedFeed::default();
    for (provider, raw) in [("openai", root.openai), ("anthropic", root.anthropic)] {
        let Some(raw) = raw else { continue };
        let Ok(wire) = object::<WireProvider<'_>>(raw) else {
            parsed.stats.skip(SkipReason::ProviderShape);
            continue;
        };
        for (model, raw) in wire.models {
            if !prices::valid_key_part(&model) {
                parsed.stats.skip(SkipReason::InvalidKey);
                continue;
            }
            match parse_row(raw) {
                Ok(row) => {
                    parsed.models.insert(format!("{provider}/{model}"), row);
                }
                Err(reason) => parsed.stats.skip(reason),
            }
        }
    }
    Ok(parsed)
}

fn parse_rates(raw: &RawValue) -> Result<FeedRates, SkipReason> {
    let WireRates { input, output, cache_read, cache_write } = object(raw)?;
    let row = FeedRates { input: input.ok_or(SkipReason::MissingRates)?, output: output.ok_or(SkipReason::MissingRates)?, cache_read, cache_write };
    if !row.valid() {
        return Err(SkipReason::InvalidRate);
    }
    Ok(row)
}

fn parse_row(raw: &RawValue) -> Result<FeedRow, SkipReason> {
    if raw.get() == "null" {
        return Err(SkipReason::MissingCost);
    }
    let model: WireModel<'_> = object(raw)?;
    let cost = model.cost.ok_or(SkipReason::MissingCost)?;
    let base = parse_rates(cost)?;
    let wire: WireTiers<'_> = object(cost)?;
    let mut tiers = Vec::new();
    for raw in wire.tiers.into_iter().flatten() {
        let tier: WireTier<'_> = object(raw).map_err(|_| SkipReason::InvalidTier)?;
        let descriptor: WireDescriptor<'_> = object(tier.tier).map_err(|_| SkipReason::InvalidTier)?;
        if descriptor.kind != "context" {
            // Ignore identifiable non-context modes, including their rates.
            continue;
        }
        let threshold = descriptor.size.and_then(|size| serde_json::from_str::<u64>(size.get()).ok()).filter(|&n| n > 0).ok_or(SkipReason::InvalidTier)?;
        let rates = parse_rates(raw).map_err(|_| SkipReason::InvalidTier)?;
        tiers.push(FeedTier { threshold, rates });
    }
    tiers.sort_unstable_by_key(|t| t.threshold);
    tiers.dedup();
    if tiers.windows(2).any(|w| w[0].threshold == w[1].threshold) {
        return Err(SkipReason::ConflictingTier);
    }
    let legacy_over_200k = if tiers.is_empty() { wire.context_over_200k.map(parse_rates).transpose()? } else { None };
    Ok(FeedRow { base, tiers, legacy_over_200k })
}

/// Compile one row, consulting the matching builtin only for known cutoffs and long TTLs.
pub(super) fn compile_live(provider: &str, row: &FeedRow, builtin: Option<&ModelPrice>) -> Result<PriceBands, SkipReason> {
    if !row.base.valid() || row.tiers.iter().any(|tier| !tier.rates.valid()) || row.legacy_over_200k.is_some_and(|r| !r.valid()) {
        return Err(SkipReason::InvalidRate);
    }
    // A cache read comes through here too. Stored feed facts must already be normalized.
    if row.tiers.first().is_some_and(|tier| tier.threshold == 0) || row.tiers.windows(2).any(|w| w[0].threshold >= w[1].threshold) {
        return Err(SkipReason::InvalidTier);
    }
    let builtin_tier = builtin.and_then(|b| b.long_context.as_ref());
    let mut bands =
        PriceBands { base: row.base.band(provider), tiers: row.tiers.iter().map(|t| ContextBand { threshold: t.threshold, rates: t.rates.band(provider) }).collect(), builtin_ttl_fields: Vec::new() };
    if bands.tiers.is_empty() {
        match (row.legacy_over_200k, builtin_tier) {
            (Some(rates), Some(tier)) => bands.tiers.push(ContextBand { threshold: tier.threshold, rates: rates.band(provider) }),
            (None, None) => {}
            // The legacy field doesn't reliably identify its cutoff. Likewise, a
            // flat feed row must not erase a known surcharge when tiers go missing.
            _ => return Err(SkipReason::AmbiguousTier),
        }
    }
    if let Some(builtin) = builtin {
        if bands.tiers.iter().map(|t| t.threshold).eq(builtin_tier.iter().map(|t| t.threshold)) {
            let fields = &mut bands.builtin_ttl_fields;
            for (rates, t1h, t12h, path1h, path12h) in std::iter::once((&mut bands.base, builtin.cache_write_1h, builtin.cache_write_12h, "base.cacheWrite1h", "base.cacheWrite12h"))
                .chain(bands.tiers.iter_mut().zip(builtin_tier).map(|(live, b)| (&mut live.rates, b.cache_write_1h, b.cache_write_12h, "tiers[0].cacheWrite1h", "tiers[0].cacheWrite12h")))
            {
                if provider == "anthropic" && t1h.is_some() {
                    rates.cache_write_1h = t1h;
                    fields.push(path1h);
                }
                if provider == "openai" && t12h.is_some() {
                    rates.cache_write_12h = t12h;
                    fields.push(path12h);
                }
            }
        }
    }
    Ok(bands)
}

/// Compile using provider/model map keys and the same exact-then-snapshot lookup as reports.
pub(super) fn compile_row(builtins: &Table<ModelPrice>, key: &str, row: &FeedRow) -> Result<PriceBands, SkipReason> {
    let (provider, model) = prices::split_price_key(key).ok_or(SkipReason::InvalidKey)?;
    if !matches!(provider, "openai" | "anthropic") {
        return Err(SkipReason::InvalidKey);
    }
    compile_live(provider, row, prices::lookup(builtins, provider, model).map(|(_, price)| price))
}

#[cfg(test)]
#[path = "models_dev_tests.rs"]
mod tests;
