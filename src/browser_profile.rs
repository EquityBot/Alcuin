//! Native persistent-profile admission. Records protect recovery independently of capacity.
//! Reference: profileState.ts, tabLeaseRegistry.ts and their ownership/retirement tests.
use crate::{
    native_fs::NativeEntry,
    process::{Liveness, ProcessIdentity},
    sessions::{merge_object, timestamp},
    storage::{DirectoryLock, atomic_json, atomic_json_until, private_dir},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(test)]
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_TABS: usize = 3;
pub const MAX_WAITERS: usize = 32;
const STALE_MS: i64 = 120_000;
const REGISTRY: &str = "oracle-tab-leases.json";
const WAITERS: &str = "oracle-tab-waiters.json";
/// Outside the tree being deleted, so removing profile contents cannot reopen admission.
pub(crate) fn fresh_disposal_marker(profile: &Path) -> Option<PathBuf> {
    let name = profile.file_name()?.to_str()?;
    uuid::Uuid::parse_str(name.strip_prefix("alcuin-browser-")?).ok()?;
    Some(profile.with_extension("retiring.json"))
}
fn deadline_cleanup() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(1)
}
const ATTACHMENTS: &str = "oracle-browser-attachments.json";
const IDLE_OWNER: &str = "oracle-browser-idle-owner.json";
// shortcut: a live process's marker expires after ten minutes, bounding one whose Drop could
// not confirm removal in a long-lived server. Only the attach-to-admission gap relies on it;
// a waiter or lease refuses cleanup afterwards. A clock stepped back keeps it active.
const ATTACHMENT_MS: i64 = 600_000;
fn attachment_active(row: &Value, now: i64) -> bool {
    record_liveness(row) != Liveness::Dead
        && row["createdAtMs"]
            .as_i64()
            .is_some_and(|at| now - at < ATTACHMENT_MS)
}
/// A borrowed attachment to a persistent browser. Idle cleanup refuses while one is held,
/// so Chrome cannot exit before that run registers admission. The run releases its own
/// marker once it holds no admission; Drop removes exactly that marker.
pub(crate) struct AttachMarker {
    registry: Arc<ProfileRegistry>,
    id: String,
}
impl Drop for AttachMarker {
    fn drop(&mut self) {
        let lock_path = self.registry.path.join("oracle-tab-leases.lock");
        let removed =
            DirectoryLock::acquire_until(&lock_path, deadline_cleanup()).and_then(|lock| {
                self.registry
                    .edit_attachments(Some(deadline_cleanup()), |rows| {
                        rows.retain(|row| row["id"] != self.id.as_str());
                    })?;
                lock.release()
            });
        if let Err(error) = removed {
            eprintln!(
                "Borrowed attachment marker remains until its process exits or it expires: {error:#}"
            );
        }
    }
}

struct AdmissionDelivery {
    lease: Option<TabLease>,
    runtime: tokio::runtime::Handle,
}
#[derive(Debug)]
pub(crate) struct UnsettledAdmission(String);
impl std::fmt::Display for UnsettledAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for UnsettledAdmission {}
impl Drop for AdmissionDelivery {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            // A dropped receiver grants no target authority. Keep its exact cleanup off the runtime.
            drop(self.runtime.spawn_blocking(move || {
                if lease.release_unissued_before(deadline_cleanup()).is_err() {
                    eprintln!("Unused browser admission retained after abandoned delivery; cleanup was not confirmed");
                }
            }));
        }
    }
}

