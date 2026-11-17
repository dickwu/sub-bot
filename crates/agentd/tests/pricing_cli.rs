#![cfg(unix)]

use axum::{
    http::{HeaderMap, StatusCode},
    routing::get,
    Router,
};
use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};
use sha2::Digest;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, process::Command, sync::Notify, task::JoinHandle};

const FIXTURE: &str = include_str!("fixtures/models-dev-pricing.json");
const TOKEN: &str = "local-test-bearer";
const URL_MARKER: &str = "catalog-query-must-not-be-logged";

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sb-price-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Feed {
    url: String,
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Feed {
    async fn start(block_second: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/feed?extra={URL_MARKER}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let app = Router::new().route(
            "/feed",
            get({
                let (calls, started) = (calls.clone(), started.clone());
                move |headers: HeaderMap| {
                    let (calls, started) = (calls.clone(), started.clone());
                    async move {
                        assert!(headers.get("authorization").is_none() && headers.get("x-api-key").is_none());
                        let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                        started.notify_one();
                        if block_second && call == 2 {
                            std::future::pending::<()>().await;
                        }
                        if block_second && call > 2 {
                            (StatusCode::SERVICE_UNAVAILABLE, "body-must-not-be-logged")
                        } else {
                            (StatusCode::OK, FIXTURE)
                        }
                    }
                }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, calls, started, task }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
    async fn stop(self) {
        self.task.abort();
        // The listener is closed before the next command tries to refresh.
        let _ = self.task.await;
    }
}

fn command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sub-bot"));
    command.env_clear().env("PATH", "/usr/bin:/bin").env("RUST_LOG", "debug").env("TZ", "UTC").arg("--config").arg(config).kill_on_drop(true);
    command
}

async fn run(config: &Path, args: &[&str]) -> Output {
    tokio::time::timeout(Duration::from_secs(10), command(config).args(args).output()).await.unwrap().unwrap()
}

fn success(out: &Output) {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn output_json(out: &Output) -> Value {
    success(out);
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(!log.contains(URL_MARKER) && !log.contains(TOKEN));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn write_config(path: &Path, config: &Value) {
    fs::write(path, config.to_string()).unwrap();
}

#[tokio::test]
async fn pricing_cli_isolated_list_show_and_refresh_policy() {
    let feed = Feed::start(false).await;
    let dir = TempDir::new();
    let config_path = dir.0.join("config.json");
    let unused_dir = dir.0.join("unused");
    let data = dir.0.join("selected");
    let mut cfg = json!({
        "dataDir": unused_dir, "apiKeyFile":"nonexistent-openai.key",
        "providers":{"anthropic":{"apiKeyFile":"nonexistent-anthropic.key"}},
        "clients":[{"webhookSecretFile":"nonexistent-webhook.key"}],
        "pricing":{"modelsDevUrl":feed.url,"models":{"openai/gpt-6.2-astra":{"input":9,"output":21}}}
    });
    write_config(&config_path, &cfg);
    let data_arg = data.to_str().unwrap();
    for bad in [["pricing", "openai", "--refresh"], ["usage", "--refresh-prices", "--since=bad-time"]] {
        assert!(!run(&config_path, &bad).await.status.success());
    }
    assert_eq!(feed.count(), 0);
    let first = run(&config_path, &["--data-dir", data_arg, "pricing", "--refresh", "--json"]).await;
    let prices = output_json(&first);
    assert_eq!(prices["unit"], "perMillionTokens");
    assert_eq!(prices["cacheStatus"], "fresh");
    let list = prices["models"].as_array().unwrap();
    assert!(list.windows(2).all(|rows| rows[0]["key"].as_str() < rows[1]["key"].as_str()));
    assert!(list.iter().any(|row| row["key"] == "openai/gpt-6.2-astra" && row["priceSource"] == "override"));
    assert!(list.iter().any(|row| row["key"] == "openai/gpt-6.2-sol" && row["priceSource"] == "models.dev"));
    assert!(list.iter().any(|row| row["key"] == "openai/gpt-6.1-sol" && row["builtinTtlFields"] == json!(["base.cacheWrite12h", "tiers[0].cacheWrite12h"])));
    assert!(!unused_dir.exists());
    assert_eq!(fs::read_dir(&data).unwrap().count(), 1); // just pricing/, no logs/state/usage

    let show = async |key| output_json(&run(&config_path, &["--data-dir", data_arg, "pricing", key, "--json"]).await);
    let sol = show("openai/gpt-6.2-sol-20261117").await;
    assert_eq!(sol["fetchedAt"], prices["fetchedAt"]);
    let sol = &sol["models"][0];
    assert_eq!(sol["matchedKey"], "openai/gpt-6.2-sol");
    assert_eq!(sol["priceSource"], "models.dev");
    assert_eq!(sol["tiers"][0]["threshold"], 272_000);
    assert!(sol["tiers"][0].get("cacheWrite12h").is_none());
    let unknown = show("openai/unknown").await;
    assert_eq!(unknown["models"], json!([{"key":"openai/unknown","matchedKey":null,"priceSource":"unknown","base":null,"tiers":[],"builtinTtlFields":[]}]));
    let tabular = run(&config_path, &["--data-dir", data_arg, "usage"]).await;
    success(&tabular);
    let text = String::from_utf8(tabular.stdout).unwrap();
    assert!(text.contains("estimate (估测), not a bill") && text.contains("models.dev cache: fresh") && text.contains("sub-bot pricing"));
    assert_eq!(feed.count(), 1);

    cfg["pricing"]["enabled"] = json!(false);
    write_config(&config_path, &cfg);
    let hidden = run(&config_path, &["--data-dir", data_arg, "usage", "--refresh-prices", "--json"]).await;
    assert!(output_json(&hidden).get("estimate").is_none());
    assert!(String::from_utf8_lossy(&hidden.stderr).contains("pricing.refresh_disabled"));
    assert_eq!(feed.count(), 1);
    let explicit = run(&config_path, &["--data-dir", data_arg, "pricing", "--refresh", "--json"]).await;
    assert_eq!(output_json(&explicit)["cacheStatus"], "fresh");
    assert_eq!(feed.count(), 2); // explicit pricing ignores the usage display switch

    cfg["pricing"]["source"] = json!("builtin");
    write_config(&config_path, &cfg);
    let offline = output_json(&run(&config_path, &["--data-dir", data_arg, "pricing", "--refresh", "--json"]).await);
    assert_eq!(offline["cacheStatus"], "disabled");
    assert_eq!(offline.get("fetchedAt"), Some(&Value::Null));
    assert!(offline["models"].as_array().unwrap().iter().any(|row| row["key"] == "openai/gpt-6.2-astra" && row["priceSource"] == "override"));
    assert_eq!(feed.count(), 2); // the healthy mirror was not contacted in builtin mode
    feed.stop().await;
}

fn append_usage(data: &Path, date: &str) {
    let usage = data.join("usage");
    fs::create_dir_all(&usage).unwrap();
    let ts = format!("{date}T12:00:00Z").parse::<chrono::DateTime<Utc>>().unwrap().timestamp_millis();
    let mut records = String::new();
    for (model, input, write) in [("gpt-6.2-sol", 1000, 0), ("gpt-6.2-sol", 20, 1), ("gpt-6.2-astra", 300, 0), ("custom", 100, 0), ("gpt-6-astra", 20, 0), ("unknown", 3, 0)] {
        let row = json!({"v":1,"ts":ts,"day":date,"provider":"openai","account":"test","model":model,
                         "tokens":{"input":input,"cacheWrite12h":write,"totalTokens":input}});
        records.push_str(&row.to_string());
        records.push('\n');
    }
    fs::write(usage.join(format!("{date}.jsonl")), records).unwrap();
}

#[tokio::test]
async fn serve_and_cli_share_a_stale_snapshot_during_blocked_and_failed_refreshes() {
    let feed = Feed::start(true).await;
    let dir = TempDir::new();
    let config_path = dir.0.join("config.json");
    let data = dir.0.join("data");
    let socket = dir.0.join("s");
    let token_hash: String = sha2::Sha256::digest(TOKEN.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
    let mut cfg = json!({
        "dataDir":data, "defaultProvider":"faux", "persistSessions":false, "profiles":{},
        "clients":[{"name":"test","tokenSha256":token_hash,"profiles":[]}],
        "pricing":{"modelsDevUrl":feed.url,"models":{"openai/custom":{"input":5,"output":10}}}
    });
    write_config(&config_path, &cfg);
    let first = output_json(&run(&config_path, &["pricing", "--refresh", "--json"]).await);
    assert_eq!(first["cacheStatus"], "fresh");
    let date = Utc::now().date_naive().to_string();
    append_usage(&data, &date);
    let cache_path = data.join("pricing/models-dev.json");
    let mut cache: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    let at = (Utc::now() - TimeDelta::hours(25)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    cache["fetchedAt"] = json!(at);
    fs::write(&cache_path, cache.to_string()).unwrap();
    let expected_bytes = fs::read(&cache_path).unwrap();

    // Both sockets are bound before the immediate worker tick. The API serves the
    // seeded catalogue even while this worker's GET is blocked at the fixture.
    let mut daemon = command(&config_path).args(["--log-dir", "-", "serve", "--bind", "none", "--unix-socket"]).arg(&socket).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while feed.count() < 2 {
            feed.started.notified().await;
        }
    })
    .await
    .unwrap();
    let client = agent_ai::http::client_builder().unix_socket(socket).timeout(Duration::from_secs(2)).build().unwrap();
    let params = format!("http://localhost/v1/usage?since={date}&until={date}&by=model");
    let http: Value = client.get(&params).bearer_auth(TOKEN).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(http["estimate"]["fetchedAt"], at);
    assert_eq!(http["estimate"]["cacheStatus"], "stale");
    assert_eq!(http["total"].get("priceSource"), Some(&Value::Null));
    assert_eq!(http["total"]["priceSources"], json!({"models.dev":3,"override":1,"builtin":1,"unknown":1}));
    assert_eq!(http["total"]["unpricedRequests"], 2);
    let sol = http["groups"].as_array().unwrap().iter().find(|row| row["key"]["model"] == "gpt-6.2-sol").unwrap();
    assert_eq!(sol["priceSource"], "models.dev");
    assert_eq!(sol["unpricedRequests"], 1);
    assert!(sol.get("estCost").is_none());
    let astra = http["groups"].as_array().unwrap().iter().find(|row| row["key"]["model"] == "gpt-6.2-astra").unwrap();
    assert_eq!(astra["estCost"], 0.0024);
    assert_eq!(feed.count(), 2);

    let out = run(&config_path, &["usage", "--by", "model", "--since", &date, "--until", &date, "--json"]).await;
    assert_eq!(output_json(&out), http);
    let log = String::from_utf8_lossy(&out.stderr);
    for field in ["pricing.cache_stale", "pricing.fetch_fail", "pricing.fallback", "chosen", "503", "bytes_received", "duration_ms"] {
        assert!(log.contains(field), "missing diagnostic {field}");
    }
    assert!(!log.contains("body-must-not-be-logged"));
    assert_eq!(feed.count(), 3);
    feed.stop().await;
    let forced = run(&config_path, &["usage", "--by", "model", "--since", &date, "--until", &date, "--refresh-prices", "--json"]).await;
    assert_eq!(output_json(&forced), http);
    let prices_offline = output_json(&run(&config_path, &["pricing", "openai/gpt-6.2-sol", "--refresh", "--json"]).await);
    assert_eq!(prices_offline["fetchedAt"], at);
    assert_eq!(prices_offline["models"][0]["priceSource"], "models.dev");
    assert_eq!(fs::read(cache_path).unwrap(), expected_bytes);

    cfg["pricing"]["source"] = json!("builtin");
    write_config(&config_path, &cfg);
    let offline = run(&config_path, &["pricing", "openai/custom", "--refresh", "--json"]).await;
    let prices = output_json(&offline);
    assert_eq!(prices["cacheStatus"], "disabled");
    assert_eq!(prices.get("fetchedAt"), Some(&Value::Null));
    assert_eq!(prices["models"][0]["priceSource"], "override");
    assert_eq!(prices["models"][0]["base"]["input"], 5.0);
    let log = String::from_utf8_lossy(&offline.stderr);
    assert!(log.contains("pricing.refresh_disabled") && !log.contains("pricing.fetch_start"));

    // Shutdown while the refresh is still in flight must not wait for its 15s budget.
    // SAFETY: `daemon` is live and its pid belongs to this test's child process.
    assert_eq!(unsafe { libc::kill(daemon.id().unwrap() as i32, libc::SIGTERM) }, 0);
    let exit = tokio::time::timeout(Duration::from_secs(5), daemon.wait()).await.unwrap().unwrap();
    assert!(exit.success());
}
