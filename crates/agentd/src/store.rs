//! Append-only restart journals. Secrets are deliberately absent from delivery records.
use crate::{
    hub::Task,
    issues::{IssueEvent, IssueScope, Reporter, Severity},
    webhook::{Deliverer, DeliveryState, DeliveryStatus, RetryPolicy, WebhookTarget},
};
use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug)]
pub struct DataDirLocked {
    pub path: PathBuf,
}

impl std::fmt::Display for DataDirLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dataDir {} is in use by another sub-bot server", self.path.display())
    }
}

impl std::error::Error for DataDirLocked {}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delivery {
    pub url: String,
    pub payload: serde_json::Value,
    pub client: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub index: Option<usize>,
    pub status: DeliveryStatus,
}

type DeliveryLane = (Option<String>, String);

#[derive(Default)]
pub struct Store {
    pub tasks: Mutex<HashMap<String, Task>>,
    deliveries: Mutex<HashMap<String, Delivery>>,
    journals: Option<Journals>,
    lanes: Mutex<HashMap<DeliveryLane, Arc<tokio::sync::Mutex<()>>>>,
    workers: Mutex<Vec<tokio::task::AbortHandle>>,
    corrupt_journals: Mutex<Vec<&'static str>>,
}

struct Journals {
    task_file: Mutex<File>,
    delivery_file: Mutex<File>,
    // Keep the same inode locked until all journal handles have been closed. Never unlink it.
    #[cfg(unix)]
    _lock: File,
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    Ok(options.open(path)?)
}

fn load<T: DeserializeOwned>(path: &Path) -> Result<(Vec<T>, bool)> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], false)),
        Err(e) => return Err(e.into()),
    };
    let mut records = vec![];
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        // A crash can tear the final append; quarantine completed corrupt records instead.
        if !line.ends_with(b"\n") {
            break;
        }
        match serde_json::from_slice(line) {
            Ok(record) => records.push(record),
            Err(_) => {
                // Tighten permissions before renaming, preserving the original bytes for diagnosis.
                private_file(path)?;
                let name = path.file_name().expect("journal filename").to_string_lossy();
                std::fs::rename(path, path.with_file_name(format!("{name}.corrupt-{}", crate::hub::unix_ms())))?;
                return Ok((records, true));
            }
        }
    }
    Ok((records, false))
}

fn append<T: Serialize>(file: &Mutex<File>, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = file.lock().unwrap();
    file.write_all(&bytes)?;
    file.sync_data()?;
    Ok(())
}

fn compact<T: Serialize>(dir: &Path, name: &str, records: impl Iterator<Item = T>) -> Result<File> {
    let path = dir.join(name);
    let temporary = dir.join(format!("{name}.tmp"));
    let file = private_file(&temporary)?;
    file.set_len(0)?;
    let file = Mutex::new(file);
    for record in records {
        append(&file, &record)?;
    }
    file.lock().unwrap().sync_all()?;
    std::fs::rename(temporary, &path)?;
    File::open(dir)?.sync_all()?;
    private_file(&path)
}

