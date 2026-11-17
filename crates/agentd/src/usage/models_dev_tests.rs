use super::*;
use crate::usage::prices::official_prices;

pub(crate) const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/models-dev-pricing.json");

#[test]
fn sample_standard_prices_and_context_bands() {
    let parsed = parse_models_dev(FIXTURE).unwrap();
    assert_eq!(parsed.models.len(), 6);
    assert!(parsed.stats.skipped.is_empty());
    let rates = |r: &RateBand| (r.input, r.output, r.cache_read.unwrap(), r.cache_write_30m.unwrap());
    for (key, base, long) in [("openai/gpt-6.2-astra", (8., 40., 0.4, 10.), (16., 60., 0.8, 20.)), ("openai/gpt-6.2-sol", (2., 10., 0.1, 2.5), (4., 15., 0.2, 5.))] {
        let builtins = official_prices();
        for bands in [compile_row(&builtins, key, &parsed.models[key]).unwrap(), builtins[key].bands()] {
            assert_eq!(rates(&bands.base), base);
            assert_eq!(bands.tiers.len(), 1);
            assert_eq!(bands.tiers[0].threshold, 272_000);
            assert_eq!(rates(&bands.tiers[0].rates), long);
            for r in [&bands.base, &bands.tiers[0].rates] {
                assert_eq!((r.cache_write_12h, r.cache_write_5m, r.cache_write_1h), (None, None, None));
                assert_eq!(r.reasoning, r.output);
            }
        }
        assert!(parsed.models[key].legacy_over_200k.is_none());
    }
    assert_eq!(parsed.models["anthropic/claude-haiku-5-5"].tiers[0].threshold, 100_000);
    assert!(parsed.models.contains_key("anthropic/claude-sonnet-4-5-20250929"));
}

#[test]
fn bad_rows_are_isolated_and_unknown_fields_are_ignored() {
    let parsed = parse_models_dev(
        br#"{
      "anthropic": {"models": null},
      "openai": {"id": "other", "env": false, "models": {
        "": {}, "\u001b": {}, "missing-cost": {}, "null-model": null, "null-cost": {"cost":null},
        "model-array": [], "cost-array": {"cost":[0,0]}, "cost-string": {"cost":"free"},
        "missing-input": {"cost":{"output":2}}, "null-output": {"cost":{"input":2,"output":null}},
        "negative": {"cost":{"input":-1,"output":0}}, "negative-cache": {"cost":{"input":0,"output":0,"cache_write":-1}},
        "overflow": {"cost":{"input":1e9000,"output":1}}, "string": {"cost":{"input":"1","output":1}},
        "alias": {"id":"wrong-id", "name":false,"family":2,"status":"deprecated","limit":[], "experimental":"ignored",
            "cost":{"input":0,"output":0,"cache_read":null,"audio_input":-10,
                    "tiers":[{"tier":{"type":"flex","size":"bad"},"input":false}]}}
      }},
      "other": {"models":{"same":{"cost":{"input":1,"output":2}}}}
    }"#,
    )
    .unwrap();
    assert_eq!(parsed.models.keys().collect::<Vec<_>>(), ["openai/alias"]);
    assert_eq!(parsed.models["openai/alias"].base, FeedRates { input: 0.0, output: 0.0, cache_read: None, cache_write: None });
    assert!(parsed.models["openai/alias"].tiers.is_empty());
    assert_eq!(parsed.stats.skipped[&SkipReason::ProviderShape], 1);
    assert_eq!(parsed.stats.skipped[&SkipReason::InvalidKey], 2);
    assert_eq!(parsed.stats.skipped[&SkipReason::MissingCost], 3);
    assert_eq!(parsed.stats.skipped[&SkipReason::MissingRates], 2);
    assert_eq!(parsed.stats.skipped[&SkipReason::InvalidRate], 2);
    assert_eq!(parsed.stats.skipped[&SkipReason::BadShape], 5);
    for empty in ["{}", r#"{"openai":null}"#, r#"{"other":{"models":{}}}"#] {
        assert!(parse_models_dev(empty.as_bytes()).unwrap().models.is_empty());
    }
    for invalid in ["{}{}", "null", "[]", r#"{"other":[1,]}"#, r#"{"openai":{"models":{}"#] {
        assert!(parse_models_dev(invalid.as_bytes()).is_err(), "{invalid}");
    }
}

fn one_row(cost: &str) -> Result<FeedRow, SkipReason> {
    let raw = serde_json::value::to_raw_value(&serde_json::json!({"cost": serde_json::from_str::<serde_json::Value>(cost).unwrap()})).unwrap();
    parse_row(&raw)
}

