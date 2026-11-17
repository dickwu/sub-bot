//! Immutable price tables. One book is shared by all records in a report.
use super::prices::{self, CostBreakdown, PricingConfig, PricingSource};
use super::UsageRecord;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub(super) type Table<T> = BTreeMap<String, T>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PriceSource {
    Override,
    #[serde(rename = "models.dev")]
    ModelsDev,
    Builtin,
    Unknown,
}

impl PriceSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::ModelsDev => "models.dev",
            Self::Builtin => "builtin",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RateBand {
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_30m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_12h: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ContextBand {
    pub threshold: u64,
    #[serde(flatten)]
    pub rates: RateBand,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct PriceBands {
    pub base: RateBand,
    /// Ordered by increasing cutoff; each band prices the whole request.
    pub tiers: Vec<ContextBand>,
    pub builtin_ttl_fields: Vec<&'static str>,
}

#[derive(Debug)]
pub(super) struct PriceMatch<'a> {
    pub matched_key: &'a str,
    pub source: PriceSource,
    pub bands: &'a PriceBands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum CacheStatus {
    Fresh,
    Stale,
    Missing,
    Disabled,
}

impl CacheStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Missing => "missing",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CacheInfo {
    pub fetched_at: Option<DateTime<Utc>>,
    pub cache_status: CacheStatus,
}

#[derive(Clone, Copy, Default)]
pub(super) struct ProviderCounts {
    pub openai: usize,
    pub anthropic: usize,
}

impl ProviderCounts {
    pub fn of(live: &Table<PriceBands>) -> Self {
        Self { openai: live.keys().filter(|k| k.starts_with("openai/")).count(), anthropic: live.keys().filter(|k| k.starts_with("anthropic/")).count() }
    }

    pub fn loses_provider(&self, previous: &Self) -> bool {
        (self.openai == 0 && previous.openai > 0) || (self.anthropic == 0 && previous.anthropic > 0)
    }
}

struct LocalPrices {
    overrides: Table<PriceBands>,
    builtin: Table<PriceBands>,
}

pub(crate) struct PriceBook {
    pub(super) enabled: bool,
    pub(super) source: PricingSource,
    pub(super) refresh_hours: u32,
    pub(super) fetched_at: Option<DateTime<Utc>>,
    pub(super) live_counts: ProviderCounts,
    local: Arc<LocalPrices>,
    live: Table<PriceBands>,
}

impl PriceBook {
    pub(super) fn build(cfg: &PricingConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            source: cfg.source,
            refresh_hours: cfg.refresh_hours,
            fetched_at: None,
            live_counts: ProviderCounts::default(),
            local: Arc::new(LocalPrices {
                overrides: cfg.models.iter().map(|(key, row)| (key.clone(), row.bands())).collect(),
                builtin: prices::official_prices().into_iter().map(|(key, row)| (key, row.bands())).collect(),
            }),
            live: Table::new(),
        }
    }

    pub(super) fn with_live(&self, fetched_at: DateTime<Utc>, live: Table<PriceBands>) -> Self {
        debug_assert_eq!(self.source, PricingSource::ModelsDev);
        Self { enabled: self.enabled, source: self.source, refresh_hours: self.refresh_hours, fetched_at: Some(fetched_at), live_counts: ProviderCounts::of(&live), local: self.local.clone(), live }
    }

    pub(super) fn cache_info(&self, now: DateTime<Utc>) -> CacheInfo {
        let cache_status = match (self.source, self.fetched_at) {
            (PricingSource::Builtin, _) => CacheStatus::Disabled,
            (_, Some(at)) if at <= now && now - at < TimeDelta::hours(i64::from(self.refresh_hours)) => CacheStatus::Fresh,
            (_, Some(_)) => CacheStatus::Stale,
            (_, None) => CacheStatus::Missing,
        };
        CacheInfo { fetched_at: self.fetched_at, cache_status }
    }

    pub(super) fn lookup(&self, provider: &str, model: &str) -> Option<PriceMatch<'_>> {
        [(PriceSource::Override, &self.local.overrides), (PriceSource::ModelsDev, &self.live), (PriceSource::Builtin, &self.local.builtin)]
            .into_iter()
            .find_map(|(source, table)| prices::lookup(table, provider, model).map(|(matched_key, bands)| PriceMatch { matched_key, source, bands }))
    }

    pub(super) fn known_keys(&self) -> BTreeSet<&str> {
        self.local.overrides.keys().chain(self.live.keys()).chain(self.local.builtin.keys()).map(String::as_str).collect()
    }

    pub(super) fn cost(&self, record: &UsageRecord) -> (PriceSource, Option<CostBreakdown>) {
        if self.enabled {
            if let Some(price) = self.lookup(&record.provider, &record.model) {
                return (price.source, prices::estimate_record(price.bands, &record.provider, &record.tokens));
            }
        }
        (PriceSource::Unknown, None)
    }
}