impl Store {
    /// Ephemeral task/delivery state: no filesystem access, recovery, or lock.
    pub fn memory() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn open(dir: &Path, ttl: Duration) -> Result<Arc<Self>> {
        crate::config::prepare_temp_data_dir(dir, true)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        let lock = {
            use std::os::fd::AsRawFd;
            let lock = private_file(&dir.join("lock"))?;
            // SAFETY: lock owns a valid descriptor for the duration of this call and the Store.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(DataDirLocked { path: dir.parent().unwrap_or(dir).to_path_buf() }.into());
                }
                return Err(anyhow::Error::new(error).context(format!("failed to lock {}", dir.join("lock").display())));
            }
            lock
        };
        let mut corrupt_journals = vec![];
        let (records, corrupt) = load::<Task>(&dir.join("tasks.jsonl"))?;
        if corrupt {
            corrupt_journals.push("tasks.jsonl");
        }
        let mut tasks = HashMap::new();
        for task in records {
            tasks.insert(task.task_id.clone(), task);
        }
        let now = crate::hub::unix_ms();
        tasks.retain(|_, task| task.finished_at.is_none_or(|finished| now.saturating_sub(finished) <= ttl.as_millis() as u64));
        let (records, corrupt) = load::<Delivery>(&dir.join("deliveries.jsonl"))?;
        if corrupt {
            corrupt_journals.push("deliveries.jsonl");
        }
        let mut deliveries = HashMap::new();
        for delivery in records {
            deliveries.insert(delivery.status.delivery_id.clone(), delivery);
        }
        for delivery in deliveries.values() {
            if let (Some(id), Some(index)) = (&delivery.task_id, delivery.index) {
                if let Some(task) = tasks.get_mut(id) {
                    if let Some(target) = task.deliveries.get_mut(index) {
                        target.status = delivery.status.clone();
                    }
                    if index == 0 && delivery.status.event == "task.completed" && delivery.status.state != DeliveryState::Pending {
                        task.delivery = Some(delivery.status.clone());
                    }
                }
            }
        }
        deliveries.retain(|_, d| d.status.state == DeliveryState::Pending);
        let task_file = compact(dir, "tasks.jsonl", tasks.values())?;
        let delivery_file = compact(dir, "deliveries.jsonl", deliveries.values())?;
        Ok(Arc::new(Self {
            tasks: Mutex::new(tasks),
            deliveries: Mutex::new(deliveries),
            journals: Some(Journals {
                task_file: Mutex::new(task_file),
                delivery_file: Mutex::new(delivery_file),
                #[cfg(unix)]
                _lock: lock,
            }),
            lanes: Mutex::new(HashMap::new()),
            workers: Mutex::new(vec![]),
            corrupt_journals: Mutex::new(corrupt_journals),
        }))
    }

    pub fn report_corruption(&self, reporter: &Reporter) {
        let journals = std::mem::take(&mut *self.corrupt_journals.lock().unwrap());
        if !journals.is_empty() {
            reporter.report(
                IssueEvent::new(Severity::Error, "store_corrupt", "store", "Corrupt journals quarantined; recovered records before corruption").details(serde_json::json!({"journals": journals})),
            );
        }
    }

    pub fn save_task(&self, task: &Task) -> Result<()> {
        if let Some(journals) = &self.journals {
            append(&journals.task_file, task)?;
        }
        Ok(())
    }

    pub fn pending(&self) -> Vec<Delivery> {
        self.deliveries.lock().unwrap().values().filter(|d| d.status.state == DeliveryState::Pending).cloned().collect()
    }

    pub fn save_delivery(&self, delivery: &Delivery) -> Result<()> {
        let mut deliveries = self.deliveries.lock().unwrap();
        if let Some(journals) = &self.journals {
            append(&journals.delivery_file, delivery)?;
        }
        if delivery.status.state == DeliveryState::Pending {
            deliveries.insert(delivery.status.delivery_id.clone(), delivery.clone());
        } else {
            deliveries.remove(&delivery.status.delivery_id);
        }
        if let (Some(id), Some(index)) = (&delivery.task_id, delivery.index) {
            if let Some(task) = self.tasks.lock().unwrap().get_mut(id) {
                if let Some(target) = task.deliveries.get_mut(index) {
                    target.status = delivery.status.clone();
                }
                if index == 0 && delivery.status.event == "task.completed" && delivery.status.state != DeliveryState::Pending {
                    task.delivery = Some(delivery.status.clone());
                }
                self.save_task(task)?;
            }
        }
        Ok(())
    }

    pub fn start(self: &Arc<Self>, delivery: Delivery, target: Option<WebhookTarget>, reporter: &Arc<Reporter>, policy: RetryPolicy) {
        let scope = IssueScope { client: delivery.client.clone(), session_id: delivery.session_id.clone(), task_id: delivery.task_id.clone(), ..Default::default() };
        let targets = reporter.subscription_targets(&scope);
        let lane = {
            let mut lanes = self.lanes.lock().unwrap();
            lanes.retain(|_, lane| Arc::strong_count(lane) > 1);
            lanes.entry((delivery.task_id.clone(), delivery.url.clone())).or_default().clone()
        };
        let weak = Arc::downgrade(self);
        let reporter = Arc::downgrade(reporter);
        let worker = tokio::spawn(async move {
            let _guard = lane.lock().await;
            let update = |status: &DeliveryStatus| {
                if let Some(store) = weak.upgrade() {
                    let mut record = delivery.clone();
                    record.status = status.clone();
                    if store.save_delivery(&record).is_err() {
                        if let Some(reporter) = reporter.upgrade() {
                            reporter.report(IssueEvent::new(Severity::Error, "store_write_failed", "store", "Delivery journal write failed").scope(scope.clone()));
                        }
                    }
                }
            };
            let status = if let Some(target) = &target {
                Deliverer::new(reqwest::Client::new(), policy).resume(target, &delivery.payload, delivery.status.clone(), &update).await
            } else {
                let mut status = delivery.status.clone();
                status.state = DeliveryState::Failed;
                status.last_error = Some("target no longer configured".into());
                status
            };
            update(&status);
            if let Some(reporter) = reporter.upgrade() {
                reporter.on_delivery_failed(&status, &delivery.url, scope.clone(), &targets);
                if status.state == DeliveryState::Failed && status.event == "issue" && delivery.payload["kind"] != "webhook_delivery_failed" {
                    reporter.report(Reporter::delivery_failure(&status, &delivery.url, scope.client.as_deref()));
                }
                if target.is_none() {
                    reporter.report(IssueEvent::new(Severity::Error, "webhook_target_removed", "store", "Pending webhook target no longer configured").scope(scope));
                }
            }
        });
        let mut workers = self.workers.lock().unwrap();
        workers.retain(|w| !w.is_finished());
        workers.push(worker.abort_handle());
    }

    pub async fn flush(&self) {
        while self.workers.lock().unwrap().iter().any(|w| !w.is_finished()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        for worker in self.workers.get_mut().unwrap().drain(..) {
            worker.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::TaskStatus;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn exclusive_lock_prevents_concurrent_journal_access_and_releases_on_drop() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let data_dir = std::env::temp_dir().join(format!("sub-bot-lock-{}", uuid::Uuid::new_v4()));
        let dir = data_dir.join("state");
        let ttl = Duration::from_secs(60);
        let store = Store::open(&dir, ttl).unwrap();
        let retained = store.clone();
        let metadata = std::fs::metadata(dir.join("lock")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        // A rejected opener must not even load/quarantine/compact the journals.
        private_file(&dir.join("tasks.jsonl")).unwrap().write_all(b"{torn").unwrap();
        drop(store);
        let error = Store::open(&dir, ttl).err().expect("second store must fail");
        assert_eq!(error.downcast_ref::<DataDirLocked>().expect("typed lock error").path, data_dir);
        assert_eq!(error.to_string(), format!("dataDir {} is in use by another sub-bot server", data_dir.display()));
        assert_eq!(std::fs::read(dir.join("tasks.jsonl")).unwrap(), b"{torn");
        let other = Store::open(&data_dir.join("other/state"), ttl).unwrap();
        drop(other);
        drop(retained);
        let reopened = Store::open(&dir, ttl).unwrap();
        assert_eq!(std::fs::metadata(dir.join("lock")).unwrap().ino(), metadata.ino());
        assert!(std::fs::read(dir.join("tasks.jsonl")).unwrap().is_empty());
        drop(reopened);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn load_compacts_latest_states_ttl_and_torn_tail() {
        let dir = std::env::temp_dir().join(format!("sub-bot-journal-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        let mut task: Task = serde_json::from_value(json!({
            "taskId": "task", "owner": "client", "sessionId": "session", "status": "queued", "createdAt": 0,
            "finishedAt": null, "result": null, "error": null, "delivery": null, "deliveries": []
        }))
        .unwrap();
        store.save_task(&task).unwrap();
        task.status = TaskStatus::Succeeded;
        task.finished_at = Some(crate::hub::unix_ms());
        store.save_task(&task).unwrap();
        let mut delivery = Delivery {
            url: "http://localhost/hook".into(),
            payload: json!({"ok": true}),
            client: Some("client".into()),
            session_id: None,
            task_id: None,
            index: None,
            status: DeliveryStatus { delivery_id: "pending".into(), event: "task.completed".into(), state: DeliveryState::Pending, attempts: 2, last_status: Some(500), last_error: None },
        };
        store.save_delivery(&delivery).unwrap();
        delivery.status.delivery_id = "done".into();
        store.save_delivery(&delivery).unwrap();
        delivery.status.state = DeliveryState::Delivered;
        store.save_delivery(&delivery).unwrap();
        drop(store);
        private_file(&dir.join("tasks.jsonl")).unwrap().write_all(b"{torn").unwrap();
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        assert_eq!(store.tasks.lock().unwrap()["task"].status, TaskStatus::Succeeded);
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.pending()[0].status.attempts, 2);
        assert_eq!(std::fs::read_to_string(dir.join("tasks.jsonl")).unwrap().lines().count(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("deliveries.jsonl")).unwrap().lines().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            for name in ["tasks.jsonl", "deliveries.jsonl"] {
                assert_eq!(std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        task.finished_at = Some(0);
        store.save_task(&task).unwrap();
        drop(store);
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        assert!(store.tasks.lock().unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn completed_corrupt_records_are_quarantined_and_reported_after_attachment() {
        let dir = std::env::temp_dir().join(format!("sub-bot-corrupt-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        let mut task: Task = serde_json::from_value(json!({
            "taskId": "before", "owner": "client", "sessionId": "session", "status": "queued", "createdAt": 0,
            "finishedAt": null, "result": null, "error": null, "delivery": null, "deliveries": []
        }))
        .unwrap();
        store.save_task(&task).unwrap();
        let mut delivery = Delivery {
            url: "http://localhost/hook".into(),
            payload: json!({"ok": true}),
            client: Some("client".into()),
            session_id: None,
            task_id: None,
            index: None,
            status: DeliveryStatus { delivery_id: "before".into(), event: "task.completed".into(), state: DeliveryState::Pending, attempts: 2, last_status: Some(500), last_error: None },
        };
        store.save_delivery(&delivery).unwrap();
        for name in ["tasks.jsonl", "deliveries.jsonl"] {
            private_file(&dir.join(name)).unwrap().write_all(b"broken\n").unwrap();
        }
        task.task_id = "after".into();
        store.save_task(&task).unwrap();
        delivery.status.delivery_id = "after".into();
        store.save_delivery(&delivery).unwrap();
        drop(store);
        let originals: Vec<_> = ["tasks.jsonl", "deliveries.jsonl"].iter().map(|name| std::fs::read(dir.join(name)).unwrap()).collect();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["tasks.jsonl", "deliveries.jsonl"] {
                std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        assert_eq!(store.tasks.lock().unwrap().len(), 1);
        assert!(store.tasks.lock().unwrap().contains_key("before"));
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.pending()[0].status.delivery_id, "before");
        assert_eq!(store.pending()[0].status.attempts, 2);
        for (name, original) in ["tasks.jsonl", "deliveries.jsonl"].iter().zip(originals) {
            let backups: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{name}.corrupt-"))).collect();
            assert_eq!(backups.len(), 1);
            let suffix = backups[0].file_name().unwrap().to_string_lossy();
            assert!(suffix.strip_prefix(&format!("{name}.corrupt-")).unwrap().parse::<u64>().is_ok());
            assert_eq!(std::fs::read(&backups[0]).unwrap(), original);
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap().lines().count(), 1);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for path in [&backups[0], &dir.join(name)] {
                    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
                }
            }
        }
        let reporter = Reporter::new(&crate::config::AlertsConfig::default(), None, vec![]);
        reporter.flush(Duration::from_secs(1)).await;
        assert!(reporter.recent(None, 0, 100).is_empty());
        reporter.attach_store(&store);
        reporter.attach_store(&store);
        reporter.flush(Duration::from_secs(1)).await;
        let events = reporter.recent(None, 0, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "store_corrupt");
        assert_eq!(events[0].severity, Severity::Error);
        assert_eq!(events[0].details["journals"], json!(["tasks.jsonl", "deliveries.jsonl"]));
        drop(store);
        let store = Store::open(&dir, Duration::from_secs(60)).unwrap();
        assert_eq!(store.tasks.lock().unwrap().len(), 1);
        assert_eq!(store.pending().len(), 1);
        assert!(store.corrupt_journals.lock().unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
