//! Durable creator, result and finalisation evidence for a newly allocated target.
use crate::{
    artifacts::sha256,
    browser,
    native_fs::NativeEntry,
    process::ProcessIdentity,
    sessions::{SessionStore, timestamp},
    storage::{DirectoryLock, atomic_json, atomic_json_until, private_dir, private_file},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

const RECORD_LIMIT: u64 = 512 * 1024;
const SAFE_INTEGER: u64 = 9_007_199_254_740_991;
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Target disposition deadline reached");
    Ok(remaining)
}

/// Standalone native exports have an admission identity without a consultation session.
pub(crate) fn creator_admission(creator: &Value) -> &Value {
    creator
        .get("sessionId")
        .unwrap_or(&creator["lease"]["admissionId"])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetCreation {
    version: u8,
    kind: String,
    allocation_id: String,
    target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observation_url: Option<String>,
}
impl TargetCreation {
    /// Call only after the actual new-target acknowledgement and attachment succeed.
    pub(crate) fn allocated(target_id: &str) -> Self {
        Self {
            version: 1,
            kind: "created".into(),
            allocation_id: Uuid::new_v4().to_string(),
            target_id: target_id.into(),
            observation_url: None,
        }
    }
    pub fn allocation_id(&self) -> &str {
        &self.allocation_id
    }
    pub(crate) fn is_allocated_for(&self, target_id: &str) -> bool {
        self.version == 1
            && self.kind == "created"
            && self.target_id == target_id
            && self.observation_url.is_none()
            && Uuid::parse_str(&self.allocation_id).is_ok()
    }
    pub(crate) fn observation(target_id: &str, allocation_id: &str, url: &str) -> Self {
        Self {
            version: 1,
            kind: "historical-observation".into(),
            allocation_id: allocation_id.into(),
            target_id: target_id.into(),
            observation_url: Some(url.into()),
        }
    }
    pub(crate) fn is_observation_for(&self, target_id: &str) -> bool {
        self.version == 1
            && self.kind == "historical-observation"
            && self.target_id == target_id
            && Uuid::parse_str(&self.allocation_id).is_ok()
            && self
                .observation_url
                .as_deref()
                .and_then(crate::browser::conversation_id)
                .is_some()
    }
    pub(crate) fn observation_url(&self) -> Option<&str> {
        self.is_observation_for(&self.target_id)
            .then_some(self.observation_url.as_deref())
            .flatten()
    }
}
fn uuid(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 36
            && s.bytes().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            })
    })
}
fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
fn disposition(creator: &Value, decision: &str, reason: &str, absence: &str) -> Value {
    json!({"version":1,"allocationId":creator["allocationId"],"decision":decision,"absence":absence,"reason":reason,"checkedAt":timestamp(),"target":creator["target"]})
}
pub struct TargetJournal {
    store: SessionStore,
    path: PathBuf,
    directory: NativeEntry,
}
impl TargetJournal {
    pub fn open(store: &SessionStore) -> Result<Self> {
        let path = store.home().join("target-dispositions");
        private_dir(&path)?;
        let directory = NativeEntry::open(&path, true)?;
        Ok(Self {
            store: store.clone(),
            path,
            directory,
        })
    }
    fn file(&self, id: &Value) -> Result<PathBuf> {
        ensure!(uuid(id), "Invalid target allocation identity");
        self.directory.require_matches(&self.path)?;
        Ok(self.path.join(format!(
            "{}.json",
            id.as_str().context("Missing allocation identity")?
        )))
    }
    pub fn read(&self, id: &Value) -> Result<Value> {
        let path = self.file(id)?;
        let mut entry = NativeEntry::open(&path, false)?;
        ensure!(
            entry.handle.metadata()?.len() <= RECORD_LIMIT,
            "Target disposition record is unverified"
        );
        let mut bytes = vec![];
        (&mut entry.handle)
            .take(RECORD_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= RECORD_LIMIT,
            "Target disposition record is unverified"
        );
        entry.require_matches(&path)?;
        self.directory.require_matches(&self.path)?;
        let record: Value = serde_json::from_slice(&bytes)?;
        let creator = &record["creator"];
        ensure!(
            record["version"] == 1
                && creator["version"] == 1
                && creator["kind"] == "created"
                && creator["allocationId"] == *id
                && creator["controllerPid"]
                    .as_u64()
                    .is_some_and(|pid| pid > 0 && pid <= SAFE_INTEGER)
                && creator.get("controllerStartedAtMs").is_some_and(
                    |start| start.is_null() || start.as_f64().is_some_and(f64::is_finite)
                )
                && creator["target"]["host"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && creator["target"]["targetId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && creator["target"]["port"]
                    .as_u64()
                    .is_some_and(|p| (1..=65535).contains(&p))
                && uuid(&creator["target"]["claimId"])
                && record["disposition"]["version"] == 1
                && record["disposition"]["allocationId"] == *id,
            "Target disposition record is unverified"
        );
        Ok(record)
    }
    pub fn record_creation(
        &self,
        creation: &TargetCreation,
        target: &Value,
        identity: &ProcessIdentity,
        session_id: Option<&str>,
        lease: Option<Value>,
    ) -> Result<Value> {
        ensure!(
            creation.version == 1
                && creation.kind == "created"
                && target["targetId"] == creation.target_id
                && uuid(&json!(creation.allocation_id))
                && uuid(&target["claimId"]),
            "Target creation evidence does not match its owner"
        );
        ensure!(
            *identity == ProcessIdentity::current()?,
            "Target creator is not the current native controller"
        );
        let mut creator = json!({"version":1,"kind":"created","allocationId":creation.allocation_id,"target":target,"controllerPid":identity.pid,"controllerStartedAtMs":identity.started_at_ms,"createdAt":timestamp()});
        if let Some(id) = session_id {
            creator["sessionId"] = json!(id);
        }
        if let Some(lease) = lease {
            creator["lease"] = lease;
        }
        let record = json!({"version":1,"creator":creator,"nativeOwnerIdentity":identity,"disposition":disposition(&creator,"retained","commit-missing","unknown")});
        let path = self.file(&creator["allocationId"])?;
        let mut file = private_file(&path, true)?;
        file.write_all(&serde_json::to_vec_pretty(&record)?)?;
        file.sync_all()?;
        self.directory.require_matches(&self.path)?;
        let saved = self.read(&creator["allocationId"])?;
        ensure!(saved == record, "Target creator was not durably committed");
        Ok(creator)
    }
    fn update(
        &self,
        creator: &Value,
        decide: impl FnOnce(Value) -> Result<Value>,
    ) -> Result<Value> {
        let (lock, current) = self.locked_record(creator)?;
        let next = decide(current)?;
        let saved = self.save(&next)?;
        lock.release()?;
        Ok(saved)
    }
    fn locked_record(&self, creator: &Value) -> Result<(DirectoryLock, Value)> {
        self.locked_record_until(creator, Instant::now() + Duration::from_secs(5))
    }
    fn locked_record_until(
        &self,
        creator: &Value,
        deadline: Instant,
    ) -> Result<(DirectoryLock, Value)> {
        let path = self.file(&creator["allocationId"])?;
        remaining(deadline)?;
        let lock = DirectoryLock::acquire_until(&path.with_extension("lock"), deadline)?;
        let current = self.read(&creator["allocationId"])?;
        remaining(deadline)?;
        ensure!(
            current["creator"] == *creator,
            "Target creator changed; preserving its disposition"
        );
        Ok((lock, current))
    }
    fn save_until(&self, next: &Value, deadline: Instant) -> Result<Value> {
        let path = self.file(&next["creator"]["allocationId"])?;
        self.directory.require_matches(&self.path)?;
        atomic_json_until(&path, next, deadline)?;
        let saved = self.read(&next["creator"]["allocationId"])?;
        ensure!(
            saved == *next,
            "Target disposition was not durably committed"
        );
        remaining(deadline)?;
        Ok(saved)
    }
    fn save(&self, next: &Value) -> Result<Value> {
        let path = self.file(&next["creator"]["allocationId"])?;
        self.directory.require_matches(&self.path)?;
        atomic_json(&path, &next)?;
        let saved = self.read(&next["creator"]["allocationId"])?;
        ensure!(
            saved == *next,
            "Target disposition was not durably committed"
        );
        Ok(saved)
    }
    /// The caller proves exact renderer authority and native absence while this record is locked.
    pub(crate) async fn retire_finalised<F, Fut>(&self, creator: &Value, retire: F) -> Result<Value>
    where
        F: FnOnce(&Value) -> Fut,
        Fut: Future<Output = Result<(String, String)>>,
    {
        self.retire_record_until(creator, Instant::now() + Duration::from_secs(5), |record| {
            retire(&record["commit"])
        })
        .await
    }
    pub(crate) fn retirement_id(&self, creator: &Value, commit: &Value) -> Result<String> {
        let record = self.read(&creator["allocationId"])?;
        ensure!(
            record["creator"] == *creator && record["commit"] == *commit,
            "Retirement creator or commitment changed"
        );
        Self::saved_retirement_id(&record)
    }
    fn saved_retirement_id(record: &Value) -> Result<String> {
        let attempt = &record["retirement"];
        ensure!(
            attempt["version"] == 1
                && uuid(&attempt["id"])
                && attempt["commitSha256"] == sha256(&serde_json::to_vec(&record["commit"])?),
            "Retirement attempt does not match its durable commitment"
        );
        Ok(attempt["id"]
            .as_str()
            .context("Retirement identity is missing")?
            .to_owned())
    }
    pub(crate) fn is_current_completed_owner(&self, record: &Value) -> bool {
        let Ok(current) = ProcessIdentity::current() else {
            return false;
        };
        matches!(
            record["commit"]["kind"].as_str(),
            Some("session" | "research-report" | "grok-session")
        ) && record["finalisedAt"]
            .as_str()
            .is_some_and(|time| !time.is_empty())
            && record["creator"]["controllerPid"] == current.pid
            && record["creator"]["controllerStartedAtMs"].as_f64() == Some(current.started_at_ms)
            && serde_json::from_value::<ProcessIdentity>(record["nativeOwnerIdentity"].clone())
                .is_ok_and(|owner| owner == current)
            && self.verify(record).is_ok()
    }
    async fn retire_record_until<F, Fut>(
        &self,
        creator: &Value,
        deadline: Instant,
        retire: F,
    ) -> Result<Value>
    where
        F: FnOnce(Value) -> Fut,
        Fut: Future<Output = Result<(String, String)>>,
    {
        let (lock, mut record) = self.locked_record_until(creator, deadline)?;
        if record["disposition"]["decision"] == "retired" {
            lock.release()?;
            return Ok(record["disposition"].clone());
        }
        let operation_deadline = deadline
            .checked_sub(Duration::from_millis(100))
            .context("Target retirement has no persistence budget")?;
        remaining(operation_deadline)?;
        let current = ProcessIdentity::current()?;
        let owner_allowed = if let Some(identity) = record.get("nativeOwnerIdentity") {
            let identity: ProcessIdentity = serde_json::from_value(identity.clone())?;
            ensure!(
                creator["controllerPid"] == identity.pid
                    && creator["controllerStartedAtMs"].as_f64() == Some(identity.started_at_ms),
                "Target native owner differs from its creator"
            );
            identity == current || identity.liveness() == crate::process::Liveness::Dead
        } else {
            let pid = creator["controllerPid"]
                .as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .context("Invalid target creator PID")?;
            match ProcessIdentity::read(pid) {
                Ok(None) => true,
                Ok(Some(actual)) => {
                    creator["controllerStartedAtMs"]
                        .as_f64()
                        .is_some_and(|start| actual.started_at_ms != start)
                        || (actual == current
                            && creator["controllerStartedAtMs"].as_f64()
                                == Some(actual.started_at_ms))
                }
                Err(_) => false,
            }
        };
        let pending = record["disposition"]["reason"] == "settled-pre-dispatch-close-pending";
        let (absence, reason) = if record["finalisedAt"].as_str().is_none_or(|s| s.is_empty())
            && !pending
        {
            ("unknown".into(), "finalisation-incomplete".into())
        } else if !owner_allowed {
            ("unknown".into(), "active-owner".into())
        } else {
            let result = async {
                if !pending {
                    self.verify(&record)?;
                    if record.get("retirement").is_none() {
                        record["retirement"] = json!({"version":1,"id":Uuid::new_v4().to_string(),
                                "commitSha256":sha256(&serde_json::to_vec(&record["commit"])?) });
                        record = self.save_until(&record, deadline)?;
                    }
                    Self::saved_retirement_id(&record)?;
                }
                remaining(operation_deadline)?;
                // Stop a stalled CDP operation early so its disposition normally lands in budget.
                let result =
                    tokio::time::timeout(remaining(operation_deadline)?, retire(record.clone()))
                        .await;
                result.context("Target retirement operation deadline reached")?
            };
            match tokio::time::timeout(remaining(deadline)?, result).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => (
                    "unknown".into(),
                    if pending {
                        "settled-pre-dispatch-close-pending".into()
                    } else {
                        error.to_string()
                    },
                ),
                Err(_) => (
                    "unknown".into(),
                    if pending {
                        "settled-pre-dispatch-close-pending".into()
                    } else {
                        "target-disposition-deadline".into()
                    },
                ),
            }
        };
        ensure!(
            matches!(absence.as_str(), "confirmed-absent" | "unknown"),
            "Invalid target retirement evidence"
        );
        record["disposition"] = disposition(
            creator,
            if absence == "confirmed-absent" {
                "retired"
            } else {
                "retained"
            },
            &reason,
            &absence,
        );
        // The outcome is decided, so its record outlasts the deadline: a stalled write that gave
        // up here would report a false failure and leave the retirement undecided. The deadline
        // bounds the operation and the lock above. This write stays under the record's lock.
        let saved = self.save(&record)?;
        // The lock would otherwise inherit the spent deadline, and a release that fails once is
        // not retried, so a contended release would strand it.
        lock.release_until(Instant::now() + Duration::from_secs(5))?;
        Ok(saved["disposition"].clone())
    }
    pub(crate) async fn retire_settled_before_dispatch<F, Fut>(
        &self,
        creator: &Value,
        retire: F,
    ) -> Result<Value>
    where
        F: FnOnce(Instant) -> Fut,
        Fut: Future<Output = Result<(String, String)>>,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        self.retire_record_until(creator, deadline, |record| async move {
            ensure!(
                record["disposition"]["reason"] == "settled-pre-dispatch-close-pending",
                "Pre-dispatch retirement has no durable pending disposition"
            );
            retire(deadline).await
        })
        .await
    }
    pub(crate) fn begin_pre_dispatch_retirement(
        &self,
        creator: &Value,
        binding: &Value,
    ) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let (lock, mut record) = self.locked_record_until(creator, deadline)?;
        ensure!(
            record["commit"].is_null()
                && record["nativeOwnerIdentity"] == json!(ProcessIdentity::current()?)
                && record["disposition"]["decision"] != "retired",
            "Pre-dispatch retirement requires the original uncommitted native creator"
        );
        if record["preDispatchRetirement"].is_null() {
            record["preDispatchRetirement"] =
                json!({"version":1,"id":Uuid::new_v4().to_string(),"binding":binding});
        }
        let attempt = &record["preDispatchRetirement"];
        ensure!(
            attempt["version"] == 1 && uuid(&attempt["id"]) && attempt["binding"] == *binding,
            "Pre-dispatch retirement intent or reservation changed"
        );
        let id = attempt["id"]
            .as_str()
            .context("Pre-dispatch retirement identity is missing")?
            .to_owned();
        record["disposition"] = disposition(
            creator,
            "retained",
            "settled-pre-dispatch-close-pending",
            "unknown",
        );
        self.save_until(&record, deadline)?;
        lock.release_until(deadline)?;
        Ok(id)
    }
    /// Advance before inspecting a record so a blocked owner or lock cannot starve later rows.
    pub async fn reconcile<F, Fut, R>(&self, mut retire: F, mut release: R) -> Result<Vec<Value>>
    where
        F: FnMut(Value, Instant) -> Fut,
        Fut: Future<Output = Result<(String, String)>>,
        R: FnMut(&Value, &Value, Instant) -> Result<()>,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        self.reconcile_until(deadline, &mut retire, &mut release)
            .await
    }
    async fn reconcile_until<F, Fut, R>(
        &self,
        deadline: Instant,
        retire: &mut F,
        release: &mut R,
    ) -> Result<Vec<Value>>
    where
        F: FnMut(Value, Instant) -> Fut,
        Fut: Future<Output = Result<(String, String)>>,
        R: FnMut(&Value, &Value, Instant) -> Result<()>,
    {
        self.directory.require_matches(&self.path)?;
        let cursor_path = self.path.join("reconcile-cursor.json");
        let cursor = (|| -> Result<String> {
            let mut entry = NativeEntry::open(&cursor_path, false)?;
            ensure!(
                entry.handle.metadata()?.len() <= 4096,
                "Reconciliation cursor exceeds its bound"
            );
            let mut bytes = vec![];
            (&mut entry.handle).take(4097).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 4096,
                "Reconciliation cursor exceeds its bound"
            );
            entry.require_matches(&cursor_path)?;
            let saved: Value = serde_json::from_slice(&bytes)?;
            Ok(saved["after"].as_str().unwrap_or("").to_owned())
        })()
        .unwrap_or_default();
        let mut reached = cursor.is_empty();
        let mut last = String::new();
        let mut checked = 0;
        let mut results = vec![];
        for entry in fs::read_dir(&self.path)? {
            if checked >= 4 || Instant::now() >= deadline {
                break;
            }
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_file()
                || name.len() != 41
                || !name.to_ascii_lowercase().ends_with(".json")
                || !name.as_bytes()[..36]
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
            {
                continue;
            }
            if !reached {
                if name == cursor {
                    reached = true;
                }
                continue;
            }
            last = name.clone();
            self.directory.require_matches(&self.path)?;
            if atomic_json_until(&cursor_path, &json!({"after":last}), deadline).is_err() {
                break;
            }
            checked += 1;
            let record = match self.read(&json!(&name[..36])) {
                Ok(record) => record,
                Err(_) => continue,
            };
            if !(record["finalisedAt"]
                .as_str()
                .is_some_and(|time| !time.is_empty())
                && record["commit"].is_object())
                && record["disposition"]["reason"] != "settled-pre-dispatch-close-pending"
                && record["disposition"]["decision"] != "retired"
            {
                continue;
            }
            let creator = record["creator"].clone();
            let own_completed = self.is_current_completed_owner(&record);
            if creator["controllerStartedAtMs"].is_null() {
                continue;
            }
            let dead = if let Some(identity) = record.get("nativeOwnerIdentity") {
                serde_json::from_value::<ProcessIdentity>(identity.clone()).is_ok_and(|identity| {
                    creator["controllerPid"] == identity.pid
                        && creator["controllerStartedAtMs"].as_f64() == Some(identity.started_at_ms)
                        && identity.liveness() == crate::process::Liveness::Dead
                })
            } else {
                creator["controllerPid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .is_some_and(|pid| match ProcessIdentity::read(pid) {
                        Ok(None) => true,
                        Ok(Some(actual)) => creator["controllerStartedAtMs"]
                            .as_f64()
                            .is_some_and(|start| start != actual.started_at_ms),
                        Err(_) => false,
                    })
            };
            if !dead && !own_completed {
                continue;
            }
            let result = self
                .retire_record_until(&creator, deadline, |record| retire(record, deadline))
                .await;
            if let Ok(disposed) = result {
                if let Err(error) = release(&creator, &disposed, deadline) {
                    eprintln!(
                        "Owned tab is closed but admission cleanup remains pending: {error:#}"
                    );
                }
                results.push(disposed);
            }
        }
        if Instant::now() < deadline {
            self.directory.require_matches(&self.path)?;
            let _ = atomic_json_until(&cursor_path, &json!({"after":last}), deadline);
        }
        Ok(results)
    }
    pub fn commit(&self, creator: &Value, commit: &Value) -> Result<Value> {
        let saved = self.update(creator, |mut record| {
            let mut duplicate = commit.clone();
            duplicate["committedAt"] = record["commit"]["committedAt"].clone();
            if record["commit"].is_object() && record["commit"] == duplicate {
                self.verify(&record)?;
                return Ok(record);
            }
            ensure!(
                record["disposition"]["decision"] != "retired",
                "Target was already retired"
            );
            record["commit"] = commit.clone();
            record
                .as_object_mut()
                .context("Invalid target record")?
                .remove("retirement");
            record
                .as_object_mut()
                .context("Invalid target record")?
                .remove("finalisedAt");
            record["disposition"] =
                disposition(creator, "retained", "finalisation-incomplete", "unknown");
            self.verify(&record)?;
            Ok(record)
        })?;
        Ok(saved["commit"].clone())
    }
    pub fn finalise(&self, commit: &Value) -> Result<Value> {
        let creator = self.read(&commit["allocationId"])?["creator"].clone();
        self.update(&creator, |mut current| {
            ensure!(current["commit"] == *commit, "Target commitment changed");
            self.verify(&current)?;
            if current["finalisedAt"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                return Ok(current);
            }
            current["finalisedAt"] = json!(timestamp());
            current["disposition"] =
                disposition(&creator, "pending", "completed-and-finalised", "unknown");
            Ok(current)
        })
    }
    pub fn record_disposition(
        &self,
        creator: &Value,
        absence: &str,
        reason: &str,
    ) -> Result<Value> {
        ensure!(
            matches!(absence, "confirmed-absent" | "unknown"),
            "Invalid target absence evidence"
        );
        Ok(self.update(creator, |mut record| {
            record["disposition"] = disposition(
                creator,
                if absence == "confirmed-absent" {
                    "retired"
                } else {
                    "retained"
                },
                reason,
                absence,
            );
            Ok(record)
        })?["disposition"]
            .clone())
    }
    pub(crate) fn session_commit(&self, creator: &Value, session: &Value) -> Result<Value> {
        let observed = &session["browser"]["runtime"]["acceptedObservation"];
        let provider_text = observed["providerText"]
            .as_str()
            .or_else(|| observed["text"].as_str())
            .context("Completed assistant has no content evidence")?;
        if session["browser"]["runtime"]["provider"] == "grok"
            || observed["kind"] == "grok-private-answer"
        {
            crate::browser_grok::accept_saved_answer(
                &crate::browser_runner::request(
                    &self.store,
                    session["id"]
                        .as_str()
                        .context("Grok session identity is missing")?,
                )?,
                session,
                observed,
            )?;
            let mut commit = self.session_commit_content(
                creator,
                session,
                &observed["assistantMessageId"],
                provider_text,
                "text",
            )?;
            commit["kind"] = json!("grok-session");
            commit
                .as_object_mut()
                .expect("Session commitment object")
                .remove("conversationId");
            commit["grokCommitReceipt"] =
                session["browser"]["runtime"]["grokCommitReceipt"].clone();
            commit["grokCompletionReceipt"] = observed["wireReceipt"].clone();
            return Ok(commit);
        }
        if observed["kind"] == "deep-research-report" {
            crate::browser_research::require_report(session, observed)?;
            let mut commit =
                self.session_commit_content(creator, session, &Value::Null, provider_text, "text")?;
            commit["kind"] = json!("research-report");
            let fields = commit.as_object_mut().expect("Session commitment object");
            fields.remove("assistantMessageId");
            let content = fields
                .remove("assistantContent")
                .expect("Session content receipt");
            fields.insert("reportContent".into(), content);
            fields.insert("report".into(), observed["report"].clone());
            return Ok(commit);
        }
        self.session_commit_content(
            creator,
            session,
            browser::assistant_identity(observed)
                .context("Completed assistant identity is missing")?,
            provider_text,
            "text",
        )
    }
    pub(crate) fn historical_session_commit(
        &self,
        creator: &Value,
        session: &Value,
        observed: &Value,
    ) -> Result<Value> {
        let record = self.read(&creator["allocationId"])?;
        ensure!(
            record["creator"] == *creator && record.get("nativeOwnerIdentity").is_none(),
            "Historical observation cannot replace native commitment authority"
        );
        let (answer, representation) = match observed["lastAssistantMarkdown"].as_str() {
            Some(answer) => (answer, "markdown"),
            None => (
                observed["lastAssistantText"]
                    .as_str()
                    .context("Historical assistant text is missing")?,
                "text",
            ),
        };
        let assistant = browser::assistant_identity(observed)
            .context("Historical assistant identity is missing")?;
        ensure!(
            !crate::policy::trim(answer).is_empty()
                && assistant.as_str().is_some_and(|id| !id.is_empty())
                && session["browser"]["capturedAnswer"]["sha256"] == sha256(answer.as_bytes()),
            "Historical capture identity differs from the saved answer"
        );
        self.session_commit_content(creator, session, assistant, answer, representation)
    }
    fn session_commit_content(
        &self,
        creator: &Value,
        session: &Value,
        assistant: &Value,
        provider_text: &str,
        representation: &str,
    ) -> Result<Value> {
        let answer: &Value = &session["browser"]["capturedAnswer"];
        let id = session["id"]
            .as_str()
            .context("Completed browser session has no identity")?;
        let file = answer["file"]
            .as_str()
            .context("Completed browser session has no capture")?;
        crate::sessions::assert_id(file)?;
        let runtime = &session["browser"]["runtime"];
        let mut required = vec![];
        if session["browser"]["requiredTextOutput"].is_object() {
            required.push(session["browser"]["requiredTextOutput"].clone());
        }
        // Completion and recovery bind the same published artifact set before retirement.
        for artifact in session["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| {
                row["path"].as_str().is_some_and(|s| !s.is_empty())
                    && !row["sha256"].is_null()
                    && !row["sizeBytes"].is_null()
            })
        {
            let path = artifact["path"]
                .as_str()
                .context("Completed artifact has no path")?;
            let file = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                crate::prompts::absolute(
                    Path::new(
                        session["cwd"]
                            .as_str()
                            .context("Completed artifact has no working directory")?,
                    ),
                    path,
                )
            };
            required.push(
                json!({"file":file,"bytes":artifact["sizeBytes"],"sha256":artifact["sha256"]}),
            );
        }
        let mut commit = json!({"version":1,"kind":"session","allocationId":creator["allocationId"],"claimId":creator["target"]["claimId"],"conversationId":runtime["conversationId"],"output":{"file":crate::native_fs::canonical_wire_path(&self.store.directory(id)?.join(file))?,"bytes":answer["bytes"],"sha256":answer["sha256"]},"requiredOutputs":required,"committedAt":timestamp(),"sessionId":id,"completedAt":session["completedAt"],"submittedPromptHash":runtime["submittedPromptHash"],"assistantMessageId":assistant,"assistantContent":{"sha256":sha256(provider_text.as_bytes()),"representation":representation}});
        if let Some(format) = runtime.get("submittedPromptHashFormat") {
            commit["submittedPromptHashFormat"] = format.clone();
        }
        Ok(commit)
    }
    pub fn verify(&self, record: &Value) -> Result<()> {
        self.verify_with_session(record, None, None, None)
    }
    pub(crate) fn verify_proposed(&self, record: &Value, session: &Value) -> Result<()> {
        self.verify_with_session(record, Some(session), None, None)
    }
    pub(crate) fn verify_proposed_until(
        &self,
        record: &Value,
        session: &Value,
        cancel: &tokio_util::sync::CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<()> {
        self.verify_with_session(record, Some(session), Some(cancel), deadline)
    }
    fn verify_with_session(
        &self,
        record: &Value,
        proposed: Option<&Value>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
        deadline: Option<Instant>,
    ) -> Result<()> {
        crate::artifacts::check_deadline(deadline)?;
        let creator = &record["creator"];
        let commit = &record["commit"];
        let grok = commit["kind"] == "grok-session";
        ensure!(
            commit["version"] == 1
                && commit["allocationId"] == creator["allocationId"]
                && commit["claimId"] == creator["target"]["claimId"]
                && (if grok {
                    commit.get("conversationId").is_none()
                } else {
                    commit["conversationId"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                })
                && commit["output"]["bytes"].as_u64().is_some_and(|n| n > 0),
            "commit-missing"
        );
        verify_output(&commit["output"], cancel, deadline)?;
        ensure!(
            commit
                .get("requiredOutputs")
                .is_none_or(|outputs| outputs.is_null() || outputs.is_array()),
            "Required output receipts must be an array"
        );
        for output in commit["requiredOutputs"].as_array().into_iter().flatten() {
            verify_output(output, cancel, deadline)?;
        }
        if commit["kind"] == "native" {
            ensure!(
                commit["userMessageId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    && commit["assistantMessageId"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty()),
                "Native commitment identity is incomplete"
            );
            return Ok(());
        }
        ensure!(
            matches!(
                commit["kind"].as_str(),
                Some("session" | "research-report" | "grok-session")
            ),
            "Invalid target commitment kind"
        );
        let id = commit["sessionId"]
            .as_str()
            .context("Session commitment has no session")?;
        let session = if let Some(session) = proposed {
            session.clone()
        } else {
            self.store.require(id)?
        };
        let runtime = &session["browser"]["runtime"];
        let answer = &session["browser"]["capturedAnswer"];
        if record.get("nativeOwnerIdentity").is_some() {
            let request = crate::browser_runner::request(&self.store, id)?;
            let owner: crate::browser_commit::SubmissionOwner =
                serde_json::from_value(runtime["submissionOwner"].clone())?;
            ensure!(
                session["dispatch"]["state"] == "completed"
                    && session["dispatch"]["reservationId"] == request.reservation_id
                    && session["dispatch"]["submissionOwner"] == json!(owner)
                    && owner.claim_id == creator["target"]["claimId"]
                    && uuid(&json!(owner.reservation_id)),
                "Native completed dispatch or its exact reservation changed"
            );
        }
        if grok {
            let observed = &runtime["acceptedObservation"];
            crate::browser_grok::accept_saved_answer(
                &crate::browser_runner::request(&self.store, id)?,
                &session,
                observed,
            )?;
            ensure!(
                record.get("nativeOwnerIdentity").is_some()
                    && commit["grokCommitReceipt"] == runtime["grokCommitReceipt"]
                    && commit["grokCompletionReceipt"] == observed["wireReceipt"]
                    && commit["assistantMessageId"] == observed["assistantMessageId"]
                    && commit["assistantContent"]["representation"] == "text"
                    && commit["assistantContent"]["sha256"]
                        == sha256(
                            observed["text"]
                                .as_str()
                                .context("Grok content is missing")?
                                .as_bytes()
                        )
                    && commit["output"]["sha256"] == commit["assistantContent"]["sha256"],
                "Grok commitment, original native receipt or complete text capture changed"
            );
        } else {
            ensure!(
                runtime["provider"] != "grok"
                    && runtime["acceptedObservation"]["kind"] != "grok-private-answer"
                    && commit.get("grokCommitReceipt").is_none()
                    && commit.get("grokCompletionReceipt").is_none(),
                "Grok evidence cannot use another provider commitment kind"
            );
        }
        if commit["kind"] == "research-report" {
            let observed = &runtime["acceptedObservation"];
            crate::browser_research::require_report(&session, observed)?;
            ensure!(
                observed["completionConfirmed"] == true
                    && observed["stopExists"] == false
                    && observed["strongThinkingActive"] == false
                    && commit["report"] == observed["report"]
                    && commit["reportContent"]["sha256"] == observed["report"]["textSha256"]
                    && commit["reportContent"]["representation"] == "text"
                    && commit["assistantMessageId"].is_null()
                    && commit["assistantContent"].is_null(),
                "Research report commitment changed"
            );
            if commit["reportContent"]["sha256"] != commit["output"]["sha256"] {
                let transcript = crate::browser_turns::saved_transcript(&self.store, &session)?;
                let turns = crate::browser_turns::saved(&session)?;
                let last = turns
                    .last()
                    .context("Research transcript has no final report")?;
                ensure!(
                    sha256(transcript.as_bytes()) == commit["output"]["sha256"]
                        && last["report"] == commit["report"]
                        && last["sourceSha256"] == commit["reportContent"]["sha256"],
                    "Research transcript differs from its committed turns"
                );
            }
        } else {
            ensure!(
                runtime["acceptedObservation"]["kind"] != "deep-research-report"
                    && commit["report"].is_null()
                    && commit["reportContent"].is_null()
                    && commit["assistantMessageId"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                    && digest(&commit["assistantContent"]["sha256"])
                    && matches!(
                        commit["assistantContent"]["representation"].as_str(),
                        Some("text" | "markdown")
                    ),
                "Session assistant commitment is incomplete"
            );
        }
        ensure!(
            session["status"] == "completed"
                && session["completedAt"] == commit["completedAt"]
                && session["id"] == creator["sessionId"]
                && runtime["controllerPid"] == creator["controllerPid"]
                && (creator["controllerStartedAtMs"].is_null()
                    || runtime["controllerProcessStartedAtMs"] == creator["controllerStartedAtMs"])
                && runtime["ownedRecoveryTarget"]["claimId"] == creator["target"]["claimId"]
                && runtime["ownedRecoveryTarget"]["targetId"] == creator["target"]["targetId"]
                && runtime["ownedRecoveryTarget"]["creator"] == *creator
                && runtime["conversationId"] == commit["conversationId"]
                && runtime["submittedPromptHash"] == commit["submittedPromptHash"]
                && runtime.get("submittedPromptHashFormat")
                    == commit.get("submittedPromptHashFormat")
                && answer["version"] == 1
                && answer["sessionId"] == session["id"]
                && answer["encoding"] == "utf8"
                && answer["sha256"] == commit["output"]["sha256"]
                && answer["bytes"] == commit["output"]["bytes"],
            "Session commitment or ownership changed"
        );
        let file = answer["file"]
            .as_str()
            .context("Invalid captured answer filename")?;
        crate::sessions::assert_id(file)?;
        let expected = self.store.directory(id)?.join(file).canonicalize()?;
        ensure!(
            PathBuf::from(
                commit["output"]["file"]
                    .as_str()
                    .context("Missing output path")?
            )
            .canonicalize()?
                == expected,
            "Session commitment output changed"
        );
        let required = commit["requiredOutputs"].as_array();
        if record.get("nativeOwnerIdentity").is_some() {
            let cwd = Path::new(
                session["cwd"]
                    .as_str()
                    .context("Completed output has no working directory")?,
            );
            for artifact in session["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|artifact| {
                    artifact["path"].is_string()
                        && artifact["sha256"].is_string()
                        && artifact["sizeBytes"].is_number()
                })
            {
                let file = crate::prompts::absolute(
                    cwd,
                    artifact["path"]
                        .as_str()
                        .context("Completed artifact path is missing")?,
                );
                ensure!(
                    required.is_some_and(|rows| rows.iter().any(|row| row["file"]
                        .as_str()
                        .is_some_and(|path| Path::new(path) == file)
                        && row["sha256"] == artifact["sha256"]
                        && row["bytes"] == artifact["sizeBytes"])),
                    "Completed artifact set changed after commitment"
                );
            }
        }
        let required_text = &session["browser"]["requiredTextOutput"];
        if let Some(output) = session["options"]["writeOutputPath"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            let cwd = PathBuf::from(
                session["cwd"]
                    .as_str()
                    .context("Missing output working directory")?,
            );
            let expected = cwd.join(output).canonicalize()?;
            ensure!(
                required_text["file"]
                    .as_str()
                    .map(PathBuf::from)
                    .is_some_and(|path| path.canonicalize().is_ok_and(|p| p == expected))
                    && required.is_some_and(|rows| rows
                        .iter()
                        .any(|row| row["file"] == required_text["file"]
                            && row["bytes"] == required_text["bytes"]
                            && row["sha256"] == required_text["sha256"])),
                "Required answer output is unverified"
            );
        }
        if session["options"]["writeArtifacts"] == true {
            let directory = required_text["file"]
                .as_str()
                .map(PathBuf::from)
                .and_then(|p| p.parent().map(|p| p.to_owned()));
            for artifact in session["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["kind"] == "file")
            {
                ensure!(
                    directory.is_some()
                        && required.is_some_and(|rows| rows.iter().any(|row| row["file"]
                            .as_str()
                            .map(PathBuf::from)
                            .and_then(|p| p.parent().map(|p| p.to_owned()))
                            == directory
                            && row["bytes"] == artifact["sizeBytes"]
                            && row["sha256"] == artifact["sha256"])),
                    "Required file export is unverified"
                );
            }
        }
        Ok(())
    }
}
fn verify_output(
    output: &Value,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    deadline: Option<Instant>,
) -> Result<()> {
    crate::artifacts::check_deadline(deadline)?;
    ensure!(
        output["bytes"].as_u64().is_some_and(|n| n <= SAFE_INTEGER) && digest(&output["sha256"]),
        "Committed output evidence is incomplete"
    );
    let path = PathBuf::from(
        output["file"]
            .as_str()
            .context("Committed output path is unavailable")?,
    );
    let mut entry = NativeEntry::open(&path, false)?;
    ensure!(
        entry.handle.metadata()?.len()
            == output["bytes"].as_u64().context("Invalid output size")?,
        "Committed output changed"
    );
    if output.get("dev").is_some() || output.get("ino").is_some() {
        let (device, inode) = crate::native_fs::NativeIdentity::legacy_stat(&entry.handle)?;
        ensure!(
            output
                .get("dev")
                .is_none_or(|v| v.as_str() == Some(&device))
                && output.get("ino").is_none_or(|v| v.as_str() == Some(&inode)),
            "Committed output native identity changed"
        );
    }
    if let Some(identity) = output.get("nativeIdentity") {
        ensure!(
            entry.identity
                == serde_json::from_value::<crate::native_fs::NativeIdentity>(identity.clone())?,
            "Committed output native identity changed"
        );
    }
    let stamp = crate::native_fs::read_stamp(&entry.handle)?;
    crate::artifacts::verify_streamed_file_until(
        &mut entry.handle,
        output["bytes"].as_u64().context("Invalid output size")?,
        output["sha256"].as_str().context("Invalid output digest")?,
        cancel,
        deadline,
    )?;
    entry.require_matches(&path)?;
    ensure!(
        stamp == crate::native_fs::read_stamp(&entry.handle)?,
        "Committed output changed during verification"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn completed_dispatch_fixture(store: &SessionStore, id: &str, creator: &Value) {
    let saved = store.require(id).unwrap();
    let prompt = saved["options"]["prompt"]
        .as_str()
        .unwrap_or("PUBLIC native source");
    let reservation = Uuid::new_v4().to_string();
    let owner =
        json!({"claimId":creator["target"]["claimId"],"reservationId":Uuid::new_v4().to_string()});
    atomic_json(&store.directory(id).unwrap().join("browser-request.json"),
        &json!({"version":1,"sessionId":id,"reservationId":reservation,"prompt":prompt,
            "promptSha256":sha256(prompt.as_bytes()),"optionsSha256":sha256(&serde_json::to_vec(&saved["options"]).unwrap()),
            "uploadPaths":[]})).unwrap();
    store.update(id, |current| Ok(Some(json!({"dispatch":{"state":"completed","reservationId":reservation,"submissionOwner":owner},
        "browser":crate::sessions::merge_object(&current["browser"], &json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"],
            &json!({"submissionOwner":owner}))}))})))).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn native(home: &std::path::Path) -> (TargetJournal, Value, Value) {
        let store = SessionStore::new(home.join(".oracle"));
        let journal = TargetJournal::open(&store).unwrap();
        let creation = TargetCreation::allocated("TARGET-PUBLIC");
        let creator=journal.record_creation(&creation,&json!({"host":"127.0.0.1","port":9222,"targetId":"TARGET-PUBLIC","claimId":Uuid::new_v4().to_string()}),&ProcessIdentity::current().unwrap(),None,None).unwrap();
        let file = home.join("PUBLIC-native.md");
        let bytes = b"PUBLIC exact native answer";
        fs::write(&file, bytes).unwrap();
        let commit = json!({"version":1,"kind":"native","allocationId":creator["allocationId"],"claimId":creator["target"]["claimId"],"conversationId":"consult-public","output":{"file":file,"bytes":bytes.len(),"sha256":sha256(bytes)},"committedAt":"PUBLIC first commitment","userMessageId":"user-public","assistantMessageId":"assistant-public","userText":"PUBLIC original source"});
        (journal, creator, commit)
    }
    fn abandoned(journal: &TargetJournal, creator: &Value) -> Value {
        let mut record = journal.read(&creator["allocationId"]).unwrap();
        let mut owner: ProcessIdentity =
            serde_json::from_value(record["nativeOwnerIdentity"].clone()).unwrap();
        owner.pid = u32::MAX;
        assert_eq!(owner.liveness(), crate::process::Liveness::Dead);
        record["nativeOwnerIdentity"] = json!(owner);
        record["creator"]["controllerPid"] = json!(owner.pid);
        let creator = record["creator"].clone();
        atomic_json(&journal.file(&creator["allocationId"]).unwrap(), &record).unwrap();
        creator
    }
    #[tokio::test]
    async fn reconciliation_advances_its_cursor_and_limits_each_pass_to_four() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        journal
            .record_disposition(&creator, "confirmed-absent", "PUBLIC previous retirement")
            .unwrap();
        let template = journal.read(&creator["allocationId"]).unwrap();
        let _ = abandoned(&journal, &creator);
        for _ in 0..5 {
            let mut row = template.clone();
            let id = json!(Uuid::new_v4().to_string());
            row["creator"]["allocationId"] = id.clone();
            row["commit"]["allocationId"] = id.clone();
            row["disposition"]["allocationId"] = id.clone();
            atomic_json(&journal.file(&id).unwrap(), &row).unwrap();
            abandoned(&journal, &row["creator"]);
        }
        fs::write(
            journal.path.join(format!("{}.json", "é".repeat(18))),
            b"PUBLIC malformed unicode filename",
        )
        .unwrap();
        let visited = std::sync::Mutex::new(vec![]);
        let results = journal
            .reconcile(
                |_, _| async { panic!("Retired traversal cannot issue another close") },
                |creator, _, _| {
                    let cursor: Value = serde_json::from_slice(
                        &fs::read(journal.path.join("reconcile-cursor.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        cursor["after"],
                        format!("{}.json", creator["allocationId"].as_str().unwrap())
                    );
                    visited
                        .lock()
                        .unwrap()
                        .push(creator["allocationId"].clone());
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 4);
        let first = visited.lock().unwrap().clone();
        let next = journal
            .reconcile(
                |_, _| async { panic!("Retired traversal cannot issue another close") },
                |creator, _, _| {
                    assert!(!first.contains(&creator["allocationId"]));
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(next.len(), 2);
    }
    #[tokio::test]
    async fn a_blocked_record_consumes_the_budget_after_advancing_the_cursor() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        journal
            .record_disposition(&creator, "confirmed-absent", "PUBLIC absent")
            .unwrap();
        let mut second = journal.read(&creator["allocationId"]).unwrap();
        let id = json!(Uuid::new_v4().to_string());
        second["creator"]["allocationId"] = id.clone();
        second["commit"]["allocationId"] = id.clone();
        second["disposition"]["allocationId"] = id.clone();
        atomic_json(&journal.file(&id).unwrap(), &second).unwrap();
        abandoned(&journal, &creator);
        abandoned(&journal, &second["creator"]);
        let names: Vec<_> = fs::read_dir(&journal.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.len() == 41 && name.ends_with(".json"))
            .collect();
        assert_eq!(names.len(), 2);
        let blocked = DirectoryLock::acquire(
            &journal.path.join(&names[0]).with_extension("lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let started = Instant::now();
        let rows = journal
            .reconcile_until(
                started + Duration::from_millis(150),
                &mut |_, _| async { panic!("A locked record cannot grant a close") },
                &mut |_, _, _| panic!("A locked record cannot release a lease"),
            )
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
        let cursor: Value =
            serde_json::from_slice(&fs::read(journal.path.join("reconcile-cursor.json")).unwrap())
                .unwrap();
        assert_eq!(cursor["after"], names[0]);
        blocked.release().unwrap();
        let rows = journal
            .reconcile(
                |_, _| async { panic!("An already retired target cannot be closed") },
                |owner, _, _| {
                    assert_eq!(
                        format!("{}.json", owner["allocationId"].as_str().unwrap()),
                        names[1]
                    );
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }
    #[tokio::test]
    async fn reconciliation_preserves_active_owners_and_does_not_retire_when_cursor_write_fails() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        assert!(
            journal
                .reconcile(
                    |_, _| async { panic!("A current controller cannot be reconciled") },
                    |_, _, _| Ok(())
                )
                .await
                .unwrap()
                .is_empty()
        );
        let creator = abandoned(&journal, &creator);
        let before = journal.read(&creator["allocationId"]).unwrap();
        fs::remove_file(journal.path.join("reconcile-cursor.json")).unwrap();
        fs::create_dir(journal.path.join("reconcile-cursor.json")).unwrap();
        assert!(
            journal
                .reconcile(
                    |_, _| async { panic!("A failed cursor write cannot grant retirement") },
                    |_, _, _| Ok(())
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), before);
    }
    #[tokio::test]
    async fn failed_settlement_retries_after_cursor_wrap_without_another_close() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        journal
            .record_disposition(
                &creator,
                "confirmed-absent",
                "PUBLIC absence before interruption",
            )
            .unwrap();
        let creator = abandoned(&journal, &creator);
        let saved = journal.read(&creator["allocationId"]).unwrap();
        let calls = std::cell::Cell::new(0);
        for _ in 0..3 {
            journal
                .reconcile(
                    |_, _| async { panic!("Durable absence cannot issue another close") },
                    |owner, disposition, _| {
                        assert_eq!(owner, &creator);
                        assert_eq!(disposition, &saved["disposition"]);
                        calls.set(calls.get() + 1);
                        if calls.get() == 1 {
                            anyhow::bail!("PUBLIC transient settlement failure");
                        }
                        Ok(())
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), saved);
        assert_eq!(
            fs::read(home.path().join("PUBLIC-native.md")).unwrap(),
            b"PUBLIC exact native answer"
        );
    }
    #[tokio::test]
    async fn retirement_failure_is_persisted_and_retired_records_never_close_again() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        let failed = journal
            .retire_finalised(&creator, |_| async {
                anyhow::bail!("PUBLIC exact retirement failure")
            })
            .await
            .unwrap();
        assert_eq!(failed["decision"], "retained");
        assert_eq!(failed["reason"], "PUBLIC exact retirement failure");
        assert_eq!(
            journal.read(&creator["allocationId"]).unwrap()["disposition"],
            failed
        );
        journal
            .record_disposition(&creator, "confirmed-absent", "PUBLIC confirmed absence")
            .unwrap();
        let creator = abandoned(&journal, &creator);
        let releases = std::cell::Cell::new(0);
        let results = journal
            .reconcile(
                |_, _| async { panic!("A retired record cannot issue another close") },
                |owner, disposed, _| {
                    assert_eq!(owner, &creator);
                    assert_eq!(disposed["absence"], "confirmed-absent");
                    releases.set(releases.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(releases.get(), 1);
    }
    #[tokio::test]
    async fn expired_retirement_budget_cannot_publish_a_disposition() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        let before = journal.read(&creator["allocationId"]).unwrap();
        let result = journal
            .retire_record_until(
                &creator,
                Instant::now() + Duration::from_millis(50),
                |_| async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok(("confirmed-absent".into(), "PUBLIC too late".into()))
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), before);
    }
    #[test]
    fn native_commit_and_finalisation_are_durable_idempotent_and_reverify_outputs() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        assert_eq!(
            journal.read(&creator["allocationId"]).unwrap()["disposition"]["reason"],
            "commit-missing"
        );
        assert_eq!(journal.commit(&creator, &commit).unwrap(), commit);
        let first = journal.finalise(&commit).unwrap();
        let mut duplicate = commit.clone();
        duplicate["committedAt"] = json!("PUBLIC later attempt");
        assert_eq!(journal.commit(&creator, &duplicate).unwrap(), commit);
        assert_eq!(journal.finalise(&commit).unwrap(), first);
        fs::write(
            home.path().join("PUBLIC-native.md"),
            b"PUBLIC changed output",
        )
        .unwrap();
        assert!(journal.commit(&creator, &duplicate).is_err());
        assert!(journal.finalise(&commit).is_err());
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), first);
    }

    #[tokio::test]
    async fn a_stalled_retirement_preserves_its_commit_bound_identity_for_retry() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        // The deadline only has to outlast marking the retirement under a parallel suite; the
        // operation then stalls far beyond it.
        let result = journal
            .retire_record_until(
                &creator,
                Instant::now() + Duration::from_secs(3),
                |record| async move {
                    assert!(record["retirement"]["id"].is_string());
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    panic!("A stalled operation cannot declare absence")
                },
            )
            .await
            .unwrap();
        assert_eq!(result["decision"], "retained");
        let marker = journal.retirement_id(&creator, &commit).unwrap();
        let failed = journal
            .retire_finalised(&creator, |_| async {
                anyhow::bail!("PUBLIC retry failure")
            })
            .await
            .unwrap();
        assert_eq!(failed["decision"], "retained");
        assert_eq!(journal.retirement_id(&creator, &commit).unwrap(), marker);
        let mut changed = commit.clone();
        changed["userText"] = json!("PUBLIC different committed source");
        journal.commit(&creator, &changed).unwrap();
        assert!(journal.retirement_id(&creator, &changed).is_err());
        assert!(journal.retirement_id(&creator, &commit).is_err());
    }
    #[tokio::test]
    async fn a_decision_reached_as_the_budget_ends_is_published_despite_the_stall() {
        for (budget, absence, decision) in [
            (Duration::from_secs(2), "confirmed-absent", "retired"),
            (Duration::from_millis(500), "unknown", "retained"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let (journal, creator, commit) = native(home.path());
            journal.commit(&creator, &commit).unwrap();
            journal.finalise(&commit).unwrap();
            let deadline = Instant::now() + budget;
            // Blocking stands in for a stall between the evidence and its durable write; the
            // operation still completes on its first poll, as a real close can.
            let result = journal
                .retire_record_until(&creator, deadline, |_| async move {
                    std::thread::sleep(budget + Duration::from_millis(50));
                    Ok((absence.to_owned(), "PUBLIC stalled evidence".to_owned()))
                })
                .await
                .unwrap();
            assert_eq!(result["decision"], decision);
            assert_eq!(result["absence"], absence);
            assert!(Instant::now() >= deadline);
            let saved = journal.read(&creator["allocationId"]).unwrap();
            assert_eq!(saved["disposition"], result);
        }
    }
    #[tokio::test]
    async fn a_late_publication_releases_its_lock_despite_contention() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        let budget = Duration::from_millis(500);
        let lock = journal
            .file(&creator["allocationId"])
            .unwrap()
            .with_extension("lock");
        let contender = lock.clone();
        let holder = std::cell::Cell::new(None);
        let result = journal
            .retire_record_until(&creator, Instant::now() + budget, |_| {
                let holder = &holder;
                async move {
                    std::thread::sleep(budget + Duration::from_millis(50));
                    holder.set(Some(crate::storage::hold_recovery_mutex(
                        &contender,
                        Duration::from_millis(500),
                    )));
                    Ok(("unknown".to_owned(), "PUBLIC stalled evidence".to_owned()))
                }
            })
            .await
            .unwrap();
        assert_eq!(result["decision"], "retained");
        assert!(!lock.exists());
        holder.take().unwrap().join().unwrap();
    }
    #[test]
    fn creator_replacement_and_directory_replacement_refuse_writes() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, commit) = native(home.path());
        let path = journal.file(&creator["allocationId"]).unwrap();
        let mut record = journal.read(&creator["allocationId"]).unwrap();
        record["creator"]["createdAt"] = json!("PUBLIC replacement creator");
        atomic_json(&path, &record).unwrap();
        assert!(journal.commit(&creator, &commit).is_err());
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), record);
        let original = journal.path.with_file_name("target-dispositions-original");
        fs::rename(&journal.path, &original).unwrap();
        private_dir(&journal.path).unwrap();
        fs::copy(original.join(path.file_name().unwrap()), &path).unwrap();
        assert!(journal.read(&creator["allocationId"]).is_err());
        assert!(
            journal
                .record_disposition(&creator, "confirmed-absent", "PUBLIC absence")
                .is_err()
        );
    }
    #[test]
    fn session_commit_requires_exact_capture_creator_and_required_output_receipts() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let output = home.path().join("PUBLIC-output.md");
        let session=store.create(json!({"prompt":"PUBLIC journal source","model":"gpt-6-astra","engine":"browser","writeOutputPath":output}),home.path(),None).unwrap();
        let id = session["id"].as_str().unwrap();
        let journal = TargetJournal::open(&store).unwrap();
        let identity = ProcessIdentity::current().unwrap();
        let creator=journal.record_creation(&TargetCreation::allocated("TARGET-PUBLIC"),&json!({"host":"127.0.0.1","port":9222,"targetId":"TARGET-PUBLIC","claimId":Uuid::new_v4().to_string()}),&identity,Some(id),None).unwrap();
        let bytes = b"PUBLIC exact session answer";
        let capture = crate::artifacts::capture(
            &store,
            id,
            "captured-public.md",
            bytes,
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        fs::write(&output, bytes).unwrap();
        let required = json!({"file":output,"bytes":bytes.len(),"sha256":sha256(bytes)});
        store.update(id,|_|Ok(Some(json!({"status":"completed","completedAt":"PUBLIC completion time","browser":{"capturedAnswer":capture,"requiredTextOutput":required,"runtime":{"controllerPid":identity.pid,"controllerProcessStartedAtMs":identity.started_at_ms,"conversationId":"consult-public","submittedPromptHash":"PUBLIC committed prompt hash","submittedPromptHashFormat":"original-source-v1","ownedRecoveryTarget":{"targetId":"TARGET-PUBLIC","claimId":creator["target"]["claimId"],"creator":creator}}}})))).unwrap();
        let mut commit = json!({"version":1,"kind":"session","allocationId":creator["allocationId"],"claimId":creator["target"]["claimId"],"conversationId":"consult-public","output":{"file":store.directory(id).unwrap().join(&capture.file),"bytes":capture.bytes,"sha256":capture.sha256},"sessionId":id,"completedAt":"PUBLIC completion time","submittedPromptHash":"PUBLIC committed prompt hash","submittedPromptHashFormat":"original-source-v1","assistantMessageId":"assistant-public","assistantContent":{"sha256":sha256(bytes),"representation":"text"},"committedAt":"PUBLIC commitment time"});
        completed_dispatch_fixture(&store, id, &creator);
        assert!(journal.commit(&creator, &commit).is_err());
        commit["requiredOutputs"] = json!([required]);
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        let image = home.path().join("PUBLIC-published-image.png");
        let image_bytes = b"PUBLIC saved image";
        fs::write(&image, image_bytes).unwrap();
        store.update(id,|current|Ok(Some(json!({"artifacts":[{"kind":"image","path":image,"sizeBytes":image_bytes.len(),"sha256":sha256(image_bytes)}],"browser":crate::sessions::merge_object(&current["browser"],&json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"],&json!({"acceptedObservation":{"text":String::from_utf8_lossy(bytes),"lastAssistantMessageId":"assistant-public"}}))}))})))).unwrap();
        let all_outputs = journal
            .session_commit(&creator, &store.require(id).unwrap())
            .unwrap();
        assert!(
            all_outputs["requiredOutputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["file"] == json!(image))
        );
        journal.commit(&creator, &all_outputs).unwrap();
        journal.finalise(&all_outputs).unwrap();
        journal
            .record_disposition(&creator, "confirmed-absent", "PUBLIC confirmed absence")
            .unwrap();
        let retired = journal.read(&creator["allocationId"]).unwrap();
        let mut retry = journal
            .session_commit(&creator, &store.require(id).unwrap())
            .unwrap();
        retry["committedAt"] = json!("PUBLIC later cleanup retry");
        assert_eq!(journal.commit(&creator, &retry).unwrap(), all_outputs);
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), retired);
        store
            .update(id, |_| Ok(Some(json!({"status":"cancelled"}))))
            .unwrap();
        assert!(journal.finalise(&all_outputs).is_err());
    }
    #[test]
    fn grok_commitment_keeps_private_turn_identity_without_a_conversation_id() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let prompt = "PUBLIC original Grok source";
        // Provider fallback retains the immutable original Latest request; its
        // actual provider and mode are recorded separately in native evidence.
        let session = store
            .create(
                json!({"prompt":prompt,"model":"gpt-6-astra","engine":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = session["id"].as_str().unwrap();
        let journal = TargetJournal::open(&store).unwrap();
        let identity = ProcessIdentity::current().unwrap();
        let creator = journal.record_creation(&TargetCreation::allocated("TARGET-PUBLIC"),
            &json!({"host":"127.0.0.1","port":9222,"targetId":"TARGET-PUBLIC","claimId":Uuid::new_v4().to_string()}),
            &identity,Some(id),None).unwrap();
        completed_dispatch_fixture(&store, id, &creator);
        let owner = store.require(id).unwrap()["browser"]["runtime"]["submissionOwner"].clone();
        let bytes = b"PUBLIC complete Grok answer";
        let capture = crate::artifacts::capture(
            &store,
            id,
            "captured-public.md",
            bytes,
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let user = format!("response-{}", Uuid::new_v4());
        let assistant = format!("response-{}", Uuid::new_v4());
        let completed_receipt = json!({"version":1,"provider":"grok","kind":"grok-websocket-v1","state":"completed",
            "privateChat":true,"mode":"Auto","claimId":owner["claimId"],"reservationId":owner["reservationId"],
            "dispatchId":Uuid::new_v4().to_string(),"clientMessageId":Uuid::new_v4().to_string(),"userMessageId":user,
            "assistantMessageId":assistant,"responseId":Uuid::new_v4().to_string(),"promptSha256":sha256(prompt.as_bytes())});
        let mut committed_receipt = completed_receipt.clone();
        committed_receipt["state"] = json!("committed");
        committed_receipt["assistantMessageId"] = Value::Null;
        committed_receipt["responseId"] = Value::Null;
        let observed = json!({"state":"completed","provider":"grok","kind":"grok-private-answer","privateChat":true,
            "mode":"Auto","completionConfirmed":true,"stopExists":false,"strongThinkingActive":false,
            "wireReceipt":completed_receipt,"userMessageId":user,"assistantMessageId":assistant,"userText":prompt,
            "text":std::str::from_utf8(bytes).unwrap(),"providerText":std::str::from_utf8(bytes).unwrap(),"images":[],"files":[]});
        store.update(id,|_|Ok(Some(json!({"status":"completed","completedAt":"PUBLIC completion","browser":{
            "provider":"grok","capturedAnswer":capture,"runtime":{"provider":"grok","controllerPid":identity.pid,
            "controllerProcessStartedAtMs":identity.started_at_ms,"submissionOwner":owner,"grokCommitReceipt":committed_receipt,
            "submittedPromptHash":sha256(prompt.as_bytes()),"submittedPromptHashFormat":"grok-request-v1",
            "effectiveModel":"grok-web-auto","modelSelection":{"verified":true,"source":"grok-mode-picker","selectedMode":"Auto","resolvedLabel":"Grok Auto"},
            "acceptedObservation":observed,"ownedRecoveryTarget":{"targetId":"TARGET-PUBLIC","claimId":creator["target"]["claimId"],"creator":creator}}}})))).unwrap();
        let saved = store.require(id).unwrap();
        let request = crate::browser_runner::request(&store, id).unwrap();
        for completed in [false, true] {
            let mut known = saved.clone();
            known["browser"]["runtime"]["grokCommitReceipt"] = completed_receipt.clone();
            if !completed {
                known["browser"]["runtime"]["grokCommitReceipt"]["state"] = json!("committed");
            }
            crate::browser_grok::accept_saved_answer(&request, &known, &observed).unwrap();
            for field in ["responseId", "assistantMessageId"] {
                let mut changed = known.clone();
                changed["browser"]["runtime"]["grokCommitReceipt"][field] =
                    json!(if field == "responseId" {
                        Uuid::new_v4().to_string()
                    } else {
                        format!("response-{}", Uuid::new_v4())
                    });
                assert!(
                    crate::browser_grok::accept_saved_answer(&request, &changed, &observed)
                        .is_err(),
                    "{completed}/{field}"
                );
            }
        }
        let commit = journal.session_commit(&creator, &saved).unwrap();
        assert_eq!(commit["kind"], "grok-session");
        assert!(commit.get("conversationId").is_none());
        assert_eq!(commit["grokCommitReceipt"], committed_receipt);
        assert_eq!(commit["grokCompletionReceipt"], completed_receipt);
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        let record = journal.read(&creator["allocationId"]).unwrap();
        for case in [
            "source",
            "wire",
            "user",
            "mode",
            "private",
            "owner",
            "output",
            "kind",
            "invented-conversation",
            "missing-native",
        ] {
            let mut changed = saved.clone();
            let mut changed_record = record.clone();
            match case {
                "source" => {
                    changed["browser"]["runtime"]["acceptedObservation"]["userText"] =
                        json!("PUBLIC changed source")
                }
                "wire" => {
                    changed["browser"]["runtime"]["acceptedObservation"]["wireReceipt"]["responseId"] =
                        json!(Uuid::new_v4().to_string())
                }
                "user" => {
                    changed["browser"]["runtime"]["grokCommitReceipt"]["userMessageId"] =
                        json!(format!("response-{}", Uuid::new_v4()))
                }
                "mode" => {
                    changed["browser"]["runtime"]["modelSelection"]["selectedMode"] = json!("Fast")
                }
                "private" => {
                    changed["browser"]["runtime"]["acceptedObservation"]["privateChat"] =
                        json!(false)
                }
                "owner" => {
                    changed["browser"]["runtime"]["submissionOwner"]["claimId"] =
                        json!(Uuid::new_v4().to_string())
                }
                "output" => {
                    changed_record["commit"]["assistantContent"]["sha256"] = json!("0".repeat(64))
                }
                "kind" => {
                    changed_record["commit"]["kind"] = json!("session");
                    changed_record["commit"]["conversationId"] = json!("PUBLIC-fake");
                }
                "invented-conversation" => {
                    changed_record["commit"]["conversationId"] = json!("PUBLIC-fake")
                }
                "missing-native" => {
                    changed_record
                        .as_object_mut()
                        .unwrap()
                        .remove("nativeOwnerIdentity");
                }
                _ => unreachable!(),
            }
            assert!(
                journal.verify_proposed(&changed_record, &changed).is_err(),
                "{case}"
            );
        }
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), record);
    }
    #[test]
    fn malformed_and_oversized_records_are_not_read_as_authority() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, _) = native(home.path());
        let path = journal.file(&creator["allocationId"]).unwrap();
        for bytes in [b"null".to_vec(), vec![b' '; RECORD_LIMIT as usize + 1]] {
            fs::write(&path, bytes).unwrap();
            assert!(journal.read(&creator["allocationId"]).is_err());
        }
        assert!(journal.read(&json!("../PUBLIC-other-record")).is_err());
    }
    #[test]
    fn research_commitment_binds_native_report_evidence_without_inventing_assistant_ids() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let source = "PUBLIC complete research request";
        let session = store
            .create(
                json!({"prompt":source,"model":"gpt-6-astra","engine":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = session["id"].as_str().unwrap();
        let journal = TargetJournal::open(&store).unwrap();
        let identity = ProcessIdentity::current().unwrap();
        let creator=journal.record_creation(&TargetCreation::allocated("TARGET-PUBLIC"),&json!({"host":"127.0.0.1","port":9222,"targetId":"TARGET-PUBLIC","claimId":Uuid::new_v4().to_string()}),&identity,Some(id),None).unwrap();
        let text = "PUBLIC completed research report with more than forty characters of exactly captured content.";
        let capture = crate::artifacts::capture(
            &store,
            id,
            "captured-public.md",
            text.as_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let hash = browser::prompt_fingerprint(source, "PUBLIC-user");
        let observed = json!({"kind":"deep-research-report","currentUrl":"https://chatgpt.com/c/consult-public","conversationId":"consult-public","lastUserMessageId":"PUBLIC-user","lastUserOriginalSourceMessageId":"PUBLIC-user","lastUserOriginalSourceText":source,"lastUserTurnIndex":2,"text":text,"completionConfirmed":true,"stopExists":false,"strongThinkingActive":false,"report":{"version":1,"scope":"page-frame","pageTargetId":"TARGET-PUBLIC","targetId":"PUBLIC-report","frameId":"PUBLIC-frame","url":"https://connector_openai_deep_research-public.oaiusercontent.com/report","ownerTurnIndex":2,"completed":true,"researchStarted":false,"textSha256":sha256(text.as_bytes())}});
        store.update(id,|_|Ok(Some(json!({"status":"completed","completedAt":"PUBLIC completion time","browser":{"config":{"researchMode":"deep"},"capturedAnswer":capture,"runtime":{"chromeTargetId":"TARGET-PUBLIC","controllerPid":identity.pid,"controllerProcessStartedAtMs":identity.started_at_ms,"conversationId":"consult-public","submittedPromptHash":hash,"submittedPromptHashFormat":"original-source-v1","userCommitReceipt":{"messageId":"PUBLIC-user","messageFingerprint":hash},"acceptedObservation":observed,"ownedRecoveryTarget":{"targetId":"TARGET-PUBLIC","conversationId":"consult-public","claimId":creator["target"]["claimId"],"creator":creator}}}})))).unwrap();
        completed_dispatch_fixture(&store, id, &creator);
        let completed = store.require(id).unwrap();
        let commit = journal.session_commit(&creator, &completed).unwrap();
        assert_eq!(commit["kind"], "research-report");
        assert!(
            commit.get("assistantMessageId").is_none() && commit.get("assistantContent").is_none()
        );
        assert_eq!(commit["report"], observed["report"]);
        journal.commit(&creator, &commit).unwrap();
        journal.finalise(&commit).unwrap();
        for case in [
            "source",
            "frame",
            "content",
            "status",
            "downgrade",
            "outputs",
        ] {
            let mut changed = completed.clone();
            let mut changed_commit = commit.clone();
            match case {
                "source" => {
                    changed["browser"]["runtime"]["acceptedObservation"]["lastUserOriginalSourceText"] =
                        json!("PUBLIC other request")
                }
                "frame" => changed_commit["report"]["frameId"] = json!("PUBLIC-other-frame"),
                "content" => {
                    changed_commit["reportContent"]["sha256"] =
                        json!(sha256(b"PUBLIC other report"))
                }
                "status" => {
                    changed["browser"]["runtime"]["acceptedObservation"]["completionConfirmed"] =
                        json!(false)
                }
                "outputs" => {
                    changed_commit["output"]["sha256"] = json!(sha256(b"PUBLIC changed capture"))
                }
                _ => {
                    changed_commit["kind"] = json!("session");
                    changed_commit["assistantMessageId"] = json!("PUBLIC-invented");
                    changed_commit["assistantContent"] = changed_commit["reportContent"].clone();
                }
            }
            assert!(
                journal
                    .verify_proposed(
                        &json!({"creator":creator,"commit":changed_commit}),
                        &changed
                    )
                    .is_err(),
                "{case}"
            );
        }
        let initial = "PUBLIC earlier completed answer";
        let first_capture = crate::artifacts::capture(
            &store,
            id,
            "captured-initial.md",
            initial.as_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let transcript =
            crate::browser_turns::transcript(initial, &[("PUBLIC follow-up".into(), text.into())]);
        let transcript_capture = crate::artifacts::capture(
            &store,
            id,
            "captured-transcript.md",
            transcript.as_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let mut sequence = completed.clone();
        sequence["options"]["browserFollowUps"] = json!(["PUBLIC follow-up"]);
        sequence["browser"]["capturedAnswer"] = json!(transcript_capture);
        sequence["browser"]["runtime"]["userCommitReceipt"]["conversation"] =
            json!({"conversationId":"consult-public"});
        let final_receipt = sequence["browser"]["runtime"]["userCommitReceipt"].clone();
        sequence["dispatch"] = json!({"turnIndex":1,"turnCapture":capture,"history":[{"userMessageId":"PUBLIC-initial-user","assistantMessageId":"PUBLIC-initial-assistant","assistantTurnId":"PUBLIC-initial-turn","answerSha256":sha256(initial.as_bytes()),"capture":first_capture}]});
        sequence["browser"]["runtime"]["completedBrowserTurns"] = json!([
            {"index":0,"receipt":{"messageId":"PUBLIC-initial-user","conversation":{"conversationId":"consult-public"}},"assistantMessageId":"PUBLIC-initial-assistant","assistantTurnId":"PUBLIC-initial-turn","sourceSha256":sha256(initial.as_bytes()),"capture":first_capture},
            {"index":1,"receipt":final_receipt,"sourceSha256":sha256(text.as_bytes()),"capture":capture,"report":observed["report"]}
        ]);
        let sequence_commit = journal.session_commit(&creator, &sequence).unwrap();
        assert_ne!(
            sequence_commit["output"]["sha256"],
            sequence_commit["reportContent"]["sha256"]
        );
        journal
            .verify_proposed(
                &json!({"creator":creator,"commit":sequence_commit}),
                &sequence,
            )
            .unwrap();
        sequence["browser"]["runtime"]["completedBrowserTurns"][1]["report"]["frameId"] =
            json!("PUBLIC-other-report");
        assert!(
            journal
                .verify_proposed(
                    &json!({"creator":creator,"commit":sequence_commit}),
                    &sequence
                )
                .is_err()
        );
        sequence["browser"]["runtime"]["completedBrowserTurns"][1]["report"] =
            observed["report"].clone();
        let substituted = "PUBLIC different final answer from another same-session capture";
        let replaced_capture = crate::artifacts::capture(
            &store,
            id,
            "captured-replaced-final.md",
            substituted.as_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let replaced_transcript = crate::browser_turns::transcript(
            initial,
            &[("PUBLIC follow-up".into(), substituted.into())],
        );
        let replaced_output = crate::artifacts::capture(
            &store,
            id,
            "captured-replaced-transcript.md",
            replaced_transcript.as_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        sequence["dispatch"]["turnCapture"] = json!(replaced_capture);
        sequence["browser"]["runtime"]["completedBrowserTurns"][1]["capture"] =
            json!(replaced_capture);
        sequence["browser"]["capturedAnswer"] = json!(replaced_output);
        let replaced_commit = journal.session_commit(&creator, &sequence).unwrap();
        assert!(
            journal
                .verify_proposed(
                    &json!({"creator":creator,"commit":replaced_commit}),
                    &sequence
                )
                .is_err(),
            "A different same-session final capture cannot replace the accepted report"
        );
        journal
            .record_disposition(
                &creator,
                "confirmed-absent",
                "PUBLIC absence after verified report commitment",
            )
            .unwrap();
        journal
            .verify(&journal.read(&creator["allocationId"]).unwrap())
            .unwrap();
    }
    #[test]
    fn optional_native_stat_receipt_refuses_same_bytes_on_a_replacement_file() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, mut commit) = native(home.path());
        let file = home.path().join("PUBLIC-native.md");
        let original = fs::File::open(&file).unwrap();
        let (dev, ino) = crate::native_fs::NativeIdentity::legacy_stat(&original).unwrap();
        commit["output"]["dev"] = json!(dev);
        commit["output"]["ino"] = json!(ino);
        journal.commit(&creator, &commit).unwrap();
        fs::rename(&file, home.path().join("PUBLIC-native-original.md")).unwrap();
        fs::write(&file, b"PUBLIC exact native answer").unwrap();
        assert!(journal.finalise(&commit).is_err());
        assert!(journal.read(&creator["allocationId"]).unwrap()["finalisedAt"].is_null());
        drop(original);
    }
    #[test]
    fn malformed_required_output_list_and_missing_controller_start_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let (journal, creator, mut commit) = native(home.path());
        commit["requiredOutputs"] = json!({"PUBLIC malformed":"not an array"});
        assert!(journal.commit(&creator, &commit).is_err());
        let path = journal.file(&creator["allocationId"]).unwrap();
        let mut record = journal.read(&creator["allocationId"]).unwrap();
        record["creator"]
            .as_object_mut()
            .unwrap()
            .remove("controllerStartedAtMs");
        atomic_json(&path, &record).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(journal.read(&creator["allocationId"]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