pub struct ProfileRegistry {
    path: PathBuf,
    directory: NativeEntry,
}
impl ProfileRegistry {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        private_dir(path)?;
        Self::open_existing(path)
    }
    pub(crate) fn open_existing(path: &Path) -> Result<Arc<Self>> {
        let directory = NativeEntry::open(path, true)?;
        let canonical = fs::canonicalize(path)?;
        directory.require_matches(&canonical)?;
        Ok(Arc::new(Self {
            path: canonical,
            directory,
        }))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn check(&self) -> Result<()> {
        self.directory.require_matches(&self.path)
    }
    fn read(&self, filename: &str, limit: u64, default: Value) -> Result<Value> {
        self.check()?;
        let path = self.path.join(filename);
        let mut entry = match NativeEntry::open(&path, false) {
            Ok(entry) => entry,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(default);
            }
            Err(error) => {
                return Err(
                    error.context("Unable to verify profile control record; preserving ownership")
                );
            }
        };
        anyhow::ensure!(
            entry.handle.metadata()?.len() <= limit,
            "Profile control record exceeds its limit"
        );
        let mut bytes = Vec::new();
        entry
            .handle
            .by_ref()
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= limit,
            "Profile control record exceeds its limit"
        );
        entry.require_matches(&path)?;
        self.check()?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn write_until(
        &self,
        filename: &str,
        value: &Value,
        deadline: Option<std::time::Instant>,
    ) -> Result<()> {
        self.check()?;
        let path = self.path.join(filename);
        if path.symlink_metadata().is_ok() {
            NativeEntry::open(&path, false)?;
        }
        match deadline {
            Some(deadline) => atomic_json_until(&path, value, deadline)?,
            None => atomic_json(&path, value)?,
        }
        self.check()
    }
    fn transaction<T>(&self, work: impl FnOnce(&mut Value, &mut Value) -> Result<T>) -> Result<T> {
        self.transaction_until(None, work)
    }
    fn transaction_until<T>(
        &self,
        deadline: Option<std::time::Instant>,
        work: impl FnOnce(&mut Value, &mut Value) -> Result<T>,
    ) -> Result<T> {
        self.check()?;
        let lock_path = self.path.join("oracle-tab-leases.lock");
        let lock = match deadline {
            Some(deadline) => DirectoryLock::acquire_until(&lock_path, deadline)?,
            None => DirectoryLock::acquire(&lock_path, Duration::from_secs(1))?,
        };
        self.transaction_locked(lock, deadline, work)
    }
    fn transaction_locked<T>(
        &self,
        lock: DirectoryLock,
        deadline: Option<std::time::Instant>,
        work: impl FnOnce(&mut Value, &mut Value) -> Result<T>,
    ) -> Result<T> {
        self.check()?;
        let mut registry =
            self.read(REGISTRY, 2 * 1024 * 1024, json!({"version":1,"leases":[]}))?;
        let mut waiters = self.read(WAITERS, 64 * 1024, json!({"version":1,"waiters":[]}))?;
        validate_registry(&registry, &waiters)?;
        normalise_native_owner_timestamps(&mut registry["leases"]);
        normalise_native_owner_timestamps(&mut waiters["waiters"]);
        let before_registry = registry.clone();
        let before_waiters = waiters.clone();
        let value = work(&mut registry, &mut waiters)?;
        validate_registry(&registry, &waiters)?;
        anyhow::ensure!(
            deadline.is_none_or(|deadline| std::time::Instant::now() < deadline),
            "Browser profile transaction deadline reached"
        );
        // Write the real lease before retiring its waiter. A duplicate token reconciles later.
        if registry != before_registry {
            self.write_until(REGISTRY, &registry, deadline)?;
        }
        if waiters != before_waiters {
            self.write_until(WAITERS, &waiters, deadline)?;
        }
        lock.release()?;
        Ok(value)
    }
    pub async fn acquire(
        self: &Arc<Self>,
        session_id: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<TabLease> {
        self.acquire_with_limit(session_id, MAX_TABS, timeout, cancel)
            .await
    }
    pub async fn acquire_with_limit(
        self: &Arc<Self>,
        session_id: &str,
        max_tabs: usize,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<TabLease> {
        self.acquire_admission(session_id, max_tabs, timeout, cancel, false)
            .await
    }
    /// `local_chrome`: records here describe a locally launched Chrome, whose refused
    /// loopback port proves it exited (reference `localChrome`).
    pub(crate) async fn acquire_admission(
        self: &Arc<Self>,
        session_id: &str,
        max_tabs: usize,
        timeout: Duration,
        cancel: &CancellationToken,
        local_chrome: bool,
    ) -> Result<TabLease> {
        anyhow::ensure!(max_tabs > 0, "Browser tab limit must be positive");
        crate::sessions::assert_id(session_id)?;
        check_cancel(cancel)?;
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .context("Invalid native admission deadline")?;
        // Removing exited controllers' records is optional upkeep beside admission, on its own
        // budget and committed under the registry lock, so it never delays or fails admission.
        let reaper = self.clone();
        let reap_cancel = cancel.clone();
        drop(tokio::spawn(async move {
            if let Err(error) = reaper
                .reap_absent_target_leases(local_chrome, &reap_cancel)
                .await
                && !reap_cancel.is_cancelled()
            {
                eprintln!("[browser] Browser slot record cleanup skipped: {error}");
            }
        }));
        let registry = self.clone();
        let session_id = session_id.to_owned();
        let worker_cancel = cancel.child_token();
        let _cancel_on_drop = worker_cancel.clone().drop_guard();
        let runtime = tokio::runtime::Handle::current();
        // The complete native transaction and its cleanup stay on the same blocking worker.
        let mut delivery = tokio::task::spawn_blocking(move || {
            registry
                .acquire_blocking(&session_id, max_tabs, deadline, &worker_cancel)
                .map(|lease| AdmissionDelivery {
                    lease: Some(lease),
                    runtime,
                })
        })
        .await
        .map_err(|error|UnsettledAdmission(format!("Browser admission worker failed; exact admission state requires inspection: {error}")))??;
        if cancel.is_cancelled() {
            let lease = Arc::new(
                delivery
                    .lease
                    .take()
                    .context("Browser admission delivery is missing")?,
            );
            let cleanup = lease.release_unissued(deadline_cleanup()).await;
            if let Err(cleanup) = cleanup {
                return Err(UnsettledAdmission(format!("Browser admission was cancelled; exact admission cleanup remains pending: {cleanup:#}")).into());
            }
            bail!("Browser admission was cancelled");
        }
        delivery
            .lease
            .take()
            .context("Browser admission delivery is missing")
    }
    fn acquire_blocking(
        self: &Arc<Self>,
        session_id: &str,
        max_tabs: usize,
        native_deadline: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<TabLease> {
        let process = ProcessIdentity::current()?;
        let created = timestamp();
        let mut waiter_id = Uuid::new_v4().to_string();
        let lease_id = Uuid::new_v4().to_string();
        let mut registered = false;
        let started = std::time::Instant::now();
        let mut holding = "unobserved".to_owned();
        let mut last_wait_line: Option<std::time::Instant> = None;
        let slot_timeout = |holding: &str| {
            format!(
                "Timed out waiting for ChatGPT browser slot after {}s ({max_tabs} max; holdingSessions={holding}).",
                (started.elapsed().as_millis() as f64 / 1000.0).round()
            )
        };
        let result = (|| {
            loop {
                check_cancel(cancel)?;
                anyhow::ensure!(
                    std::time::Instant::now() < native_deadline,
                    "{}",
                    slot_timeout(&holding)
                );
                self.check()?;
                let lock = DirectoryLock::acquire_cancellable_until(
                    &self.path.join("oracle-tab-leases.lock"),
                    native_deadline,
                    cancel,
                )?;
                let mut settled = Vec::new();
                let (admitted, position, maintenance_wait) = self.transaction_locked(lock, Some(native_deadline), |registry, waiters| {
                    check_cancel(cancel)?;
                    settled = retry_failed_releases(&self.path, registry, waiters);
                    let retiring = fresh_disposal_marker(&self.path).map(|path| match fs::symlink_metadata(path) {
                        Ok(_) => Ok(true),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                        Err(error) => Err(error),
                    }).transpose()?.unwrap_or(false);
                    anyhow::ensure!(!retiring, "Fresh browser profile is being retired; admission is closed");
                    prune(registry, chrono::Utc::now().timestamp_millis())?;
                    let leases = registry["leases"].as_array().unwrap();
                    holding = holding_sessions(leases.iter().filter(|l| l["capacityReleased"] != true));
                    let records = waiters["waiters"].as_array_mut().unwrap();
                    records.retain(|w| !leases.iter().any(|l| l["waiterId"] == w["id"] && same_admission(l, w))
                        && (fresh(w, chrono::Utc::now().timestamp_millis()) || record_liveness(w) != Liveness::Dead));
                    if let Some(prior) = leases.iter().find(|l| l["id"] == lease_id && l["waiterId"] == waiter_id && l["processIdentity"] == json!(process) && l["sessionId"] == session_id) {
                        return Ok((Some(prior.clone()), 1, false));
                    }
                    let index = records.iter().position(|w| w["id"] == waiter_id && w["processIdentity"] == json!(process) && w["sessionId"] == session_id);
                    let index = match index {
                        Some(i) => { records[i]["updatedAt"] = json!(timestamp()); i },
                        None => {
                            anyhow::ensure!(records.len() < MAX_WAITERS, "ChatGPT browser admission queue is full (32 pending controllers); retry later.");
                            if registered {
                                waiter_id = Uuid::new_v4().to_string();
                                eprintln!("[browser] Expired admission token reclaimed; queue position reset to the tail.");
                            }
                            records.push(json!({"id":waiter_id,"pid":process.pid,"processStartedAtMs":process.started_at_ms,"processIdentity":process,"sessionId":session_id,"maxConcurrentTabs":max_tabs,"createdAt":created,"updatedAt":timestamp()}));
                            registered = true;
                            records.len() - 1
                        }
                    };
                    let maintenance = self.read("oracle-tab-maintenance.json", 4096, Value::Null)?;
                    // A malformed, unknown or live maintenance owner blocks all admissions.
                    if !maintenance.is_null() && (maintenance["version"] != 1 || record_liveness(&maintenance) != Liveness::Dead) { return Ok((None, index + 1, index == 0)); }
                    if index != 0 || leases.iter().filter(|l| l["capacityReleased"] != true).count() >= max_tabs { return Ok((None, index + 1, false)); }
                    let lease = json!({"id":lease_id,"waiterId":waiter_id,"pid":process.pid,"processStartedAtMs":process.started_at_ms,"processIdentity":process,"sessionId":session_id,"createdAt":created,"updatedAt":timestamp()});
                    registry["leases"].as_array_mut().unwrap().push(lease.clone());
                    records.remove(index);
                    Ok((Some(lease), 1, false))
                })?;
                forget_settled_releases(&self.path, &settled);
                if let Some(owner) = admitted {
                    // Cancellation after promotion is handled by the same exact release authority.
                    let lease = TabLease {
                        registry: self.clone(),
                        owner,
                    };
                    if cancel.is_cancelled() {
                        registered = false;
                        if let Err(cleanup) = lease.release_unissued_before(deadline_cleanup()) {
                            return Err(UnsettledAdmission(format!("Browser admission was cancelled; exact admission cleanup remains pending: {cleanup:#}")).into());
                        }
                        bail!("Browser admission was cancelled");
                    }
                    eprintln!(
                        "[browser] Acquired ChatGPT browser slot {} ({max_tabs} max).",
                        &lease_id[..8]
                    );
                    return Ok(lease);
                }
                if last_wait_line.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                    eprintln!(
                        "[browser] Waiting for ChatGPT browser slot{} ({max_tabs} max, position {position}, {}s elapsed; holdingSessions={holding}).",
                        if maintenance_wait {
                            " during profile maintenance"
                        } else {
                            ""
                        },
                        (started.elapsed().as_millis() as f64 / 1000.0).round()
                    );
                    last_wait_line = Some(std::time::Instant::now());
                }
                std::thread::sleep(
                    Duration::from_millis(50)
                        .min(native_deadline.saturating_duration_since(std::time::Instant::now())),
                );
            }
        })();
        // A lock or transaction deadline after this wait's own deadline is this wait's timeout,
        // reported with its original cause (reference 811b290). Cancellation stays cancellation.
        let result = result.map_err(|error| {
            if !cancel.is_cancelled()
                && std::time::Instant::now() >= native_deadline
                && !error.to_string().contains("holdingSessions=")
            {
                error.context(slot_timeout(&holding))
            } else {
                error
            }
        });
        if result.is_err() && registered {
            let origin = json!({"id":lease_id,"waiterId":waiter_id,"processIdentity":process,"sessionId":session_id,"createdAt":created});
            let cleanup = self.transaction_until(Some(deadline_cleanup()), |registry, waiters| {
                retire_unissued_admission(registry, waiters, &origin);
                Ok(())
            });
            if let Err(cleanup) = cleanup {
                retain_failed_release(&self.path, &origin, &cleanup);
                let error = result
                    .err()
                    .context("Admission cleanup has no original failure")?;
                return Err(UnsettledAdmission(format!(
                    "{error:#}; exact browser admission cleanup remains pending: {cleanup:#}"
                ))
                .into());
            }
        }
        result
    }
    pub fn records(&self) -> Result<Value> {
        self.transaction(|registry, _| Ok(registry.clone()))
    }
    /// Hold admission excluded until an empty disposable browser has exited.
    pub(crate) fn empty_disposal_lock(
        &self,
        deadline: std::time::Instant,
    ) -> Result<DirectoryLock> {
        self.check()?;
        let lock =
            DirectoryLock::acquire_until(&self.path.join("oracle-tab-leases.lock"), deadline)?;
        let registry = self.read(REGISTRY, 2 * 1024 * 1024, json!({"version":1,"leases":[]}))?;
        let waiters = self.read(WAITERS, 64 * 1024, json!({"version":1,"waiters":[]}))?;
        validate_registry(&registry, &waiters)?;
        anyhow::ensure!(
            registry["leases"].as_array().is_some_and(Vec::is_empty)
                && waiters["waiters"].as_array().is_some_and(Vec::is_empty),
            "Fresh browser still has admission or recovery work; preserving its process"
        );
        anyhow::ensure!(
            !self.edit_attachments(Some(deadline), |rows| !rows.is_empty())?,
            "A borrowed attachment has not reached admission; preserving its browser"
        );
        self.check()?;
        Ok(lock)
    }
    /// Caller holds the tab-lease lock. Expired and dead-process markers are pruned.
    fn edit_attachments<T>(
        &self,
        deadline: Option<std::time::Instant>,
        edit: impl FnOnce(&mut Vec<Value>) -> T,
    ) -> Result<T> {
        let mut value = self.read(
            ATTACHMENTS,
            64 * 1024,
            json!({"version":1,"attachments":[]}),
        )?;
        anyhow::ensure!(
            value["version"] == 1 && value["attachments"].is_array(),
            "Browser attachment markers are malformed; preserving ownership"
        );
        let before = value.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let rows = value["attachments"].as_array_mut().unwrap();
        rows.retain(|row| attachment_active(row, now));
        let result = edit(rows);
        anyhow::ensure!(
            rows.len() <= MAX_WAITERS,
            "Too many borrowed browser attachments are pending"
        );
        if value != before {
            self.write_until(ATTACHMENTS, &value, deadline)?;
        }
        Ok(result)
    }
    pub(crate) fn set_idle_owner(&self, session: &str) -> Result<()> {
        crate::sessions::assert_id(session)?;
        self.write_until(
            IDLE_OWNER,
            &json!({"version":1,"sessionId":session}),
            Some(std::time::Instant::now() + Duration::from_secs(1)),
        )
    }
    /// A launch without idle authority replaces any earlier owner's browser.
    pub(crate) fn clear_idle_owner(&self) -> Result<()> {
        match fs::remove_file(self.path.join(IDLE_OWNER)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
    pub(crate) fn idle_owner(&self) -> Option<String> {
        let value = self.read(IDLE_OWNER, 4096, Value::Null).ok()?;
        let session = value["sessionId"]
            .as_str()
            .filter(|_| value["version"] == 1)?;
        crate::sessions::assert_id(session).ok()?;
        Some(session.to_owned())
    }
    /// Recorded under the bring-up lock, before a borrowed attachment releases it.
    pub(crate) fn mark_attachment(
        self: &Arc<Self>,
        deadline: std::time::Instant,
    ) -> Result<AttachMarker> {
        let id = Uuid::new_v4().to_string();
        let process = ProcessIdentity::current()?;
        let lock =
            DirectoryLock::acquire_until(&self.path.join("oracle-tab-leases.lock"), deadline)?;
        self.edit_attachments(Some(deadline), |rows| {
            rows.push(json!({"id":id,"processIdentity":process,"createdAtMs":chrono::Utc::now().timestamp_millis()}));
        })?;
        lock.release()?;
        Ok(AttachMarker {
            registry: Arc::clone(self),
            id,
        })
    }
    pub(crate) fn records_until(&self, deadline: std::time::Instant) -> Result<Value> {
        self.transaction_until(Some(deadline), |registry, _| Ok(registry.clone()))
    }
    pub fn release_disposed(
        &self,
        journal: &crate::browser_disposition::TargetJournal,
        creator: &Value,
        disposed: &Value,
        deadline: std::time::Instant,
        allow_current_owner: bool,
    ) -> Result<bool> {
        let require_receipt = || -> Result<Value> {
            let record = journal.read(&creator["allocationId"])?;
            anyhow::ensure!(
                creator["controllerStartedAtMs"]
                    .as_f64()
                    .is_some_and(f64::is_finite),
                "Target creator start is unverified; retaining its browser slot"
            );
            anyhow::ensure!(
                record["creator"] == *creator
                    && record["disposition"] == *disposed
                    && disposed["version"] == 1
                    && disposed["allocationId"] == creator["allocationId"]
                    && disposed["decision"] == "retired"
                    && disposed["absence"] == "confirmed-absent"
                    && disposed["target"] == creator["target"],
                "Exact durable target absence receipt is missing; retaining its browser slot"
            );
            let current = ProcessIdentity::current()?;
            let dead = if let Some(native) = record.get("nativeOwnerIdentity") {
                let identity: ProcessIdentity = serde_json::from_value(native.clone())?;
                anyhow::ensure!(
                    creator["controllerPid"] == identity.pid
                        && creator["controllerStartedAtMs"].as_f64()
                            == Some(identity.started_at_ms),
                    "Target creator native identity changed"
                );
                identity.liveness() == Liveness::Dead || allow_current_owner && identity == current
            } else {
                let pid = creator["controllerPid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .context("Invalid target creator PID")?;
                match ProcessIdentity::read(pid) {
                    Ok(None) => true,
                    Ok(Some(actual)) => {
                        creator["controllerStartedAtMs"]
                            .as_f64()
                            .is_some_and(|start| start != actual.started_at_ms)
                            || allow_current_owner
                                && actual == current
                                && creator["controllerStartedAtMs"].as_f64()
                                    == Some(actual.started_at_ms)
                    }
                    Err(_) => false,
                }
            };
            anyhow::ensure!(
                dead,
                "Target creator remains live or unverified; retaining its slot"
            );
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Browser lease release deadline reached"
            );
            Ok(record["nativeOwnerIdentity"].clone())
        };
        require_receipt()?;
        let profile = creator["lease"]["profileDir"]
            .as_str()
            .context("Target creator profile is missing")?;
        anyhow::ensure!(
            crate::storage::same_directory(&self.path, Path::new(profile)),
            "Target creator profile changed"
        );
        let lease_id = creator["lease"]["id"]
            .as_str()
            .context("Target creator lease is missing")?;
        let expected = self.transaction_until(Some(deadline), |registry, _| {
            Ok(registry["leases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|lease| lease["id"] == lease_id)
                .cloned())
        })?;
        let Some(expected) = expected else {
            return Ok(false);
        };
        self.transaction_until(Some(deadline), |registry, waiters| {
            let native = require_receipt()?;
            let rows = registry["leases"].as_array_mut().unwrap();
            let Some(index) = rows.iter().position(|lease| lease["id"] == lease_id) else {
                return Ok(false);
            };
            let lease = &rows[index];
            anyhow::ensure!(
                same_owner(lease, &expected),
                "Browser slot changed owners during release; preserving replacement"
            );
            anyhow::ensure!(
                lease["pid"] == creator["controllerPid"]
                    && lease["processStartedAtMs"] == creator["controllerStartedAtMs"]
                    && lease["sessionId"]
                        == *crate::browser_disposition::creator_admission(creator)
                    && lease["chromeTargetId"] == creator["target"]["targetId"]
                    && lease["chromePort"] == creator["target"]["port"]
                    && lease["targetDispositionAllocationId"] == creator["allocationId"]
                    && lease["chromeHost"]
                        .as_str()
                        .zip(creator["target"]["host"].as_str())
                        .is_some_and(|(a, b)| crate::browser::normalise_host(a)
                            == crate::browser::normalise_host(b)),
                "Browser slot changed owners or targets; preserving replacement"
            );
            if !native.is_null() {
                anyhow::ensure!(
                    lease
                        .get("processIdentity")
                        .is_none_or(|identity| identity == &native),
                    "Browser slot native owner changed; preserving replacement"
                );
            }
            let removed = rows.remove(index);
            waiters["waiters"].as_array_mut().unwrap().retain(|waiter| {
                !(waiter["id"] == removed["waiterId"] && same_admission(waiter, &removed))
            });
            Ok(true)
        })
    }
    pub(crate) fn recovery_lease(
        self: &Arc<Self>,
        id: &str,
        session_id: &str,
        host: &str,
        port: u16,
        target_id: &str,
    ) -> Result<Arc<TabLease>> {
        let owner = self.transaction(|registry, _| {
            let owner = registry["leases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["id"] == id)
                .context("Recovery lease is missing; no target-retirement authority")?;
            anyhow::ensure!(
                owner["sessionId"] == session_id
                    && owner["chromeTargetId"] == target_id
                    && owner["chromePort"].as_u64() == Some(u64::from(port))
                    && owner["chromeHost"]
                        .as_str()
                        .is_some_and(|h| crate::browser::normalise_host(h)
                            == crate::browser::normalise_host(host)),
                "Recovery lease belongs to a different target or endpoint"
            );
            let current = ProcessIdentity::current()?;
            anyhow::ensure!(
                record_liveness(owner) == Liveness::Dead
                    || owner["processIdentity"] == json!(current),
                "Recovery target still has a live or unknown foreign controller"
            );
            Ok(owner.clone())
        })?;
        Ok(Arc::new(TabLease {
            registry: self.clone(),
            owner,
        }))
    }
    pub fn reclaim_dead_capacity(&self) -> Result<Value> {
        self.transaction(|registry, _| {
            prune(registry, chrono::Utc::now().timestamp_millis())?;
            Ok(registry.clone())
        })
    }
}

pub struct TabLease {
    registry: Arc<ProfileRegistry>,
    owner: Value,
}
pub struct LeaseHeartbeat {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
impl TabLease {
    pub fn id(&self) -> &str {
        self.owner["id"].as_str().unwrap()
    }
    pub fn record(&self) -> &Value {
        &self.owner
    }
    pub fn heartbeat(self: &Arc<Self>) -> LeaseHeartbeat {
        let lease = self.clone();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            let mut warned = false;
            loop {
                tokio::select! {biased;_=cancelled.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(15))=>{}}
                let beat = lease.clone();
                match tokio::task::spawn_blocking(move || beat.update(&json!({}))).await {
                    Ok(Ok(())) => warned = false,
                    Ok(Err(error)) if !warned => {
                        warned = true;
                        eprintln!(
                            "[browser] ChatGPT browser slot heartbeat failed; preserving the slot until stale grace expires: {error}"
                        );
                    }
                    _ => {}
                }
            }
        });
        LeaseHeartbeat { stop, task }
    }
    pub fn require(&self) -> Result<()> {
        self.registry.transaction(|registry, _| {
            self.owned(registry)?;
            Ok(())
        })
    }
    pub(crate) fn require_until(&self, deadline: std::time::Instant) -> Result<()> {
        self.registry
            .transaction_until(Some(deadline), |registry, _| {
                self.owned(registry)?;
                Ok(())
            })
    }
    pub(crate) async fn require_unsubmitted_target(
        self: &Arc<Self>,
        target: &str,
        deadline: std::time::Instant,
    ) -> Result<()> {
        let lease = self.clone();
        let target = target.to_owned();
        tokio::task::spawn_blocking(move || {
            lease
                .registry
                .transaction_until(Some(deadline), |registry, _| {
                    let record = lease.owned(registry)?;
                    anyhow::ensure!(
                        record["chromeTargetId"] == target
                            && record["promptSubmitted"] != true
                            && record["observationIntent"].is_null(),
                        "Unsubmitted cleanup target authority changed; retaining recovery"
                    );
                    Ok(())
                })
        })
        .await
        .context("Cleanup authority check worker failed")?
    }
    fn owned<'a>(&self, registry: &'a mut Value) -> Result<&'a mut Value> {
        let record = registry["leases"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|l| l["id"] == self.owner["id"])
            .context("Browser slot ownership was lost; preserving replacement")?;
        anyhow::ensure!(
            same_owner(record, &self.owner),
            "Browser slot changed owners; preserving replacement"
        );
        Ok(record)
    }
    pub fn update(&self, patch: &Value) -> Result<()> {
        self.update_before(patch, None)
    }
    pub(crate) fn update_until(&self, patch: &Value, deadline: std::time::Instant) -> Result<()> {
        self.update_before(patch, Some(deadline))
    }
    fn update_before(&self, patch: &Value, deadline: Option<std::time::Instant>) -> Result<()> {
        anyhow::ensure!(patch.is_object(), "Invalid lease patch");
        for key in [
            "id",
            "waiterId",
            "pid",
            "processStartedAtMs",
            "processIdentity",
            "sessionId",
            "createdAt",
            "capacityReleased",
            "observationIntent",
        ] {
            anyhow::ensure!(
                patch.get(key).is_none(),
                "Lease patch cannot change ownership or capacity"
            );
        }
        self.registry.transaction_until(deadline, |registry, _| {
            let record = self.owned(registry)?;
            *record = merge_object(record, patch);
            record["updatedAt"] = json!(timestamp());
            Ok(())
        })
    }
    pub(crate) fn begin_allocation(
        &self,
        observation: Option<&Value>,
        deadline: std::time::Instant,
    ) -> Result<()> {
        self.registry
            .transaction_until(Some(deadline), |registry, _| {
                let record = self.owned(registry)?;
                anyhow::ensure!(
                    record["targetDispositionPending"] != true
                        && record["chromeTargetId"].is_null()
                        && record["targetDispositionAllocationId"].is_null()
                        && record["observationIntent"].is_null()
                        && record["capacityReleased"] != true,
                    "Browser lease already has an allocation; refusing replay"
                );
                record["targetDispositionPending"] = json!(true);
                if let Some(intent) = observation {
                    anyhow::ensure!(
                        intent["version"] == 1
                            && intent["id"]
                                .as_str()
                                .is_some_and(|id| Uuid::parse_str(id).is_ok())
                            && intent["url"]
                                .as_str()
                                .and_then(crate::browser::conversation_id)
                                .is_some(),
                        "Observation intent is unverified"
                    );
                    record["observationIntent"] = intent.clone();
                }
                record["updatedAt"] = json!(timestamp());
                Ok(())
            })
    }
    pub(crate) fn handover_observation(
        &self,
        creation: &crate::browser_disposition::TargetCreation,
        target_id: &str,
        url: &str,
        deadline: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.handover_observation_with(creation, target_id, url, deadline, cancel, || Ok(()))
    }
    fn owned_observation(
        &self,
        registry: &mut Value,
        creation: &crate::browser_disposition::TargetCreation,
        target_id: &str,
        url: &str,
    ) -> Result<Value> {
        anyhow::ensure!(
            creation.is_observation_for(target_id) && creation.observation_url() == Some(url),
            "Only the exact newly allocated observation has this authority"
        );
        anyhow::ensure!(
            self.owner["processIdentity"] == json!(ProcessIdentity::current()?),
            "Observation requires its exact current controller"
        );
        let owned = self.owned(registry)?.clone();
        anyhow::ensure!(
            owned["observationIntent"]
                == json!({"version":1,"id":creation.allocation_id(),"url":url})
                && owned["chromeTargetId"] == target_id
                && owned["targetDispositionPending"] == true
                && owned["targetDispositionAllocationId"].is_null()
                && owned["capacityReleased"] != true
                && owned["promptSubmitted"] != true
                && recoverable(&owned),
            "Observation allocation changed or has consultation authority; retaining admission"
        );
        Ok(owned)
    }
    pub(crate) fn require_observation(
        &self,
        creation: &crate::browser_disposition::TargetCreation,
        target_id: &str,
        url: &str,
        deadline: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<()> {
        check_cancel(cancel)?;
        self.registry
            .transaction_until(Some(deadline), |registry, _| {
                check_cancel(cancel)?;
                self.owned_observation(registry, creation, target_id, url)?;
                Ok(())
            })
    }
    #[allow(clippy::too_many_arguments)] // The hook exercises interruption after durable permission, before admission release.
    fn handover_observation_with(
        &self,
        creation: &crate::browser_disposition::TargetCreation,
        target_id: &str,
        url: &str,
        deadline: std::time::Instant,
        cancel: &CancellationToken,
        before_release: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        anyhow::ensure!(
            creation.is_observation_for(target_id),
            "Only a newly allocated observation can be handed over"
        );
        let intent = json!({"version":1,"id":creation.allocation_id(),"url":url});
        let filename = format!(
            "oracle-observation-handover-{}.json",
            creation.allocation_id()
        );
        self.registry.transaction_until(Some(deadline), |registry, waiters| {
            check_cancel(cancel)?;
            let owned = self.owned_observation(registry, creation, target_id, url)?;
            let mut proof = owned.clone();
            proof.as_object_mut().unwrap().remove("updatedAt");
            let authority = json!({"version":1,"kind":"open-observation-handover","creation":creation,"intent":intent,"lease":proof,"admissionRelease":"authorised"});
            let previous = self.registry.read(&filename, 64 * 1024, Value::Null)?;
            anyhow::ensure!(previous.is_null() || previous == authority, "Observation handover evidence changed; retaining admission");
            // Persist permission before releasing capacity; this receipt never asserts target absence.
            self.registry.write_until(&filename, &authority, Some(deadline))?;
            anyhow::ensure!(self.registry.read(&filename,64 * 1024,Value::Null)? == authority, "Observation handover readback changed");
            before_release()?;
            check_cancel(cancel)?;
            registry["leases"].as_array_mut().unwrap().retain(|row| row["id"] != owned["id"]);
            waiters["waiters"].as_array_mut().unwrap().retain(|row| !(row["id"] == owned["waiterId"] && same_admission(row, &owned)));
            Ok(authority)
        })
    }
    pub fn release(&self) -> Result<bool> {
        self.release_before(None)
    }
    pub(crate) async fn release_unissued(
        self: &Arc<Self>,
        deadline: std::time::Instant,
    ) -> Result<()> {
        let lease = self.clone();
        tokio::task::spawn_blocking(move || lease.release_unissued_before(deadline))
            .await
            .context("Unissued admission cleanup worker failed")?
    }
    pub(crate) fn release_unissued_before(&self, deadline: std::time::Instant) -> Result<()> {
        let mut refused = false;
        let result = self
            .registry
            .transaction_until(Some(deadline), |registry, waiters| {
                refused = true;
                let owned = self.owned(registry)?.clone();
                anyhow::ensure!(
                    owned["targetDispositionPending"] != true
                        && owned["chromeTargetId"].is_null()
                        && owned["targetDispositionAllocationId"].is_null()
                        && owned["observationIntent"].is_null(),
                    "Browser admission has target authority; retaining its exact lease"
                );
                refused = false;
                retire_unissued_admission(registry, waiters, &owned);
                Ok(())
            });
        if let Err(error) = &result
            && !refused
        {
            retain_failed_release(&self.registry.path, &self.owner, error);
        }
        result
    }
    pub(crate) fn release_before(&self, deadline: Option<std::time::Instant>) -> Result<bool> {
        let mut refused = false;
        let last = self
            .registry
            .transaction_until(deadline, |registry, waiters| {
                refused = true;
                let owned = self.owned(registry)?.clone();
                anyhow::ensure!(
                    owned["targetDispositionPending"] != true,
                    "Browser target allocation or retirement is unconfirmed; retaining its slot"
                );
                refused = false;
                let leases = registry["leases"].as_array_mut().unwrap();
                // Never prune other controllers during the reference-count decrement.
                leases.retain(|l| l["id"] != self.owner["id"]);
                waiters["waiters"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|w| !(w["id"] == owned["waiterId"] && same_admission(w, &owned)));
                self.release_decision(leases);
                Ok(leases.is_empty())
            })
            .inspect_err(|error| {
                if !refused {
                    retain_failed_release(&self.registry.path, &self.owner, error);
                }
            })?;
        eprintln!(
            "[browser] Released ChatGPT browser slot {}.",
            self.short_id()
        );
        Ok(last)
    }
    fn short_id(&self) -> String {
        self.id().chars().take(8).collect()
    }
    fn release_decision(&self, remaining: &[Value]) {
        eprintln!(
            "[browser] ChatGPT browser slot {} release decision: isLastLease={}; remaining={}; remainingSessions={}.",
            self.short_id(),
            remaining.is_empty(),
            remaining.len(),
            holding_sessions(remaining.iter())
        );
    }
    pub fn profile(&self) -> &Arc<ProfileRegistry> {
        &self.registry
    }
    pub(crate) fn confirm_target_absent(&self, target_id: &str) -> Result<()> {
        self.confirm_target_absent_before(target_id, None)
    }
    pub(crate) fn confirm_target_absent_before(
        &self,
        target_id: &str,
        deadline: Option<std::time::Instant>,
    ) -> Result<()> {
        self.registry
            .transaction_until(deadline, |registry, waiters| {
                let owned = self.owned(registry)?.clone();
                anyhow::ensure!(
                    owned["chromeTargetId"] == target_id,
                    "Target absence is for a different lease"
                );
                let leases = registry["leases"].as_array_mut().unwrap();
                leases.retain(|l| l["id"] != owned["id"]);
                waiters["waiters"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|w| !(w["id"] == owned["waiterId"] && same_admission(w, &owned)));
                self.release_decision(leases);
                Ok(())
            })?;
        eprintln!(
            "[browser] Released ChatGPT browser slot {}.",
            self.short_id()
        );
        Ok(())
    }
}
/// Exact admissions this process failed to release (reference `retainOwnedRetirement`). The next
/// admission on the same profile retries them, so a long-lived MCP host does not keep the slot.
static FAILED_RELEASES: std::sync::Mutex<Vec<(PathBuf, Value)>> = std::sync::Mutex::new(Vec::new());
fn failed_releases() -> std::sync::MutexGuard<'static, Vec<(PathBuf, Value)>> {
    FAILED_RELEASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn retain_failed_release(profile: &Path, record: &Value, error: &anyhow::Error) {
    let origin = json!({"id":record["id"],"waiterId":record["waiterId"],"processIdentity":record["processIdentity"],"sessionId":record["sessionId"],"createdAt":record["createdAt"]});
    let mut pending = failed_releases();
    if !pending
        .iter()
        .any(|(path, row)| path == profile && *row == origin)
    {
        eprintln!("[browser] Alcuin ownership cleanup remains pending: {error:#}");
        pending.push((profile.to_owned(), origin));
    }
}
/// Retires each retained admission that still has no target authority; returns those now gone.
fn retry_failed_releases(profile: &Path, registry: &mut Value, waiters: &mut Value) -> Vec<Value> {
    let pending: Vec<Value> = failed_releases()
        .iter()
        .filter(|(path, _)| path == profile)
        .map(|(_, origin)| origin.clone())
        .collect();
    let exact = |row: &Value, origin: &Value| {
        [
            "id",
            "waiterId",
            "processIdentity",
            "sessionId",
            "createdAt",
        ]
        .iter()
        .all(|key| row[key] == origin[key])
    };
    let leases = registry["leases"].as_array_mut().unwrap();
    for origin in &pending {
        leases.retain(|lease| {
            !(exact(lease, origin)
                && lease["targetDispositionPending"] != true
                && lease["targetDispositionAllocationId"].is_null())
        });
    }
    waiters["waiters"].as_array_mut().unwrap().retain(|waiter| {
        !pending.iter().any(|origin| {
            waiter["id"] == origin["waiterId"]
                && ["processIdentity", "sessionId", "createdAt"]
                    .iter()
                    .all(|key| waiter[key] == origin[key])
        })
    });
    pending
        .into_iter()
        .filter(|origin| !leases.iter().any(|lease| lease["id"] == origin["id"]))
        .collect()
}
fn forget_settled_releases(profile: &Path, settled: &[Value]) {
    failed_releases().retain(|(path, origin)| !(path == profile && settled.contains(origin)));
}
fn retire_unissued_admission(registry: &mut Value, waiters: &mut Value, origin: &Value) {
    registry["leases"].as_array_mut().unwrap().retain(|lease| {
        !(lease["id"] == origin["id"]
            && lease["waiterId"] == origin["waiterId"]
            && lease["processIdentity"] == origin["processIdentity"]
            && lease["sessionId"] == origin["sessionId"]
            && lease["createdAt"] == origin["createdAt"]
            && lease["targetDispositionPending"] != true
            && lease["chromeTargetId"].is_null()
            && lease["targetDispositionAllocationId"].is_null())
    });
    waiters["waiters"].as_array_mut().unwrap().retain(|waiter| {
        !(waiter["id"] == origin["waiterId"]
            && waiter["processIdentity"] == origin["processIdentity"]
            && waiter["sessionId"] == origin["sessionId"]
            && waiter["createdAt"] == origin["createdAt"])
    });
}
// There is deliberately no Drop decrement: unsettled target allocation must survive worker death.
fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    anyhow::ensure!(!cancel.is_cancelled(), "Browser operation was cancelled");
    Ok(())
}
fn text_id(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| !s.is_empty() && s.encode_utf16().count() <= 256)
}
fn valid_owner(v: &Value) -> bool {
    v.is_object()
        && text_id(&v["id"])
        && v["pid"]
            .as_u64()
            .is_some_and(|p| p > 0 && p <= u32::MAX as u64)
        && v["createdAt"].is_string()
        && v["updatedAt"].is_string()
        && v.get("processStartedAtMs")
            .is_none_or(|s| s.as_f64().is_some_and(f64::is_finite))
        && v.get("processIdentity").is_none_or(|s| {
            serde_json::from_value::<ProcessIdentity>(s.clone()).is_ok_and(|i| {
                Some(u64::from(i.pid)) == v["pid"].as_u64()
                    && Some(i.started_at_ms) == v["processStartedAtMs"].as_f64()
            })
        })
}
/// The shared Node writer drops `.0` from integral timestamps. Restore the Rust
/// representation only after both timestamp copies and the native identity validate,
/// so existing exact ownership comparisons remain stable without changing native ticks.
fn normalise_native_owner_timestamps(records: &mut Value) {
    for record in records.as_array_mut().expect("validated owner records") {
        if record.get("processIdentity").is_some() {
            let started = json!(
                record["processStartedAtMs"]
                    .as_f64()
                    .expect("validated native owner timestamp")
            );
            record["processStartedAtMs"] = started.clone();
            record["processIdentity"]["startedAtMs"] = started;
        }
    }
}
fn recoverable(v: &Value) -> bool {
    v["processStartedAtMs"].as_f64().is_some_and(f64::is_finite)
        && text_id(&v["sessionId"])
        && text_id(&v["chromeHost"])
        && v["chromePort"]
            .as_u64()
            .is_some_and(|p| p > 0 && p <= 65535)
        && text_id(&v["chromeTargetId"])
}
fn validate_registry(registry: &Value, queue: &Value) -> Result<()> {
    let leases = registry["leases"]
        .as_array()
        .context("Invalid browser lease registry")?;
    let waiters = queue["waiters"]
        .as_array()
        .context("Invalid browser admission queue")?;
    anyhow::ensure!(
        registry["version"] == 1 && queue["version"] == 1 && waiters.len() <= MAX_WAITERS,
        "Unknown browser registry or admission version"
    );
    let mut ids = BTreeSet::new();
    anyhow::ensure!(
        leases.iter().all(|l| valid_owner(l)
            && ids.insert(l["id"].as_str().unwrap())
            && [
                "capacityReleased",
                "targetDispositionPending",
                "promptSubmitted"
            ]
            .iter()
            .all(|k| l.get(k).is_none_or(Value::is_boolean))
            && ["waiterId", "targetDispositionAllocationId"]
                .iter()
                .all(|k| l.get(k).is_none_or(uuid_shaped))
            && (l["capacityReleased"] != true
                || l["targetDispositionPending"] == true && recoverable(l))),
        "Invalid browser lease record; preserving registry"
    );
    ids.clear();
    anyhow::ensure!(
        waiters.iter().all(|w| valid_owner(w)
            && Uuid::parse_str(w["id"].as_str().unwrap()).is_ok()
            && ids.insert(w["id"].as_str().unwrap())
            && w["maxConcurrentTabs"].as_u64().is_some_and(|n| n > 0)
            && date(&w["createdAt"]).is_some()
            && date(&w["updatedAt"]).is_some()),
        "Invalid admission queue; preserving ownership"
    );
    Ok(())
}
/// Reference `/^[0-9a-f-]{36}$/i`.
fn uuid_shaped(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}
fn date(v: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.timestamp_millis())
}
fn fresh(v: &Value, now: i64) -> bool {
    date(&v["updatedAt"]).is_none_or(|then| now.saturating_sub(then) <= STALE_MS)
}
fn same_owner(a: &Value, b: &Value) -> bool {
    [
        "id",
        "waiterId",
        "pid",
        "processStartedAtMs",
        "processIdentity",
        "sessionId",
        "createdAt",
    ]
    .iter()
    .all(|k| a[k] == b[k])
}
fn same_admission(a: &Value, b: &Value) -> bool {
    ["pid", "processStartedAtMs", "processIdentity", "sessionId"]
        .iter()
        .all(|k| a[k] == b[k])
}
fn record_liveness(v: &Value) -> Liveness {
    if let Some(identity) = v.get("processIdentity") {
        return serde_json::from_value::<ProcessIdentity>(identity.clone())
            .map_or(Liveness::Unknown, |i| i.liveness());
    }
    let Some(pid) = v["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 0)
    else {
        return Liveness::Unknown;
    };
    match ProcessIdentity::read(pid) {
        Ok(None) => Liveness::Dead,
        // Legacy (reference-written) start times come from another clock reader; the reference
        // accepts 10 s of skew (PROCESS_START_TIME_TOLERANCE_MS) before calling a PID reused.
        Ok(Some(actual)) => match v["processStartedAtMs"].as_f64() {
            Some(expected)
                if expected.is_finite()
                    && (expected - actual.started_at_ms).abs() <= LEGACY_START_TOLERANCE_MS =>
            {
                Liveness::Alive
            }
            Some(expected) if expected.is_finite() => Liveness::Dead,
            _ => Liveness::Unknown,
        },
        Err(_) => Liveness::Unknown,
    }
}
const LEGACY_START_TOLERANCE_MS: f64 = 10_000.0;
fn holding_sessions<'a>(leases: impl Iterator<Item = &'a Value>) -> String {
    let rows: Vec<String> = leases
        .map(|l| {
            format!(
                "{}:{}:pid{}:target{}",
                l["id"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>(),
                l["sessionId"].as_str().unwrap_or("unknown"),
                l["pid"],
                l["chromeTargetId"].as_str().unwrap_or("pending")
            )
        })
        .collect();
    if rows.is_empty() {
        "none".into()
    } else {
        rows.join(",")
    }
}
/// What one DevTools endpoint proves about its targets: gone, its exact target list, or nothing.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EndpointTargets {
    Gone,
    Unknown,
    Listed(BTreeSet<String>),
}
const REAP_BUDGET: Duration = Duration::from_secs(10);
const REAP_CONFIRM: Duration = Duration::from_millis(50);
pub(crate) type TargetLister = Arc<
    dyn Fn(String, u16, Duration) -> futures_util::future::BoxFuture<'static, EndpointTargets>
        + Send
        + Sync,