#[test]
fn explicit_context_tiers_sort_dedup_and_win_over_legacy() {
    let row = one_row(
        r#"{"input":1,"output":2,"context_over_200k":"unusable but ignored",
      "tiers":[{"tier":{"type":"context","size":272000},"input":3,"output":4},
               {"tier":{"type":"context","size":100000},"input":1.5,"output":2.5},
               {"tier":{"type":"context","size":272000},"input":3,"output":4}]}"#,
    )
    .unwrap();
    assert_eq!(row.tiers.iter().map(|t| (t.threshold, t.rates.input)).collect::<Vec<_>>(), [(100_000, 1.5), (272_000, 3.)]);
    assert_eq!(row.legacy_over_200k, None);
    for bad in ["0", "-3", "3.5", "null", "\"100\""] {
        let cost = format!(r#"{{"input":1,"output":1,"tiers":[{{"tier":{{"type":"context","size":{bad}}},"input":2,"output":2}}]}}"#);
        assert_eq!(one_row(&cost), Err(SkipReason::InvalidTier));
    }
    assert_eq!(one_row(r#"{"input":1,"output":1,"tiers":[{"tier":{"type":"context","size":1},"input":2}]}"#), Err(SkipReason::InvalidTier));
    assert_eq!(
        one_row(r#"{"input":1,"output":1,"tiers":[{"tier":{"type":"context","size":1},"input":2,"output":2},{"tier":{"type":"context","size":1},"input":2,"output":3}]}"#),
        Err(SkipReason::ConflictingTier)
    );
}

#[test]
fn legacy_only_uses_the_builtin_cutoff_and_flat_rows_cannot_erase_it() {
    let builtins = official_prices();
    let row = one_row(r#"{"input":1,"output":2,"context_over_200k":{"input":4,"output":8}}"#).unwrap();
    let bands = compile_row(&builtins, "openai/gpt-6.1-sol-20261117", &row).unwrap();
    assert_eq!(bands.tiers[0].threshold, 272_000);
    assert_eq!(bands.tiers[0].rates.output, 8.0);
    assert_eq!(compile_row(&builtins, "openai/unknown", &row), Err(SkipReason::AmbiguousTier));
    let flat = one_row(r#"{"input":1,"output":2}"#).unwrap();
    assert_eq!(compile_row(&builtins, "openai/gpt-6.2-sol", &flat), Err(SkipReason::AmbiguousTier));
    assert!(compile_row(&builtins, "openai/unknown", &flat).is_ok());
}

#[test]
fn absolute_builtin_ttl_is_only_copied_for_matching_bands() {
    let mut parsed = parse_models_dev(FIXTURE).unwrap();
    let builtins = official_prices();
    for (provider, model, base, long, fields) in
        [("openai", "gpt-6.1-sol", 5., 10., ["base.cacheWrite12h", "tiers[0].cacheWrite12h"]), ("anthropic", "claude-haiku-5-5", 0.2, 1., ["base.cacheWrite1h", "tiers[0].cacheWrite1h"])]
    {
        let key = format!("{provider}/{model}");
        let row = parsed.models.get_mut(&key).unwrap();
        row.base.cache_write = Some(20.0);
        row.tiers[0].rates.cache_write = Some(100.0);
        let bands = compile_row(&builtins, &key, row).unwrap();
        let short_ttl = |r: &RateBand| if provider == "openai" { r.cache_write_30m } else { r.cache_write_5m };
        let long_ttl = |r: &RateBand| if provider == "openai" { r.cache_write_12h } else { r.cache_write_1h };
        assert_eq!(bands.builtin_ttl_fields, fields);
        assert_eq!((short_ttl(&bands.base), short_ttl(&bands.tiers[0].rates)), (Some(20.), Some(100.)));
        assert_eq!((long_ttl(&bands.base), long_ttl(&bands.tiers[0].rates)), (Some(base), Some(long)));
        row.tiers[0].threshold += 1;
        let bands = compile_row(&builtins, &key, row).unwrap();
        assert!(bands.builtin_ttl_fields.is_empty());
        assert_eq!((long_ttl(&bands.base), long_ttl(&bands.tiers[0].rates)), (None, None));
    }
    let flat = one_row(r#"{"input":1,"output":2,"cache_write":9}"#).unwrap();
    assert_eq!(compile_row(&builtins, "anthropic/claude-opus-5-5", &flat).unwrap().base.cache_write_1h, Some(8.));
    assert_eq!(compile_row(&builtins, "anthropic/claude-sonnet-4-5-20250929", &flat).unwrap().base.cache_write_1h, None);
}