>;
/// Omission proves absence only in a complete, well-formed `/json/list`. A direct local
/// Chrome's listener lives as long as that Chrome, so a refused loopback connection means it
/// exited with its targets. Anything else is unknown.
pub(crate) async fn list_endpoint_targets(
    host: String,
    port: u16,
    budget: Duration,
) -> EndpointTargets {
    let lower = host.to_lowercase();
    if matches!(lower.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]") {
        // Probe refusal first: Windows retries a refused loopback connect for about 2 s.
        let bare = lower
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        match tokio::time::timeout(
            budget,
            tokio::net::TcpStream::connect((bare.as_str(), port)),
        )
        .await
        {
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                return EndpointTargets::Gone;
            }
            Ok(Ok(_)) => {}
            _ => return EndpointTargets::Unknown,
        }
    }
    let timeout = budget.min(Duration::from_millis(1_500));
    let base = format!("http://{}", crate::bridge::normalise_host_port(&host, port));
    let cancel = CancellationToken::new();
    let listed = tokio::time::timeout(
        timeout,
        crate::cdp::endpoint_json(&base, "/json/list", timeout, &cancel),
    )
    .await;
    match listed {
        Ok(Ok(Value::Array(rows))) => {
            if rows.iter().all(|row| {
                row["id"].as_str().is_some_and(|id| !id.is_empty()) && row["type"].is_string()
            }) {
                EndpointTargets::Listed(
                    rows.iter()
                        .map(|row| row["id"].as_str().unwrap().to_owned())
                        .collect(),
                )
            } else {
                EndpointTargets::Unknown
            }
        }
        _ => EndpointTargets::Unknown,
    }
}
/// A native identity proves exit exactly; a legacy record needs an absent PID or a start
/// outside the reference's 10 s tolerance. Unknown never counts as exit.
fn owner_exited(lease: &Value) -> bool {
    record_liveness(lease) == Liveness::Dead
}
impl ProfileRegistry {
    /// A capacity-released record whose controller exited keeps its slot's recovery target
    /// protected. Once that target provably no longer exists, the record protects nothing;
    /// remove it (reference `reapAbsentTargetLeases`, 26b52ee/ec03aea). Records of live or
    /// unidentifiable controllers and records whose target state is unproven are never touched.
    pub(crate) async fn reap_absent_target_leases(
        self: &Arc<Self>,
        refusal_proves_exit: bool,
        cancel: &CancellationToken,
    ) -> Result<usize> {
        let lister: TargetLister =
            Arc::new(|host, port, budget| Box::pin(list_endpoint_targets(host, port, budget)));
        self.reap_with(refusal_proves_exit, REAP_BUDGET, cancel, lister)
            .await
    }
    pub(crate) async fn reap_with(
        self: &Arc<Self>,
        refusal_proves_exit: bool,
        budget: Duration,
        cancel: &CancellationToken,
        lister: TargetLister,
    ) -> Result<usize> {
        let started = std::time::Instant::now();
        let registry = self.clone();
        let current = u64::from(std::process::id());
        let candidates = tokio::task::spawn_blocking(move || -> Result<Vec<Value>> {
            let mut records =
                registry.read(REGISTRY, 2 * 1024 * 1024, json!({"version":1,"leases":[]}))?;
            validate_registry(&records, &json!({"version":1,"waiters":[]}))?;
            normalise_native_owner_timestamps(&mut records["leases"]);
            Ok(records["leases"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| {
                    l["capacityReleased"] == true
                        && recoverable(l)
                        && l["pid"].as_u64() != Some(current)
                })
                .cloned()
                .collect())
        })
        .await
        .context("Browser slot record cleanup worker failed")??;
        if candidates.is_empty() {
            return Ok(0);
        }
        // Every endpoint is probed once, concurrently, within 40% of the budget, so a stalled
        // endpoint keeps only its own records.
        let endpoint_budget = budget.mul_f64(0.4);
        let keys: BTreeSet<(String, u16)> = candidates
            .iter()
            .map(|l| {
                (
                    l["chromeHost"].as_str().unwrap().to_owned(),
                    l["chromePort"].as_u64().unwrap() as u16,
                )
            })
            .collect();
        let probes = keys.iter().map(|(host, port)| {
            let probe = lister(host.clone(), *port, endpoint_budget);
            async move {
                tokio::time::timeout(endpoint_budget, probe)
                    .await
                    .unwrap_or(EndpointTargets::Unknown)
            }
        });
        let states = tokio::select! {biased;
            _ = cancel.cancelled() => bail!("Browser operation was cancelled"),
            states = futures_util::future::join_all(probes) => states,
        };
        let states: std::collections::BTreeMap<_, _> = keys.into_iter().zip(states).collect();
        let gone: Vec<Value> = candidates
            .into_iter()
            .filter(|l| {
                let key = (
                    l["chromeHost"].as_str().unwrap().to_owned(),
                    l["chromePort"].as_u64().unwrap() as u16,
                );
                match &states[&key] {
                    EndpointTargets::Unknown => false,
                    EndpointTargets::Gone => refusal_proves_exit,
                    EndpointTargets::Listed(ids) => {
                        !ids.contains(l["chromeTargetId"].as_str().unwrap())
                    }
                }
            })
            .collect();
        check_cancel(cancel)?;
        if gone.is_empty() {
            return Ok(0);
        }
        // Owners count as exited only if a second probe REAP_CONFIRM later agrees, and only
        // within 80% of the budget so the locked commit keeps time.
        let probe_deadline = started + budget.mul_f64(0.8);
        let exited = tokio::task::spawn_blocking(move || {
            let first: Vec<Value> = gone.into_iter().filter(owner_exited).collect();
            if first.is_empty() || std::time::Instant::now() + REAP_CONFIRM >= probe_deadline {
                return Vec::new();
            }
            std::thread::sleep(REAP_CONFIRM);
            first.into_iter().filter(owner_exited).collect::<Vec<_>>()
        })
        .await
        .context("Browser slot owner probe worker failed")?;
        check_cancel(cancel)?;
        if exited.is_empty() {
            return Ok(0);
        }
        let registry = self.clone();
        let deadline = started + budget;
        let removed = tokio::task::spawn_blocking(move || {
            registry.transaction_until(Some(deadline), |records, _| {
                // Remove only records still identical to the ones proven absent.
                let rows = records["leases"].as_array_mut().unwrap();
                let before = rows.len();
                rows.retain(|l| !exited.contains(l));
                Ok(before - rows.len())
            })
        })
        .await
        .context("Browser slot record cleanup worker failed")??;
        if removed > 0 {
            eprintln!(
                "[browser] Removed {removed} browser slot record(s) whose controllers exited and whose tabs no longer exist."
            );
        }
        Ok(removed)
    }
}
fn prune(registry: &mut Value, now: i64) -> Result<()> {
    let leases = registry["leases"]
        .as_array_mut()
        .context("Invalid lease registry")?;
    let mut retained = Vec::with_capacity(leases.len());
    for mut lease in std::mem::take(leases) {
        if lease["capacityReleased"] == true
            || fresh(&lease, now)
            || lease["targetDispositionPending"] == true && !recoverable(&lease)
            || record_liveness(&lease) != Liveness::Dead
        {
            retained.push(lease);
        } else if lease["targetDispositionPending"] == true {
            lease["capacityReleased"] = json!(true);
            retained.push(lease);
        }
    }
    *leases = retained;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn borrowed_attachment_refuses_idle_disposal_until_its_own_release() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let soon = || std::time::Instant::now() + Duration::from_secs(2);
        let attachments = || -> Value {
            serde_json::from_slice(&fs::read(home.path().join(ATTACHMENTS)).unwrap()).unwrap()
        };
        registry
            .empty_disposal_lock(soon())
            .unwrap()
            .release()
            .unwrap();
        // Pending attachment refuses; a run that fails before admission drops it.
        let marker = registry.mark_attachment(soon()).unwrap();
        assert!(registry.empty_disposal_lock(soon()).is_err());
        drop(marker);
        assert!(attachments()["attachments"].as_array().unwrap().is_empty());
        registry
            .empty_disposal_lock(soon())
            .unwrap()
            .release()
            .unwrap();
        // Two runs in one process: one's admission and release never clear the other's marker.
        let first = registry.mark_attachment(soon()).unwrap();
        let second = registry.mark_attachment(soon()).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-attach-admission",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        lease.release().unwrap();
        drop(first);
        assert_eq!(attachments()["attachments"].as_array().unwrap().len(), 1);
        assert!(registry.empty_disposal_lock(soon()).is_err());
        drop(second);
        registry
            .empty_disposal_lock(soon())
            .unwrap()
            .release()
            .unwrap();
        // A clock stepped back after marking keeps the marker active.
        let ahead = registry.mark_attachment(soon()).unwrap();
        let mut moved = attachments();
        moved["attachments"][0]["createdAtMs"] =
            json!(chrono::Utc::now().timestamp_millis() + 60_000);
        atomic_json(&home.path().join(ATTACHMENTS), &moved).unwrap();
        assert!(registry.empty_disposal_lock(soon()).is_err());
        drop(ahead);
        // Dead-process and expired markers never retain the browser.
        let current = ProcessIdentity::current().unwrap();
        let mut dead = current.clone();
        dead.native_start += 1;
        assert_eq!(dead.liveness(), Liveness::Dead);
        let now = chrono::Utc::now().timestamp_millis();
        atomic_json(
            &home.path().join(ATTACHMENTS),
            &json!({"version":1,"attachments":[
                {"id":"PUBLIC-dead","processIdentity":dead,"createdAtMs":now},
                {"id":"PUBLIC-expired","processIdentity":current,"createdAtMs":now - ATTACHMENT_MS}]}),
        )
        .unwrap();
        registry
            .empty_disposal_lock(soon())
            .unwrap()
            .release()
            .unwrap();
        assert!(attachments()["attachments"].as_array().unwrap().is_empty());
    }
    #[test]
    fn idle_owner_hint_round_trips_and_rejects_invalid_ids() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        assert_eq!(registry.idle_owner(), None);
        registry.set_idle_owner("PUBLIC-idle-owner").unwrap();
        assert_eq!(registry.idle_owner().as_deref(), Some("PUBLIC-idle-owner"));
        assert!(registry.set_idle_owner("../escape").is_err());
        atomic_json(
            &home.path().join(IDLE_OWNER),
            &json!({"version":1,"sessionId":"../escape"}),
        )
        .unwrap();
        assert_eq!(registry.idle_owner(), None);
    }
    #[tokio::test]
    async fn native_lease_survives_a_legacy_integer_timestamp_rewrite() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-legacy-roundtrip",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let path = home.path().join(REGISTRY);
        let mut wire: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let owner = &mut wire["leases"][0];
        let original = owner.clone();
        let started = owner["processStartedAtMs"].as_f64().unwrap();
        assert_eq!(started.fract(), 0.0);
        // JSON.stringify emits an integral f64 as an integer; native ticks stay strings.
        owner["processStartedAtMs"] = json!(started as u64);
        owner["processIdentity"]["startedAtMs"] = json!(started as u64);
        owner["PUBLIC-extension"] = json!({"retained":true});
        assert_ne!(owner["processStartedAtMs"], original["processStartedAtMs"]);
        assert_eq!(
            owner["processIdentity"]["nativeStart"],
            original["processIdentity"]["nativeStart"]
        );
        atomic_json(&path, &wire).unwrap();
        lease.update(&json!({"PUBLIC-heartbeat":true})).unwrap();
        let current = registry.records().unwrap();
        assert_eq!(current["leases"][0]["PUBLIC-extension"]["retained"], true);
        assert_eq!(current["leases"][0]["PUBLIC-heartbeat"], true);
        assert_eq!(
            current["leases"][0]["processIdentity"],
            original["processIdentity"]
        );
        lease.release().unwrap();
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn native_timestamp_normalisation_never_repairs_changed_ownership() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-owner-negative",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let path = home.path().join(REGISTRY);
        let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for fault in ["outer", "inner", "both", "ticks", "malformed"] {
            let mut wire = original.clone();
            let row = &mut wire["leases"][0];
            let started = row["processStartedAtMs"].as_f64().unwrap() as u64;
            row["processStartedAtMs"] = json!(started);
            row["processIdentity"]["startedAtMs"] = json!(started);
            match fault {
                "outer" => row["processStartedAtMs"] = json!(started + 1),
                "inner" => row["processIdentity"]["startedAtMs"] = json!(started + 1),
                "both" => {
                    row["processStartedAtMs"] = json!(started + 1);
                    row["processIdentity"]["startedAtMs"] = json!(started + 1);
                }
                "ticks" => row["processIdentity"]["nativeStart"] = json!("1"),
                _ => row["processIdentity"] = json!(false),
            }
            atomic_json(&path, &wire).unwrap();
            let before = fs::read(&path).unwrap();
            assert!(
                lease.update(&json!({"PUBLIC-forbidden":true})).is_err(),
                "{fault}"
            );
            assert_eq!(fs::read(&path).unwrap(), before, "{fault}");
        }
        atomic_json(&path, &original).unwrap();
        lease.release().unwrap();
    }

    #[tokio::test]
    async fn queued_native_admission_keeps_its_token_after_legacy_timestamp_rewrite() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let first = registry
            .acquire_with_limit(
                "PUBLIC-occupied",
                1,
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let waiting = registry.clone();
        let task = tokio::spawn(async move {
            waiting
                .acquire_with_limit(
                    "PUBLIC-queued",
                    1,
                    Duration::from_secs(3),
                    &CancellationToken::new(),
                )
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let waiter = loop {
            let rows = registry
                .transaction(|_, waiters| Ok(waiters.clone()))
                .unwrap();
            if let Some(row) = rows["waiters"].as_array().unwrap().first() {
                break row.clone();
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let lock = DirectoryLock::acquire(
            &home.path().join("oracle-tab-leases.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        for (file, key) in [(REGISTRY, "leases"), (WAITERS, "waiters")] {
            let path = home.path().join(file);
            let mut rows: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            for row in rows[key].as_array_mut().unwrap() {
                let started = row["processStartedAtMs"].as_f64().unwrap() as u64;
                row["processStartedAtMs"] = json!(started);
                row["processIdentity"]["startedAtMs"] = json!(started);
            }
            atomic_json(&path, &rows).unwrap();
        }
        lock.release().unwrap();
        first.release().unwrap();
        let promoted = task.await.unwrap().unwrap();
        assert_eq!(promoted.owner["waiterId"], waiter["id"]);
        assert!(
            registry
                .transaction(|_, queue| Ok(queue["waiters"].as_array().unwrap().is_empty()))
                .unwrap()
        );
        promoted.release().unwrap();
    }

    #[tokio::test]
    async fn disposal_marker_blocks_an_already_connected_waiter_after_lease_lock_release() {
        let home = tempfile::tempdir().unwrap();
        let mut fresh = crate::browser_profile_copy::TemporaryProfile::create(home.path()).unwrap();
        let registry = ProfileRegistry::open(fresh.path()).unwrap();
        let lock = registry
            .empty_disposal_lock(std::time::Instant::now() + Duration::from_secs(1))
            .unwrap();
        let waiting = registry.clone();
        let task = tokio::spawn(async move {
            waiting
                .acquire(
                    "PUBLIC-already-connected",
                    Duration::from_secs(2),
                    &CancellationToken::new(),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        let marker = fresh_disposal_marker(fresh.path()).unwrap();
        atomic_json(
            &marker,
            &json!({"version":1,"kind":"PUBLIC disposal fixture"}),
        )
        .unwrap();
        lock.release().unwrap();
        assert!(task.await.unwrap().is_err());
        let records = registry.records().unwrap();
        assert!(records["leases"].as_array().unwrap().is_empty());
        assert!(
            !fresh.path().join(WAITERS).exists(),
            "A disposal refusal must not register a waiter"
        );
        crate::native_fs::remove_owned_file(&marker, &NativeEntry::open(&marker, false).unwrap())
            .unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-available",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        lease.release().unwrap();
        drop(lease);
        drop(registry);
        fresh.cleanup().unwrap();
    }
    /// Reads the persisted admission queue without its lock. A test that polls through
    /// `transaction` competes for the same lock as the cleanup it is waiting for, and on a
    /// loaded machine can starve that cleanup's fixed one-second budget.
    fn unlocked_waiters(path: &Path) -> Option<Vec<Value>> {
        // A read can race an atomic replacement; the caller polls again.
        let bytes = fs::read(path.join(WAITERS)).ok()?;
        let queue: Value = serde_json::from_slice(&bytes).ok()?;
        queue["waiters"].as_array().cloned()
    }
    #[tokio::test]
    async fn abandoned_admission_retires_registered_waiter_without_cancelling_parent() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let foreign = registry
            .acquire_with_limit(
                "PUBLIC-foreign",
                1,
                Duration::from_secs(10),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let before = registry.records().unwrap();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let acquiring = registry.clone();
        let task = tokio::spawn(async move {
            acquiring
                .acquire_with_limit("PUBLIC-abandoned", 1, Duration::from_secs(60), &token)
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if unlocked_waiters(home.path()).is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["sessionId"] == "PUBLIC-abandoned")
            }) {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(!cancel.is_cancelled());
        loop {
            if unlocked_waiters(home.path()).is_some_and(|rows| rows.is_empty()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Abandoned waiter remained registered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(registry.records().unwrap(), before);
        foreign.release().unwrap();
    }
    #[tokio::test]
    async fn abandoned_promoted_delivery_releases_only_its_unused_lease() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-abandoned",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let foreign = registry
            .acquire(
                "PUBLIC-foreign",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let foreign_before = registry.records().unwrap()["leases"][1].clone();
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || AdmissionDelivery {
            lease: Some(lease),
            runtime,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !task.is_finished() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        drop(task);
        loop {
            let records = registry.records().unwrap();
            if records["leases"] == json!([foreign_before]) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Abandoned promoted lease remained registered"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        foreign.release().unwrap();
    }
    #[tokio::test]
    async fn registered_waiter_cancellation_cleanup_keeps_runtime_timers_running() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for (hold_ms, pending) in [(200, false), (1500, true)] {
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let foreign = registry
                .acquire_with_limit(
                    "PUBLIC-foreign",
                    1,
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let before = registry.records().unwrap();
            let cancel = CancellationToken::new();
            let progress = Arc::new(AtomicBool::new(false));
            let token = cancel.clone();
            let tick = progress.clone();
            let timer = tokio::spawn(async move {
                token.cancelled().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                tick.store(true, Ordering::SeqCst);
            });
            let observed_registry = registry.clone();
            let token = cancel.clone();
            let hold = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                loop {
                    let waiters = observed_registry
                        .read(WAITERS, 64 * 1024, json!({"version":1,"waiters":[]}))
                        .unwrap();
                    if waiters["waiters"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["sessionId"] == "PUBLIC-queued")
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "Own waiter was not registered"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                let held = DirectoryLock::acquire(
                    &observed_registry.path().join("oracle-tab-leases.lock"),
                    Duration::from_secs(1),
                )
                .unwrap();
                token.cancel();
                std::thread::sleep(Duration::from_millis(hold_ms));
                let progressed = progress.load(Ordering::SeqCst);
                held.release().unwrap();
                progressed
            });
            let result = registry
                .acquire_with_limit("PUBLIC-queued", 1, Duration::from_secs(4), &cancel)
                .await;
            let progressed = hold.join().unwrap();
            timer.await.unwrap();
            let error = result.err().unwrap();
            assert!(error.to_string().contains("cancelled"));
            assert_eq!(error.is::<UnsettledAdmission>(), pending);
            assert!(
                progressed,
                "Registered waiter cleanup blocked the runtime timer"
            );
            assert_eq!(registry.records().unwrap(), before);
            let waiters = registry
                .read(WAITERS, 64 * 1024, json!({"version":1,"waiters":[]}))
                .unwrap();
            let waiters = waiters["waiters"].as_array().unwrap();
            assert_eq!(waiters.len(), usize::from(pending));
            if pending {
                assert_eq!(waiters[0]["sessionId"], "PUBLIC-queued");
            }
            foreign.release().unwrap();
        }
    }
    #[tokio::test]
    async fn unissued_release_requires_unchanged_admission_without_target_authority() {
        for case in [
            "unissued",
            "pending",
            "target",
            "allocation",
            "replaced",
            "expired",
        ] {
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let lease = registry
                .acquire(
                    "PUBLIC-unissued",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let foreign = registry
                .acquire(
                    "PUBLIC-foreign",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            match case {
                "pending" => lease
                    .update(&json!({"targetDispositionPending":true}))
                    .unwrap(),
                "target" => lease
                    .update(&json!({"chromeTargetId":"PUBLIC-TARGET"}))
                    .unwrap(),
                "allocation" => lease
                    .update(&json!({"targetDispositionAllocationId":Uuid::new_v4().to_string()}))
                    .unwrap(),
                "replaced" => {
                    let mut records = registry.records().unwrap();
                    records["leases"][0]["pid"] = json!(u32::MAX);
                    records["leases"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("processIdentity");
                    atomic_json(&home.path().join(REGISTRY), &records).unwrap();
                }
                _ => {}
            }
            let before = registry.records().unwrap();
            let deadline = std::time::Instant::now()
                + if case == "expired" {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                };
            let result = lease.release_unissued_before(deadline);
            assert_eq!(result.is_ok(), case == "unissued", "{case}: {result:?}");
            let after = registry.records().unwrap();
            if case == "unissued" {
                assert_eq!(after["leases"], json!([before["leases"][1]]));
            } else {
                assert_eq!(after, before);
            }
            foreign.release().unwrap();
        }
    }
    #[tokio::test]
    async fn admission_waits_past_one_lock_slice_within_its_original_deadline() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let held = DirectoryLock::acquire(
            &home.path().join("oracle-tab-leases.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            held.release().unwrap();
        });
        let started = std::time::Instant::now();
        let result = registry
            .acquire(
                "PUBLIC-contended",
                Duration::from_secs(4),
                &CancellationToken::new(),
            )
            .await;
        release.join().unwrap();
        let lease =
            result.expect("Admission must use its remaining budget after one second of contention");
        assert!(started.elapsed() >= Duration::from_secs(1));
        lease.release().unwrap();
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn contended_admission_allows_runtime_cancellation_before_its_deadline() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let held = DirectoryLock::acquire(
            &home.path().join("oracle-tab-leases.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let (released, wait) = std::sync::mpsc::channel();
        let release = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(5));
            held.release().unwrap();
        });
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            token.cancel();
        });
        let started = std::time::Instant::now();
        let result = registry
            .acquire("PUBLIC-cancelled", Duration::from_secs(4), &cancel)
            .await;
        let elapsed = started.elapsed();
        released.send(()).unwrap();
        release.join().unwrap();
        timer.await.unwrap();
        assert!(result.err().unwrap().to_string().contains("cancelled"));
        assert!(
            elapsed < Duration::from_millis(600),
            "Runtime cancellation was starved for {elapsed:?}"
        );
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn observation_registry_writes_keep_the_callers_deadline_after_admission() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-deadline",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let before = registry.records().unwrap();
        let lock = DirectoryLock::acquire(
            &home.path().join("oracle-tab-leases.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let start = std::time::Instant::now();
        assert!(
            lease
                .update_until(
                    &json!({"chromeHost":"127.0.0.1"}),
                    start + Duration::from_millis(40)
                )
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(300));
        let start = std::time::Instant::now();
        assert!(lease.begin_allocation(Some(&json!({"version":1,"id":Uuid::new_v4().to_string(),"url":"https://chatgpt.com/c/public-deadline"})),start+Duration::from_millis(40)).is_err());
        assert!(start.elapsed() < Duration::from_millis(300));
        lock.release().unwrap();
        assert_eq!(registry.records().unwrap(), before);
        lease.release().unwrap();
    }
    #[tokio::test]
    async fn interrupted_observation_handover_persists_permission_and_retains_exact_admission() {
        use crate::browser_disposition::TargetCreation;
        const URL: &str = "https://chatgpt.com/c/public-handover";
        for cancel_after_receipt in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let cancel = CancellationToken::new();
            let lease = registry
                .acquire("PUBLIC-observer", Duration::from_secs(1), &cancel)
                .await
                .unwrap();
            let foreign = registry
                .acquire("PUBLIC-foreign", Duration::from_secs(1), &cancel)
                .await
                .unwrap();
            let allocation = Uuid::new_v4().to_string();
            let creation = TargetCreation::observation("PUBLIC-TARGET", &allocation, URL);
            lease
                .begin_allocation(
                    Some(&json!({"version":1,"id":allocation,"url":URL})),
                    std::time::Instant::now() + Duration::from_secs(1),
                )
                .unwrap();
            lease.update(&json!({"chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-TARGET"})).unwrap();
            let before = registry.records().unwrap();
            let path = home
                .path()
                .join(format!("oracle-observation-handover-{allocation}.json"));
            let interrupted = lease.handover_observation_with(
                &creation,
                "PUBLIC-TARGET",
                URL,
                std::time::Instant::now() + Duration::from_secs(1),
                &cancel,
                || {
                    let permission: Value = serde_json::from_slice(&fs::read(&path)?).unwrap();
                    assert_eq!(permission["admissionRelease"], "authorised");
                    if cancel_after_receipt {
                        cancel.cancel();
                        Ok(())
                    } else {
                        bail!("PUBLIC simulated interruption after receipt")
                    }
                },
            );
            assert!(interrupted.is_err());
            assert_eq!(registry.records().unwrap(), before);
            let permission = fs::read(&path).unwrap();
            lease.update(&json!({})).unwrap();
            lease
                .handover_observation(
                    &creation,
                    "PUBLIC-TARGET",
                    URL,
                    std::time::Instant::now() + Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), permission);
            let after = registry.records().unwrap();
            assert_eq!(after["leases"], json!([before["leases"][1]]));
            assert!(
                lease
                    .handover_observation(
                        &creation,
                        "PUBLIC-TARGET",
                        URL,
                        std::time::Instant::now() + Duration::from_secs(1),
                        &CancellationToken::new()
                    )
                    .is_err()
            );
            assert_eq!(registry.records().unwrap(), after);
            assert_eq!(fs::read(&path).unwrap(), permission);
            foreign.release().unwrap();
        }
    }
    #[tokio::test]
    async fn disposed_release_requires_the_durable_receipt_and_preserves_replacements() {
        use crate::{
            browser_disposition::{TargetCreation, TargetJournal},
            sessions::SessionStore,
        };
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(&home.path().join("PUBLIC-profile")).unwrap();
        let lease = registry
            .acquire(
                "PUBLIC-session",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let journal = TargetJournal::open(&store).unwrap();
        let creation = TargetCreation::allocated("PUBLIC-target");
        lease.update(&json!({"chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-target","targetDispositionPending":true,"targetDispositionAllocationId":creation.allocation_id()})).unwrap();
        let creator = journal.record_creation(&creation,
            &json!({"host":"127.0.0.1","port":9222,"targetId":"PUBLIC-target","claimId":Uuid::new_v4().to_string()}),
            &ProcessIdentity::current().unwrap(), Some("PUBLIC-session"),
            Some(json!({"id":lease.id(),"profileDir":registry.path()}))).unwrap();
        let disposed = journal
            .record_disposition(&creator, "confirmed-absent", "PUBLIC independently absent")
            .unwrap();
        let deadline = || std::time::Instant::now() + Duration::from_secs(2);
        assert!(
            registry
                .release_disposed(&journal, &creator, &disposed, deadline(), false)
                .is_err()
        );
        let original = registry.records().unwrap();
        let mut fabricated = disposed.clone();
        fabricated["reason"] = json!("PUBLIC fabricated absence");
        assert!(
            registry
                .release_disposed(&journal, &creator, &fabricated, deadline(), true)
                .is_err()
        );
        for (field, replacement) in [
            ("chromeTargetId", json!("PUBLIC-other-target")),
            ("chromePort", json!(9223)),
            ("chromeHost", json!("PUBLIC-other-host")),
            (
                "targetDispositionAllocationId",
                json!(Uuid::new_v4().to_string()),
            ),
            ("sessionId", json!("PUBLIC-other-session")),
            ("processStartedAtMs", json!(1.0)),
        ] {
            let mut changed = original.clone();
            changed["leases"][0][field] = replacement;
            if field == "processStartedAtMs" {
                changed["leases"][0]["processIdentity"]["startedAtMs"] = json!(1.0);
            }
            atomic_json(&registry.path().join(REGISTRY), &changed).unwrap();
            assert!(
                registry
                    .release_disposed(&journal, &creator, &disposed, deadline(), true)
                    .is_err(),
                "{field}"
            );
            assert_eq!(registry.records().unwrap(), changed);
        }
        atomic_json(&registry.path().join(REGISTRY), &original).unwrap();
        let mut owned_waiter = original["leases"][0].clone();
        owned_waiter["id"] = owned_waiter["waiterId"].clone();
        owned_waiter["maxConcurrentTabs"] = json!(MAX_TABS);
        let mut unrelated = owned_waiter.clone();
        unrelated["id"] = json!(Uuid::new_v4().to_string());
        unrelated["sessionId"] = json!("PUBLIC-unrelated-session");
        atomic_json(
            &registry.path().join(WAITERS),
            &json!({"version":1,"waiters":[owned_waiter,unrelated]}),
        )
        .unwrap();
        assert!(
            registry
                .release_disposed(&journal, &creator, &disposed, deadline(), true)
                .unwrap()
        );
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            registry.read(WAITERS, 65536, Value::Null).unwrap()["waiters"],
            json!([unrelated])
        );
        assert!(
            !registry
                .release_disposed(&journal, &creator, &disposed, deadline(), true)
                .unwrap()
        );
    }
    #[test]
    fn a_live_legacy_worker_retains_capacity_at_the_reference_timestamp_resolution() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let identity = ProcessIdentity::current().unwrap();
        #[cfg(windows)]
        let legacy_start = ((identity.native_start - 116444736000000000) / 10000) as f64;
        #[cfg(not(windows))]
        let legacy_start = (identity.started_at_ms / 1000.0).floor() * 1000.0;
        let lease = json!({"id":"legacy-live","pid":identity.pid,"processStartedAtMs":legacy_start,"sessionId":"legacy-live","chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-TARGET","targetDispositionPending":true,"promptSubmitted":true,"createdAt":"2020-01-01T00:00:00Z","updatedAt":"2020-01-01T00:00:00Z","unknownHistoricalField":"PUBLIC"});
        fs::write(
            home.path().join(REGISTRY),
            serde_json::to_vec(&json!({"version":1,"leases":[lease.clone()]})).unwrap(),
        )
        .unwrap();
        let records = registry.reclaim_dead_capacity().unwrap();
        assert_eq!(records["leases"], json!([lease]));
    }
    #[tokio::test]
    async fn failed_promotion_retires_only_its_unissued_lease_after_a_partial_registry_write() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let owned = registry
            .acquire(
                "PUBLIC-promotion",
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let origin = owned.record().clone();
        let mut pending = origin.clone();
        pending["id"] = json!("PUBLIC-pending");
        pending["targetDispositionPending"] = json!(true);
        pending["chromeTargetId"] = json!("PUBLIC-target");
        let mut replacement = origin.clone();
        replacement["id"] = json!("PUBLIC-replacement");
        replacement["sessionId"] = json!("PUBLIC-other");
        let mut rows = json!({"leases":[origin,pending,replacement]});
        let mut waiters = json!({"waiters":[{"id":origin["waiterId"],"processIdentity":origin["processIdentity"],"sessionId":origin["sessionId"],"createdAt":origin["createdAt"]}]});
        retire_unissued_admission(&mut rows, &mut waiters, &origin);
        assert_eq!(rows["leases"], json!([pending, replacement]));
        assert!(waiters["waiters"].as_array().unwrap().is_empty());
        let mut changed = origin.clone();
        changed["processIdentity"]["nativeStart"] = json!("1");
        let mut rows = json!({"leases":[changed]});
        retire_unissued_admission(&mut rows, &mut waiters, &origin);
        assert_eq!(rows["leases"], json!([changed]));
        owned.release().unwrap();
    }
    #[tokio::test]
    async fn configured_capacity_controls_admission_and_retires_timed_out_waiters() {
        for limit in [1, 4] {
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let cancel = CancellationToken::new();
            let mut leases = Vec::new();
            for index in 0..limit {
                leases.push(
                    registry
                        .acquire_with_limit(
                            &format!("public-{index}"),
                            limit,
                            Duration::from_secs(1),
                            &cancel,
                        )
                        .await
                        .unwrap(),
                );
            }
            assert!(
                registry
                    .acquire_with_limit("blocked", limit, Duration::from_millis(50), &cancel)
                    .await
                    .is_err()
            );
            assert!(
                registry
                    .read(WAITERS, 64 * 1024, json!({"version":1,"waiters":[]}))
                    .unwrap()["waiters"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            leases.pop().unwrap().release().unwrap();
            let promoted = registry
                .acquire_with_limit("promoted", limit, Duration::from_secs(1), &cancel)
                .await
                .unwrap();
            promoted.release().unwrap();
            for lease in leases {
                lease.release().unwrap();
            }
        }
    }
    #[tokio::test]
    async fn three_tabs_fifo_admission_and_cancelled_waiter_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let cancel = CancellationToken::new();
        let one = registry
            .acquire("one", Duration::from_secs(2), &cancel)
            .await
            .unwrap();
        let two = registry
            .acquire("two", Duration::from_secs(2), &cancel)
            .await
            .unwrap();
        let three = registry
            .acquire("three", Duration::from_secs(2), &cancel)
            .await
            .unwrap();
        let fourth_registry = registry.clone();
        let fourth_cancel = CancellationToken::new();
        let token = fourth_cancel.clone();
        let fourth = tokio::spawn(async move {
            fourth_registry
                .acquire("four", Duration::from_secs(2), &token)
                .await
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let queue = registry.read(WAITERS, 64 * 1024, Value::Null).unwrap();
            if queue["waiters"].as_array().is_some_and(|w| !w.is_empty()) {
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let fifth_registry = registry.clone();
        let token = cancel.clone();
        let fifth = tokio::spawn(async move {
            fifth_registry
                .acquire("five", Duration::from_secs(2), &token)
                .await
        });
        one.release().unwrap();
        let fourth = fourth.await.unwrap().unwrap();
        assert_eq!(fourth.record()["sessionId"], "four");
        assert!(!fifth.is_finished());
        fourth.release().unwrap();
        let fifth = fifth.await.unwrap().unwrap();
        assert_eq!(fifth.record()["sessionId"], "five");
        let waiting_registry = registry.clone();
        let waiting_cancel = CancellationToken::new();
        let token = waiting_cancel.clone();
        let waiting = tokio::spawn(async move {
            waiting_registry
                .acquire("cancelled", Duration::from_secs(2), &token)
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        waiting_cancel.cancel();
        assert!(waiting.await.unwrap().is_err());
        assert!(
            registry.read(WAITERS, 64 * 1024, Value::Null).unwrap()["waiters"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        two.release().unwrap();
        three.release().unwrap();
        fifth.release().unwrap();
    }
    #[test]
    fn dead_worker_capacity_releases_with_recovery_and_unknown_fields_retained() {
        let mut dead = ProcessIdentity::current().unwrap();
        dead.native_start += 1;
        let old = "2000-01-01T00:00:00Z";
        let lease = json!({"id":"retained","pid":dead.pid,"processStartedAtMs":dead.started_at_ms,"processIdentity":dead,"sessionId":"public-session","chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-TARGET","targetDispositionPending":true,"promptSubmitted":false,"createdAt":old,"updatedAt":old,"unknownHistorical":{"mustRemain":true},"uncertainSubmission":{"id":"PUBLIC-RESERVATION"}});
        let mut registry = json!({"version":1,"leases":[lease],"unknownTopLevel":17});
        validate_registry(&registry, &json!({"version":1,"waiters":[]})).unwrap();
        prune(&mut registry, chrono::Utc::now().timestamp_millis()).unwrap();
        assert_eq!(registry["leases"][0]["capacityReleased"], true);
        assert_eq!(
            registry["leases"][0]["uncertainSubmission"]["id"],
            "PUBLIC-RESERVATION"
        );
        assert_eq!(
            registry["leases"][0]["unknownHistorical"]["mustRemain"],
            true
        );
        assert_eq!(registry["unknownTopLevel"], 17);
        let retained = registry.clone();
        prune(&mut registry, chrono::Utc::now().timestamp_millis()).unwrap();
        assert_eq!(registry, retained);
    }
    #[test]
    fn fresh_legacy_and_unknown_allocations_stay_capacity_blockers() {
        let mut dead = ProcessIdentity::current().unwrap();
        dead.native_start += 1;
        for (fresh_record, recoverable_record) in [(true, true), (false, false)] {
            let stamp = if fresh_record {
                timestamp()
            } else {
                "2000-01-01T00:00:00Z".into()
            };
            let mut lease = json!({"id":"foreign","pid":dead.pid,"processStartedAtMs":dead.started_at_ms,"processIdentity":dead,"targetDispositionPending":true,"createdAt":stamp,"updatedAt":stamp});
            if recoverable_record {
                lease["sessionId"] = json!("public");
                lease["chromeHost"] = json!("127.0.0.1");
                lease["chromePort"] = json!(9222);
                lease["chromeTargetId"] = json!("PUBLIC");
            }
            let mut registry = json!({"version":1,"leases":[lease]});
            let before = registry.clone();
            prune(&mut registry, chrono::Utc::now().timestamp_millis()).unwrap();
            assert_eq!(registry, before);
        }
    }
    #[tokio::test]
    async fn changed_lease_owner_and_unconfirmed_target_refuse_release() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = registry
            .acquire("public", Duration::from_secs(1), &CancellationToken::new())
            .await
            .unwrap();
        lease
            .update(&json!({"targetDispositionPending":true}))
            .unwrap();
        assert!(lease.release().is_err());
        registry
            .transaction(|r, _| {
                r["leases"][0]["createdAt"] = json!("replacement");
                Ok(())
            })
            .unwrap();
        assert!(
            lease
                .update(&json!({"targetDispositionPending":false}))
                .is_err()
        );
        assert!(lease.release().is_err());
        assert_eq!(
            registry.records().unwrap()["leases"][0]["createdAt"],
            "replacement"
        );
    }
    #[tokio::test]
    async fn malformed_registry_is_preserved_byte_for_byte() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        for raw in [
            "{broken",
            "{\"version\":2,\"leases\":[]}",
            "{\"version\":1,\"leases\":[{\"id\":\"foreign\",\"pid\":0}]}",
            "{\"version\":1,\"leases\":[{\"id\":\"foreign\",\"pid\":1,\"createdAt\":\"x\",\"updatedAt\":\"y\",\"targetDispositionAllocationId\":\"not-a-uuid\"}]}",
            "{\"version\":1,\"leases\":[{\"id\":\"foreign\",\"pid\":1,\"createdAt\":\"x\",\"updatedAt\":\"y\",\"waiterId\":\"waiter\"}]}",
        ] {
            fs::write(home.path().join(REGISTRY), raw).unwrap();
            assert!(
                registry
                    .acquire("public", Duration::from_secs(1), &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(fs::read_to_string(home.path().join(REGISTRY)).unwrap(), raw);
        }
    }
    #[test]
    #[ignore = "Executed only by the synthetic worker-death regression"]
    fn synthetic_native_worker() {
        let home = std::env::var_os("HOME").unwrap();
        assert_eq!(Some(home.clone()), std::env::var_os("USERPROFILE"));
        assert!(crate::storage::same_directory(
            &std::env::current_dir().unwrap(),
            &PathBuf::from(home)
        ));
        let marker = std::env::var_os("ALCUIN_SYNTHETIC_WORKER_MARKER").unwrap();
        atomic_json(Path::new(&marker), &ProcessIdentity::current().unwrap()).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    #[test]
    fn actual_child_death_reclaims_capacity_and_preserves_uncertain_submission_recovery() {
        use std::process::{Command, Stdio};
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("public-worker.json");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "browser_profile::tests::synthetic_native_worker",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ALCUIN_SYNTHETIC_WORKER_MARKER", &marker)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let identity: ProcessIdentity = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
        assert_eq!(identity.liveness(), Liveness::Alive);
        let profile = ProfileRegistry::open(&home.path().join("public-profile")).unwrap();
        let record = json!({"id":"public-dead-worker","pid":identity.pid,"processStartedAtMs":identity.started_at_ms,"processIdentity":identity,"sessionId":"public-session","chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-TARGET","targetDispositionPending":true,"createdAt":"2000-01-01T00:00:00Z","updatedAt":"2000-01-01T00:00:00Z","uncertainSubmission":{"state":"ambiguous","claimId":"public-claim"}});
        fs::write(
            profile.path().join(REGISTRY),
            serde_json::to_vec(&json!({"version":1,"leases":[record]})).unwrap(),
        )
        .unwrap();
        assert_ne!(
            profile.reclaim_dead_capacity().unwrap()["leases"][0]["capacityReleased"],
            true
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(identity.liveness(), Liveness::Dead);
        let retained = profile.reclaim_dead_capacity().unwrap();
        assert_eq!(retained["leases"][0]["capacityReleased"], true);
        assert_eq!(
            retained["leases"][0]["uncertainSubmission"]["state"],
            "ambiguous"
        );
        assert_eq!(retained["leases"][0]["chromeTargetId"], "PUBLIC-TARGET");
    }
    fn exited_identity() -> ProcessIdentity {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(ProcessIdentity::read(pid).unwrap().is_none());
        ProcessIdentity {
            pid,
            started_at_ms: 1_000_000_000_000.0,
            native_start: 1_234_567,
        }
    }
    fn live_foreign() -> (std::process::Child, ProcessIdentity) {
        let child = if cfg!(windows) {
            std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("30").spawn()
        }
        .unwrap();
        let identity = ProcessIdentity::read(child.id()).unwrap().unwrap();
        (child, identity)
    }
    fn released(id: &str, owner: &ProcessIdentity, port: u16, target: &str) -> Value {
        json!({"id":id,"waiterId":Uuid::new_v4().to_string(),"pid":owner.pid,"processStartedAtMs":owner.started_at_ms,"processIdentity":owner,"sessionId":"public-reaped","chromeHost":"127.0.0.1","chromePort":port,"chromeTargetId":target,"targetDispositionPending":true,"capacityReleased":true,"promptSubmitted":true,"createdAt":"2000-01-01T00:00:00Z","updatedAt":"2000-01-01T00:00:00Z"})
    }
    fn write_leases(registry: &ProfileRegistry, leases: &[Value]) {
        fs::write(
            registry.path().join(REGISTRY),
            serde_json::to_vec(&json!({"version":1,"leases":leases})).unwrap(),
        )
        .unwrap();
    }
    fn fixed_lister() -> TargetLister {
        Arc::new(|_host, port, _budget| {
            Box::pin(async move {
                match port {
                    9301 => EndpointTargets::Listed(["PUBLIC-PRESENT".to_owned()].into()),
                    9302 => EndpointTargets::Unknown,
                    _ => EndpointTargets::Gone,
                }
            })
        })
    }
    #[tokio::test]
    async fn reaper_removes_only_exited_owners_records_whose_targets_are_proven_absent() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let dead = exited_identity();
        let (mut child, live) = live_foreign();
        let absent = released("PUBLIC-absent", &dead, 9301, "PUBLIC-GONE");
        let listed = released("PUBLIC-listed", &dead, 9301, "PUBLIC-PRESENT");
        let live_owner = released("PUBLIC-live", &live, 9301, "PUBLIC-GONE");
        let unknown = released("PUBLIC-unknown", &dead, 9302, "PUBLIC-GONE");
        let refused = released("PUBLIC-refused", &dead, 9303, "PUBLIC-GONE");
        write_leases(
            &registry,
            &[
                absent,
                listed.clone(),
                live_owner.clone(),
                unknown.clone(),
                refused.clone(),
            ],
        );
        let cancel = CancellationToken::new();
        let budget = Duration::from_secs(5);
        // A refused endpoint proves nothing unless the Chrome was launched locally.
        assert_eq!(
            registry
                .reap_with(false, budget, &cancel, fixed_lister())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            registry.records().unwrap()["leases"],
            json!([listed, live_owner, unknown, refused])
        );
        assert!(
            registry
                .empty_disposal_lock(std::time::Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(
            registry
                .reap_with(true, budget, &cancel, fixed_lister())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            registry.records().unwrap()["leases"],
            json!([listed, live_owner, unknown])
        );
        child.kill().unwrap();
        child.wait().unwrap();
        // Once the only blocker is proven absent, disposal is no longer held.
        write_leases(
            &registry,
            &[released("PUBLIC-last", &dead, 9303, "PUBLIC-GONE")],
        );
        assert_eq!(
            registry
                .reap_with(true, budget, &cancel, fixed_lister())
                .await
                .unwrap(),
            1
        );
        registry
            .empty_disposal_lock(std::time::Instant::now() + Duration::from_secs(1))
            .unwrap()
            .release()
            .unwrap();
    }
    #[tokio::test]
    async fn local_admission_reaps_a_refused_loopback_record_beside_admission() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert_eq!(
            list_endpoint_targets("127.0.0.1".into(), port, Duration::from_secs(4)).await,
            EndpointTargets::Gone
        );
        let dead = exited_identity();
        for local in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let record = released("PUBLIC-stale", &dead, port, "PUBLIC-GONE");
            write_leases(&registry, std::slice::from_ref(&record));
            let lease = registry
                .acquire_admission(
                    "PUBLIC-admitted",
                    1,
                    Duration::from_secs(2),
                    &CancellationToken::new(),
                    local,
                )
                .await
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(if local { 8 } else { 1 });
            let stale = loop {
                let leases = registry.records().unwrap()["leases"].clone();
                let stale = leases.as_array().unwrap().contains(&record);
                if !stale || Instant::now() >= deadline {
                    break stale;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert_eq!(stale, !local);
            lease.release().unwrap();
        }
    }
    #[tokio::test]
    async fn endpoint_listing_trusts_only_a_complete_target_list() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (body, expected) in [
            (
                r#"[{"id":"PUBLIC-A","type":"page"},{"id":"PUBLIC-B","type":"other"}]"#,
                EndpointTargets::Listed(["PUBLIC-A".to_owned(), "PUBLIC-B".to_owned()].into()),
            ),
            (r#"[{"id":"PUBLIC-A"}]"#, EndpointTargets::Unknown),
            (r#"{"targets":[]}"#, EndpointTargets::Unknown),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                // The loopback refusal probe connects first and sends nothing.
                let mut socket = loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0u8; 1024];
                    if socket.read(&mut request).await.unwrap_or(0) > 0 {
                        assert!(request.starts_with(b"GET /json/list "));
                        break socket;
                    }
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            });
            assert_eq!(
                list_endpoint_targets("127.0.0.1".into(), port, Duration::from_secs(1)).await,
                expected
            );
            server.await.unwrap();
        }
    }
    /// The slot timeout reports the wait's elapsed seconds, rounded, when the error is formed.
    /// A loaded machine can stall past a 150 ms deadline, so check the reported figure against
    /// the measured wait, then compare the rest of the message with `N` in its place.
    fn elapsed_slot_timeout(error: &anyhow::Error, started: std::time::Instant) -> String {
        let text = error.to_string();
        let (head, tail) = text.split_once(" after ").unwrap();
        let (seconds, rest) = tail.split_once("s (").unwrap();
        let reported: u64 = seconds.parse().unwrap();
        assert!(
            reported as f64 <= started.elapsed().as_secs_f64().round(),
            "{text}"
        );
        format!("{head} after Ns ({rest}")
    }
    #[tokio::test]
    async fn slot_timeout_reports_elapsed_capacity_and_holders_including_lock_deadlines() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let cancel = CancellationToken::new();
        let holder = registry
            .acquire_with_limit("PUBLIC-holder", 1, Duration::from_secs(10), &cancel)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let error = registry
            .acquire_with_limit("PUBLIC-queued", 1, Duration::from_millis(150), &cancel)
            .await
            .err()
            .unwrap();
        let expected = format!(
            "Timed out waiting for ChatGPT browser slot after Ns (1 max; holdingSessions={}:PUBLIC-holder:pid{}:targetpending).",
            &holder.id()[..8],
            std::process::id()
        );
        assert_eq!(elapsed_slot_timeout(&error, started), expected);
        // A registry lock held past the wait's deadline is that wait's timeout, with its cause.
        let lock = DirectoryLock::acquire(
            &home.path().join("oracle-tab-leases.lock"),
            Duration::from_secs(10),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = registry
            .acquire_with_limit("PUBLIC-locked", 1, Duration::from_millis(150), &cancel)
            .await
            .err()
            .unwrap();
        assert_eq!(
            elapsed_slot_timeout(&error, started),
            "Timed out waiting for ChatGPT browser slot after Ns (1 max; holdingSessions=unobserved)."
        );
        assert!(
            format!("{error:#}").contains("deadline reached"),
            "{error:#}"
        );
        lock.release().unwrap();
        holder.release().unwrap();
    }
    #[tokio::test]
    async fn a_failed_release_is_retried_by_the_next_admission_in_this_process() {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let cancel = CancellationToken::new();
        for unissued in [false, true] {
            let lease = registry
                .acquire_with_limit("PUBLIC-failed", 1, Duration::from_secs(1), &cancel)
                .await
                .unwrap();
            let lock = DirectoryLock::acquire(
                &home.path().join("oracle-tab-leases.lock"),
                Duration::from_secs(1),
            )
            .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            if unissued {
                assert!(lease.release_unissued_before(deadline).is_err());
            } else {
                assert!(lease.release_before(Some(deadline)).is_err());
            }
            lock.release().unwrap();
            // This live process still owns the record, so ordinary pruning keeps it.
            assert_eq!(registry.records().unwrap()["leases"][0]["id"], lease.id());
            let next = registry
                .acquire_with_limit("PUBLIC-next", 1, Duration::from_secs(2), &cancel)
                .await
                .unwrap();
            let leases = registry.records().unwrap()["leases"].clone();
            assert_eq!(leases.as_array().unwrap().len(), 1, "{leases}");
            assert_eq!(leases[0]["id"], next.id());
            next.release().unwrap();
            assert!(
                !failed_releases()
                    .iter()
                    .any(|(path, _)| path == registry.path())
            );
        }
    }
    #[test]
    fn legacy_owner_start_within_the_reference_tolerance_stays_alive() {
        let current = ProcessIdentity::current().unwrap();
        for (skew, alive) in [(1_000.0, true), (-9_000.0, true), (20_000.0, false)] {
            let record =
                json!({"pid":current.pid,"processStartedAtMs":current.started_at_ms + skew});
            assert_eq!(
                record_liveness(&record) == Liveness::Alive,
                alive,
                "skew {skew}"
            );
        }
    }
}
