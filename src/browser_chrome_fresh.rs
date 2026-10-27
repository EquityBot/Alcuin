//! A saved fresh-launch capability survives its original worker, never a generic attachment.
use super::*;
use crate::{artifacts::sha256, sessions::SessionStore};
use anyhow::ensure;
use serde_json::json;
use std::{collections::BTreeSet, time::Instant};

#[cfg(test)]
tokio::task_local! {
    pub(super) static TEST_DISPOSAL_STOP: &'static str;
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FreshOwner {
    version: u8,
    kind: String,
    session_id: String,
    controller_identity: ProcessIdentity,
    profile: PathBuf,
    profile_identity: NativeIdentity,
    parent_identity: NativeIdentity,
    process_identity: ProcessIdentity,
    endpoint: AttachmentEndpoint,
    startup_targets: BTreeSet<String>,
    launch_sha256: String,
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(!left.is_zero(), "Fresh browser disposal deadline reached");
    Ok(left)
}
fn launch_digest(profile: &Path) -> Result<(LaunchRecord, String)> {
    let raw = read_record(&profile.join("oracle-chrome-native.json"))?
        .context("Fresh Chrome launch record is missing")?;
    let digest = sha256(&serde_json::to_vec(&raw)?);
    Ok((serde_json::from_value(raw)?, digest))
}
impl ChromeConnection {
    pub(crate) fn fresh_disposal_owner(
        &self,
        session: &str,
        controller: &ProcessIdentity,
    ) -> Result<Option<Value>> {
        let Some(fresh) = &self.fresh else {
            return Ok(None);
        };
        if self.keep_browser {
            return Ok(None);
        }
        ensure!(
            self.child.is_some(),
            "A fresh disposal capability requires its actual child"
        );
        let profile = self
            .profile
            .as_ref()
            .context("Fresh Chrome has no profile")?;
        profile.check()?;
        ensure!(
            profile.path() == fresh.profile.path(),
            "Fresh profile changed before ownership capture"
        );
        let identity = self
            .identity
            .as_ref()
            .context("Fresh Chrome has no process identity")?;
        let entry = NativeEntry::open(profile.path(), true)?;
        let parent = NativeEntry::open(
            profile
                .path()
                .parent()
                .context("Fresh profile has no parent")?,
            true,
        )?;
        let (launch, digest) = launch_digest(profile.path())?;
        ensure!(
            launch.version == 1
                && launch.profile_identity == entry.identity
                && launch.process_identity == *identity
                && launch.endpoint.as_ref() == Some(&self.endpoint)
                && identity.liveness() == Liveness::Alive
                && self
                    .child
                    .as_ref()
                    .is_some_and(|child| child.id() == identity.pid)
                && *controller == ProcessIdentity::current()?,
            "Fresh launch ownership changed"
        );
        Ok(Some(serde_json::to_value(FreshOwner {
            version: 1,
            kind: "fresh-browser-disposal".into(),
            session_id: session.into(),
            controller_identity: controller.clone(),
            profile: profile.path().into(),
            profile_identity: entry.identity,
            parent_identity: parent.identity,
            process_identity: identity.clone(),
            endpoint: self.endpoint.clone(),
            startup_targets: fresh.startup_targets.clone(),
            launch_sha256: digest,
        })?))
    }
}
pub(super) fn require_profile_binding(
    profile: &Path,
    identity: &NativeIdentity,
    saved: &Value,
) -> Result<()> {
    let saved = Path::new(saved.as_str().context("Fresh creator profile is missing")?);
    // The creator uses the wire spelling whilst the native receipt may retain Windows' prefix.
    // Strip only that prefix; do not resolve links or accept a different saved directory.
    fn wire(path: &Path) -> std::borrow::Cow<'_, Path> {
        #[cfg(windows)]
        if let Some(text) = path.to_str() {
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                return std::borrow::Cow::Owned(PathBuf::from(format!(r"\\{unc}")));
            }
            if let Some(disk) = text.strip_prefix(r"\\?\") {
                return std::borrow::Cow::Borrowed(Path::new(disk));
            }
        }
        std::borrow::Cow::Borrowed(path)
    }
    ensure!(
        saved.is_absolute() && wire(saved) == wire(profile),
        "Fresh creator profile belongs to a different directory"
    );
    // A completed disposal retry may legitimately find the directory already removed.
    // Whilst it exists, bind both spellings to the same no-follow native directory identity.
    if profile.try_exists()? {
        let entry = NativeEntry::open(saved, true)?;
        ensure!(
            entry.identity == *identity,
            "Fresh creator profile identity changed"
        );
        entry.require_matches(profile)?;
    }
    Ok(())
}
#[test]
fn creator_profile_binding_accepts_wire_spelling_and_rejects_changed_identity() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("PUBLIC-profile");
    let other = home.path().join("PUBLIC-other");
    fs::create_dir(&profile).unwrap();
    fs::create_dir(&other).unwrap();
    let canonical = fs::canonicalize(&profile).unwrap();
    let entry = NativeEntry::open(&canonical, true).unwrap();
    let saved = json!(crate::native_fs::canonical_wire_path(&profile).unwrap());
    require_profile_binding(&canonical, &entry.identity, &saved).unwrap();
    assert!(require_profile_binding(&canonical, &entry.identity, &json!(other)).is_err());
    let other_identity = NativeEntry::open(&other, true).unwrap().identity;
    assert!(require_profile_binding(&canonical, &other_identity, &saved).is_err());
    drop(entry);
    fs::remove_dir(&profile).unwrap();
    require_profile_binding(&canonical, &other_identity, &saved).unwrap();
    assert!(require_profile_binding(&canonical, &other_identity, &json!(other)).is_err());
}
fn require_completed(
    store: &SessionStore,
    session: &str,
    owner: &FreshOwner,
    receipt: &Value,
) -> Result<Value> {
    let meta = store.stored_for_update(session)?;
    ensure!(
        owner.version == 1
            && owner.kind == "fresh-browser-disposal"
            && owner.session_id == session
            && meta["status"] == "completed"
            && meta["dispatch"]["state"] == "completed"
            && meta["browser"]["config"]["keepBrowser"] != true
            && meta["browser"]["runtime"]["freshProfileOwner"] == *receipt
            && meta["browser"]["runtime"]["targetDispositionPending"] == false
            && meta["recoveryRequired"] != true,
        "Fresh disposal requires unchanged completed retirement"
    );
    let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    let record = journal.read(&creator["allocationId"])?;
    ensure!(
        record["creator"] == *creator
            && creator["sessionId"] == session
            && record["nativeOwnerIdentity"] == json!(owner.controller_identity)
            && record["disposition"]["decision"] == "retired"
            && record["disposition"]["absence"] == "confirmed-absent"
            && record["disposition"]["target"] == creator["target"]
            && matches!(
                record["commit"]["kind"].as_str(),
                Some("session" | "research-report")
            )
            && record["commit"]["sessionId"] == session
            && record["finalisedAt"]
                .as_str()
                .is_some_and(|time| !time.is_empty())
            && creator["target"]["browserWSEndpoint"] == owner.endpoint.browser_ws_endpoint
            && creator["target"]["host"] == owner.endpoint.host
            && creator["target"]["port"] == owner.endpoint.port,
        "Fresh disposal is not bound to the completed target creator"
    );
    require_profile_binding(
        &owner.profile,
        &owner.profile_identity,
        &creator["lease"]["profileDir"],
    )?;
    journal.verify(&record)?;
    Ok(meta)
}
fn survivor_receipt(owner: &Value, record: &Value, id: &str) -> Result<Value> {
    ensure!(
        crate::browser_target::valid_id(id)
            && record["creator"]["target"]["targetId"] != id
            && record["retirement"]["version"] == 1
            && record["retirement"]["id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            && record["retirement"]["commitSha256"]
                == sha256(&serde_json::to_vec(&record["commit"])?),
        "Fresh survivor has no exact retirement binding"
    );
    Ok(
        json!({"version":1,"freshProfileOwner":owner,"creator":record["creator"],
        "retirement":record["retirement"],"targetId":id}),
    )
}
/// Called with the exact successful Target.createTarget reply before the owned tab closes.
pub(crate) fn record_fresh_survivor(
    store: &SessionStore,
    record: &Value,
    id: &str,
    deadline: Instant,
) -> Result<()> {
    let Some(session) = record["creator"]["sessionId"].as_str() else {
        return Ok(());
    };
    let saved = store.stored_for_update(session)?;
    let Some(owner) = saved["browser"]["runtime"]
        .get("freshProfileOwner")
        .filter(|owner| owner.is_object())
    else {
        return Ok(());
    };
    let parsed: FreshOwner = serde_json::from_value(owner.clone())?;
    ensure!(
        parsed.version == 1
            && parsed.session_id == session
            && parsed.controller_identity
                == serde_json::from_value::<ProcessIdentity>(
                    record["nativeOwnerIdentity"].clone()
                )?
            && record["creator"]["target"]["browserWSEndpoint"]
                == parsed.endpoint.browser_ws_endpoint
            && record["commit"]["sessionId"] == session
            && matches!(
                record["commit"]["kind"].as_str(),
                Some("session" | "research-report")
            ),
        "Fresh survivor belongs to a different launch or completion"
    );
    require_profile_binding(
        &parsed.profile,
        &parsed.profile_identity,
        &record["creator"]["lease"]["profileDir"],
    )?;
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    ensure!(
        journal.read(&record["creator"]["allocationId"])? == *record,
        "Fresh survivor journal changed"
    );
    journal.verify(record)?;
    let receipt = survivor_receipt(owner, record, id)?;
    store.update_until(session, deadline, |current| {
        ensure!(current == &saved, "Fresh survivor session changed before persistence");
        let previous = &current["browser"]["runtime"]["freshDisposalSurvivor"];
        ensure!(previous.is_null() || previous == &receipt, "Fresh survivor receipt cannot be replaced");
        Ok(Some(json!({"browser":crate::sessions::merge_object(&current["browser"], &json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"], &json!({"freshDisposalSurvivor":receipt}))}))})))
    })?;
    Ok(())
}
fn saved_survivor(store: &SessionStore, meta: &Value) -> Result<Option<String>> {
    let receipt = &meta["browser"]["runtime"]["freshDisposalSurvivor"];
    if receipt.is_null() {
        return Ok(None);
    }
    let id = receipt["targetId"]
        .as_str()
        .context("Fresh survivor target is missing")?;
    let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let record =
        crate::browser_disposition::TargetJournal::open(store)?.read(&creator["allocationId"])?;
    ensure!(
        record["creator"] == *creator
            && *receipt
                == survivor_receipt(
                    &meta["browser"]["runtime"]["freshProfileOwner"],
                    &record,
                    id
                )?,
        "Fresh survivor provenance changed"
    );
    Ok(Some(id.to_owned()))
}
fn untouched_targets(
    rows: &Value,
    startup_targets: &BTreeSet<String>,
    survivor: Option<&str>,
) -> Result<()> {
    let rows = rows["targetInfos"]
        .as_array()
        .context("Fresh Chrome target list is incomplete")?;
    ensure!(
        rows.iter().all(
            |row| row["targetId"].as_str().is_some_and(|id| !id.is_empty())
                && row["type"].is_string()
                && row["url"].is_string()
                && ((row["type"] == "page"
                    && row["url"] == "about:blank"
                    && row["targetId"]
                        .as_str()
                        .is_some_and(|id| startup_targets.contains(id) || survivor == Some(id)))
                    || browser_internal(row))
        ),
        "Fresh Chrome still contains a foreign, retained or unproven target"
    );
    Ok(())
}
pub(super) async fn wait_native_exit(identity: &ProcessIdentity, deadline: Instant) -> Result<()> {
    loop {
        match identity.liveness() {
            Liveness::Dead => return Ok(()),
            Liveness::Alive | Liveness::Unknown => {
                let left = remaining(deadline).context(
                    "Disposable blank tabs closed, but natural Chrome exit remains unconfirmed",
                )?;
                tokio::time::sleep(left.min(Duration::from_millis(25))).await;
            }
        }
    }
}
pub(super) async fn close_proven_blanks(
    browser: &Cdp,
    startup_targets: &BTreeSet<String>,
    survivor: Option<&str>,
    identity: &ProcessIdentity,
    deadline: Instant,
) -> Result<()> {
    close_proven_targets(browser, identity, deadline, &|rows| {
        untouched_targets(rows, startup_targets, survivor)
    })
    .await
}
/// A caller supplies the complete inventory policy. Only page targets accepted
/// by that policy are closed; native exit is observed without a process signal.
pub(super) async fn close_proven_targets(
    browser: &Cdp,
    identity: &ProcessIdentity,
    deadline: Instant,
    qualify: &(impl Fn(&Value) -> Result<()> + Sync),
) -> Result<()> {
    let cancel = CancellationToken::new();
    let inventory = browser
        .call(
            "Target.getTargets",
            json!({}),
            remaining(deadline)?,
            &cancel,
        )
        .await?;
    qualify(&inventory)?;
    let ids: BTreeSet<_> = inventory["targetInfos"]
        .as_array()
        .context("Missing fresh inventory")?
        .iter()
        .filter(|row| row["type"] == "page")
        .map(|row| {
            row["targetId"]
                .as_str()
                .context("Missing disposable target identity")
                .map(str::to_owned)
        })
        .collect::<Result<_>>()?;
    for id in ids {
        // Recheck the complete inventory, including navigation of a recorded blank.
        // CDP cannot make this atomic with closeTarget; never escalate to browser shutdown.
        let current = browser
            .call(
                "Target.getTargets",
                json!({}),
                remaining(deadline)?,
                &cancel,
            )
            .await?;
        qualify(&current)?;
        if !current["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["targetId"] == id)
        {
            continue;
        }
        let _ = browser
            .close_owned_target(&id, remaining(deadline)?, &cancel)
            .await;
        loop {
            if identity.liveness() == Liveness::Dead {
                return Ok(());
            }
            // Native exit is sufficient even if Chrome stops answering CDP first.
            // A close acknowledgement may precede actual target disappearance.
            // Observe until absence; never send another close for that target.
            let observation = browser.call(
                "Target.getTargets",
                json!({}),
                remaining(deadline)?,
                &cancel,
            );
            let after = match tokio::select! {
                biased;
                exit = wait_native_exit(identity, deadline) => {
                    exit?;
                    return Ok(());
                }
                result = observation => result
            } {
                Ok(rows) => rows,
                Err(error) => {
                    wait_native_exit(identity, deadline)
                        .await
                        .with_context(|| {
                            format!("Post-close inventory was unavailable: {error:#}")
                        })?;
                    return Ok(());
                }
            };
            qualify(&after)?;
            if after["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["targetId"] != id)
            {
                break;
            }
            let left =
                remaining(deadline).context("Disposable blank target absence is unconfirmed")?;
            tokio::time::sleep(left.min(Duration::from_millis(25))).await;
        }
    }
    Ok(())
}
pub(super) async fn qualify_processes(
    browser: &Cdp,
    profile: &Path,
    identity: &ProcessIdentity,
    deadline: Instant,
) -> Result<Vec<ProcessIdentity>> {
    loop {
        // A helper may exit between CDP inventory and native identity lookup.
        // Retry observation only; no close is authorised by an incomplete scan.
        match qualify_processes_once(browser, profile, identity, deadline).await {
            Ok(observed) => return Ok(observed),
            Err(error) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left <= Duration::from_millis(25) {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}
async fn qualify_processes_once(
    browser: &Cdp,
    profile: &Path,
    identity: &ProcessIdentity,
    deadline: Instant,
) -> Result<Vec<ProcessIdentity>> {
    let info = browser
        .call(
            "SystemInfo.getProcessInfo",
            json!({}),
            remaining(deadline)?,
            &CancellationToken::new(),
        )
        .await?;
    let rows = info["processInfo"]
        .as_array()
        .context("Fresh Chrome process list is incomplete")?;
    ensure!(
        !rows.is_empty() && rows.len() <= 1024,
        "Fresh Chrome process list exceeds its bound"
    );
    let mut observed = Vec::new();
    let mut browsers = Vec::new();
    for row in rows {
        remaining(deadline)?;
        let pid = row["id"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .filter(|id| *id > 0)
            .context("Fresh Chrome process identity is malformed")?;
        let kind = row["type"]
            .as_str()
            .context("Fresh Chrome process type is missing")?;
        let process = ProcessIdentity::read(pid)?
            .context("Fresh Chrome process disappeared during qualification")?;
        if kind == "browser" {
            browsers.push(process.clone());
        }
        observed.push(process);
    }
    ensure!(
        browsers == [identity.clone()]
            && identity.liveness() == Liveness::Alive
            && profile_processes(profile)? == [identity.pid],
        "Fresh Chrome no longer has its exact native launch identity"
    );
    Ok(observed)
}

/// Called only after target absence and lease release. No target is selected or created.
pub(crate) async fn retire_saved_fresh(
    store: &SessionStore,
    session: &str,
    timeout: Duration,
) -> Result<Option<Value>> {
    let deadline = Instant::now() + timeout;
    let meta = store.stored_for_update(session)?;
    let Some(receipt) = meta["browser"]["runtime"]
        .get("freshProfileOwner")
        .filter(|v| v.is_object())
        .cloned()
    else {
        return Ok(None);
    };
    let owner: FreshOwner = serde_json::from_value(receipt.clone())?;
    ensure!(
        owner.profile.is_absolute()
            && owner
                .profile
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("alcuin-browser-"))
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "Fresh disposal profile is not an exact disposable launch directory"
    );
    require_completed(store, session, &owner, &receipt)?;
    owner.endpoint.validate()?;
    // Keep launch's global -> profile -> admission lock order. Connecting is already admitted.
    let startup = startup_admission(remaining(deadline)?, &CancellationToken::new()).await?;
    let parent = NativeEntry::open(
        owner
            .profile
            .parent()
            .context("Fresh profile has no parent")?,
        true,
    )?;
    ensure!(
        parent.identity == owner.parent_identity,
        "Fresh profile parent changed"
    );
    let marker_path = crate::browser_profile::fresh_disposal_marker(&owner.profile)
        .context("Fresh disposal marker has no exact profile path")?;
    let marker = read_record(&marker_path)?;
    if let Some(marker) = &marker {
        ensure!(
            marker["version"] == 1 && marker["owner"] == receipt,
            "Fresh disposal marker belongs to a different launch"
        );
    }
    if !owner.profile.try_exists()? {
        ensure!(
            owner.process_identity.liveness() == Liveness::Dead,
            "Fresh Chrome remains alive without its profile"
        );
        if marker.is_some() {
            crate::native_fs::remove_owned_file(
                &marker_path,
                &NativeEntry::open(&marker_path, false)?,
            )?;
        }
        startup.release_until(deadline)?;
        return Ok(Some(json!({"version":1,"disposition":"removed"})));
    }
    let mut fresh = TemporaryProfile::reopen_owned(
        &owner.profile,
        &owner.profile_identity,
        &owner.parent_identity,
    )?;
    let profile = ProfileRegistry::open_existing(&owner.profile)?;
    let bringup = DirectoryLock::acquire_async(
        &owner.profile.join("oracle-browser-bringup.lock"),
        remaining(deadline)?,
        &CancellationToken::new(),
    )
    .await?;
    let admission = profile.empty_disposal_lock(deadline)?;
    if marker.is_none() {
        let (launch, digest) = launch_digest(&owner.profile)?;
        ensure!(
            digest == owner.launch_sha256
                && launch.version == 1
                && launch.profile_identity == owner.profile_identity
                && launch.process_identity == owner.process_identity
                && launch.endpoint.as_ref() == Some(&owner.endpoint)
                && NativeEntry::open(&launch.executable, false)?.identity
                    == launch.executable_identity,
            "Fresh launch receipt or executable identity changed"
        );
    } else {
        ensure!(
            owner.process_identity.liveness() == Liveness::Dead,
            "A retiring profile cannot authorise another browser close"
        );
    }
    // Refresh after exclusion; another disposer may have saved native exit evidence whilst we waited.
    let meta = require_completed(store, session, &owner, &receipt)?;
    let mut observed: Vec<ProcessIdentity> =
        meta["browser"]["runtime"]["freshProfileDisposal"]["observedProcesses"]
            .as_array()
            .map(|_| {
                serde_json::from_value(
                    meta["browser"]["runtime"]["freshProfileDisposal"]["observedProcesses"].clone(),
                )
            })
            .transpose()?
            .unwrap_or_default();
    match owner.process_identity.liveness() {
        Liveness::Alive => {
            ensure!(
                read_devtools_endpoint(&owner.profile)?.as_ref() == Some(&owner.endpoint),
                "Fresh Chrome endpoint changed"
            );
            let browser = owner
                .endpoint
                .connect_admitted(remaining(deadline)?, &CancellationToken::new())
                .await?;
            let result = async {
                observed = qualify_processes(&browser, &owner.profile, &owner.process_identity, deadline).await.context("Fresh native process qualification failed")?;
                let current = require_completed(store, session, &owner, &receipt)?;
                let survivor = saved_survivor(store, &current)?;
                untouched_targets(&browser.call("Target.getTargets", json!({}), remaining(deadline)?, &CancellationToken::new()).await?, &owner.startup_targets, survivor.as_deref())?;
                let completed = require_completed(store, session, &owner, &receipt)?;
                store.update_until(session, deadline, |current| {
                    ensure!(current["browser"]["runtime"]["freshProfileOwner"] == receipt
                        && current == &completed, "Fresh disposal owner changed before close");
                    Ok(Some(json!({"browser":crate::sessions::merge_object(&current["browser"], &json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"], &json!({"freshProfileDisposal":{"version":1,"disposition":"closing","observedProcesses":observed}}))}))})))
                })?;
                require_completed(store, session, &owner, &receipt)?;
                profile.check()?;
                parent.require_matches(owner.profile.parent().context("Fresh profile has no parent")?)?;
                ensure!(launch_digest(&owner.profile)?.1 == owner.launch_sha256,
                    "Fresh launch receipt changed before close");
                close_proven_blanks(&browser, &owner.startup_targets, survivor.as_deref(), &owner.process_identity, deadline).await.context("Exact disposable blank closure remains unconfirmed")?;
                wait_native_exit(&owner.process_identity, deadline).await?;
                Ok::<(), anyhow::Error>(())
            }.await;
            let _ = browser
                .disconnect_and_wait(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(250)),
                )
                .await;
            result?;
        }
        Liveness::Dead => {}
        Liveness::Unknown => {
            bail!("Fresh Chrome native identity is unknown; preserving its process and profile")
        }
    }
    #[cfg(test)]
    ensure!(
        TEST_DISPOSAL_STOP
            .try_with(|stage| *stage != "after-browser-exit")
            .unwrap_or(true),
        "PUBLIC injected interruption after native browser exit"
    );
    // A profile scan alone does not attest renderer/helper exit. Require the saved native set too.
    let proven_exit = loop {
        if !observed.is_empty()
            && observed
                .iter()
                .all(|identity| identity.liveness() == Liveness::Dead)
            && profile_processes(&owner.profile).is_ok_and(|owners| owners.is_empty())
        {
            break true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || observed.is_empty() {
            break false;
        }
        tokio::time::sleep(left.min(Duration::from_millis(25))).await;
    };
    if proven_exit {
        let expected = json!({"version":1,"owner":receipt,"observedProcessesSha256":sha256(&serde_json::to_vec(&observed)?)});
        ensure!(
            marker.as_ref().is_none_or(|saved| *saved == expected),
            "Fresh disposal native exit evidence changed"
        );
        crate::storage::atomic_json_until(&marker_path, &expected, deadline)?;
        parent.require_matches(
            owner
                .profile
                .parent()
                .context("Fresh profile has no parent")?,
        )?;
    }
    admission.release_until(deadline)?;
    bringup.release_until(deadline)?;
    drop(profile);
    let outcome = if proven_exit {
        #[cfg(test)]
        if TEST_DISPOSAL_STOP
            .try_with(|stage| *stage == "after-launch-record-removal")
            .unwrap_or(false)
        {
            let launch = owner.profile.join("oracle-chrome-native.json");
            crate::native_fs::remove_owned_file(&launch, &NativeEntry::open(&launch, false)?)?;
            bail!("PUBLIC injected interruption after launch record removal");
        }
        fresh.cleanup_until(deadline)?;
        crate::native_fs::remove_owned_file(
            &marker_path,
            &NativeEntry::open(&marker_path, false)?,
        )?;
        json!({"version":1,"disposition":"removed","observedProcesses":observed})
    } else {
        json!({"version":1,"disposition":"browser-exited-profile-retained","observedProcesses":observed,
            "reason":"Native renderer/helper absence remains unproved"})
    };
    startup.release_until(deadline)?;
    Ok(Some(outcome))
}

#[cfg(test)]
pub(super) fn saved_disposal_fixture(
    connection: &ChromeConnection,
    store: &SessionStore,
) -> String {
    let meta = store
        .create(
            json!({"prompt":"PUBLIC fresh cleanup fixture","model":"gpt-6-pro","engine":"browser"}),
            connection.profile.as_ref().unwrap().path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    let controller = ProcessIdentity::current().unwrap();
    let owner = connection
        .fresh_disposal_owner(&id, &controller)
        .unwrap()
        .unwrap();
    let journal = crate::browser_disposition::TargetJournal::open(store).unwrap();
    let creation = crate::browser_disposition::TargetCreation::allocated("PUBLIC-fresh-retired");
    let creator = journal.record_creation(&creation,
        &json!({"host":connection.endpoint.host,"port":connection.endpoint.port,
            "browserWSEndpoint":connection.endpoint.browser_ws_endpoint,"targetId":"PUBLIC-fresh-retired","claimId":uuid::Uuid::new_v4().to_string()}),
        &controller, Some(&id), Some(json!({"id":uuid::Uuid::new_v4().to_string(),"profileDir":connection.profile.as_ref().unwrap().path()}))).unwrap();
    complete_disposal_fixture(store, id, controller, owner, creator)
}
#[cfg(test)]
fn complete_disposal_fixture(
    store: &SessionStore,
    id: String,
    controller: ProcessIdentity,
    owner: Value,
    creator: Value,
) -> String {
    let id = prepare_disposal_fixture(store, id, controller, owner.clone(), creator.clone());
    let journal = crate::browser_disposition::TargetJournal::open(store).unwrap();
    journal
        .record_disposition(&creator, "confirmed-absent", "PUBLIC fixture absence")
        .unwrap();
    check_disposal_fixture(store, id, &owner, &creator)
}
#[cfg(test)]
pub(super) fn prepare_disposal_fixture(
    store: &SessionStore,
    id: String,
    controller: ProcessIdentity,
    owner: Value,
    creator: Value,
) -> String {
    let journal = crate::browser_disposition::TargetJournal::open(store).unwrap();
    let source = store.require(&id).unwrap()["options"]["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    let capture = crate::artifacts::capture(
        store,
        &id,
        "PUBLIC-answer.md",
        b"PUBLIC answer",
        &CancellationToken::new(),
    )
    .unwrap();
    store.update(&id, |_| Ok(Some(json!({"status":"completed","completedAt":"PUBLIC completed","recoveryRequired":false,
        "browser":{"capturedAnswer":capture,"config":{"keepBrowser":false},"runtime":{"freshProfileOwner":owner,"targetDispositionPending":false,
            "controllerPid":controller.pid,"controllerProcessStartedAtMs":controller.started_at_ms,
            "conversationId":"PUBLIC-conversation","submittedPromptHash":crate::browser::prompt_fingerprint(&source,"PUBLIC-user"),
            "submittedPromptHashFormat":"original-source-v1","userCommitReceipt":{"messageId":"PUBLIC-user"},
            "acceptedObservation":{"lastAssistantMessageId":"PUBLIC-assistant","text":"PUBLIC answer"},
            "ownedRecoveryTarget":{"creator":creator,"targetId":creator["target"]["targetId"],"claimId":creator["target"]["claimId"]}}}})))).unwrap();
    crate::browser_disposition::completed_dispatch_fixture(store, &id, &creator);
    let commit = journal
        .session_commit(&creator, &store.require(&id).unwrap())
        .unwrap();
    journal.commit(&creator, &commit).unwrap();
    journal.finalise(&commit).unwrap();
    id
}
#[cfg(test)]
fn check_disposal_fixture(
    store: &SessionStore,
    id: String,
    owner: &Value,
    creator: &Value,
) -> String {
    let journal = crate::browser_disposition::TargetJournal::open(store).unwrap();
    let saved = store.stored_for_update(&id).unwrap();
    let parsed: FreshOwner = serde_json::from_value(owner.clone()).unwrap();
    require_completed(store, &id, &parsed, owner).unwrap();
    for changed in ["reservation", "submission-owner", "captured-output"] {
        let mut value = saved.clone();
        match changed {
            "reservation" => {
                value["dispatch"]["reservationId"] = json!(uuid::Uuid::new_v4().to_string())
            }
            "submission-owner" => {
                value["dispatch"]["submissionOwner"]["reservationId"] =
                    json!(uuid::Uuid::new_v4().to_string())
            }
            _ => {
                value["browser"]["capturedAnswer"]["sha256"] =
                    json!(sha256(b"PUBLIC different answer"))
            }
        }
        store.update(&id, |_| Ok(Some(value))).unwrap();
        assert!(
            require_completed(store, &id, &parsed, owner).is_err(),
            "Accepted changed {changed}"
        );
        store.update(&id, |_| Ok(Some(saved.clone()))).unwrap();
    }
    let record = journal.read(&creator["allocationId"]).unwrap();
    let record_path = store.home().join("target-dispositions").join(format!(
        "{}.json",
        creator["allocationId"].as_str().unwrap()
    ));
    for changed in ["native-kind", "other-session"] {
        let mut value = record.clone();
        if changed == "native-kind" {
            value["commit"]["kind"] = json!("native");
            value["commit"]["userMessageId"] = json!("PUBLIC-user");
            journal.verify(&value).unwrap(); // This standalone shape is valid, but cannot dispose a session browser.
        } else {
            value["commit"]["sessionId"] = json!("PUBLIC-other-session");
        }
        crate::storage::atomic_json(&record_path, &value).unwrap();
        assert!(
            require_completed(store, &id, &parsed, owner).is_err(),
            "Accepted {changed}"
        );
        crate::storage::atomic_json(&record_path, &record).unwrap();
    }
    id
}

#[cfg(test)]
pub(super) async fn retire_last_page_fixture(
    connection: &ChromeConnection,
    store: &SessionStore,
    id: &str,
    target: &crate::browser_target::OwnedTarget,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let timeout = Duration::from_secs(5);
    let controller = ProcessIdentity::current()?;
    let owner = connection
        .fresh_disposal_owner(id, &controller)?
        .context("Missing fixture launch owner")?;
    let creation = target
        .creation()
        .context("Missing actual fixture allocation")?;
    target.lease().update(&json!({
        "targetDispositionAllocationId":creation.allocation_id(),
        "chromeHost":connection.endpoint.host,"chromePort":connection.endpoint.port,
        "chromeTargetId":target.id()
    }))?;
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    let creator = journal.record_creation(creation,
        &json!({"host":connection.endpoint.host,"port":connection.endpoint.port,"browserWSEndpoint":connection.endpoint.browser_ws_endpoint,
            "targetId":target.id(),"claimId":uuid::Uuid::new_v4().to_string()}),
        &controller, Some(id), Some(json!({"id":target.lease().id(),"profileDir":crate::native_fs::canonical_wire_path(target.lease().profile().path())?})))?;
    prepare_disposal_fixture(store, id.into(), controller, owner, creator.clone());
    store.update(id, |current| Ok(Some(json!({"recoveryRequired":true,
        "browser":crate::sessions::merge_object(&current["browser"],&json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"],&json!({"targetDispositionPending":true}))}))}))))?;
    ensure!(
        journal.read(&creator["allocationId"])?["retirement"].is_null(),
        "Fixture pre-seeded retirement authority"
    );
    mount_completed_page(
        &connection.browser,
        target.page(),
        &creator["target"]["claimId"],
    )
    .await?;
    for startup in &connection
        .fresh
        .as_ref()
        .context("Missing fresh fixture state")?
        .startup_targets
    {
        connection
            .browser
            .close_owned_target(startup, timeout, &cancel)
            .await?;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let before = connection
            .browser
            .call("Target.getTargets", json!({}), timeout, &cancel)
            .await?;
        let pages: Vec<_> = before["targetInfos"]
            .as_array()
            .context("Missing fixture target inventory")?
            .iter()
            .filter(|row| row["type"] == "page")
            .collect();
        if pages.len() == 1 && pages[0]["targetId"] == target.id() {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Fixture completed page is not the final page: {before}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let disposed = crate::browser_retirement::retire_committed(
        store,
        &journal,
        &connection.browser,
        &connection.endpoint,
        &creator,
    )
    .await?;
    ensure!(
        disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
        "Actual last-page retirement failed: {disposed}"
    );
    crate::browser_runner::settle_completed_retirement(
        store,
        &journal,
        &creator,
        &disposed,
        Instant::now() + timeout,
    )?;
    let saved = store.stored_for_update(id)?;
    let survivor = saved["browser"]["runtime"]["freshDisposalSurvivor"]["targetId"]
        .as_str()
        .context("Real retirement did not persist its survivor")?;
    let after = connection
        .browser
        .call("Target.getTargets", json!({}), timeout, &cancel)
        .await?;
    ensure!(
        after["targetInfos"]
            .as_array()
            .context("Missing post-retirement inventory")?
            .iter()
            .filter(|row| row["type"] == "page")
            .count()
            == 1
            && after["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["targetId"] == survivor && row["url"] == "about:blank")
            && after["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["targetId"] != target.id()),
        "Real survivor/absence receipt differs from Chrome"
    );
    ensure!(
        target.lease().profile().records()?["leases"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Completed lease was not released"
    );
    let foreign = connection
        .browser
        .call(
            "Target.createTarget",
            json!({"url":"about:blank"}),
            timeout,
            &cancel,
        )
        .await?["targetId"]
        .as_str()
        .context("Missing fixture foreign page")?
        .to_owned();
    let reopened = SessionStore::new(store.home());
    ensure!(
        retire_saved_fresh(&reopened, id, timeout).await.is_err()
            && connection.identity.as_ref().unwrap().liveness() == Liveness::Alive,
        "Proven survivor allowed a foreign blank page to close"
    );
    connection
        .browser
        .close_owned_target(&foreign, timeout, &cancel)
        .await?;
    Ok(())
}
/// Serves the synthetic completed ChatGPT conversation into an owned fixture page and
/// records its target claim, as a completed run leaves its answer tab.
#[cfg(test)]
pub(crate) async fn mount_completed_page(
    browser: &Cdp,
    page: &Cdp,
    claim_id: &Value,
) -> Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let cancel = CancellationToken::new();
    let timeout = Duration::from_secs(5);
    let mut events = browser.events();
    page.call(
        "Fetch.enable",
        json!({"patterns":[{"urlPattern":"*"}]}),
        timeout,
        &cancel,
    )
    .await?;
    let browser = browser.clone();
    let session = page
        .session_id()
        .context("Missing fixture page attachment")?
        .to_owned();
    let interceptor = tokio::spawn(async move {
        let body = r#"<!doctype html><meta http-equiv='Content-Security-Policy' content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'none'; form-action 'none'"><style>textarea,button{min-width:40px;min-height:24px}</style><main><form><textarea id='prompt-textarea'></textarea></form><div id='view'>PUBLIC view</div><article data-turn-key='PUBLIC-user-turn'><div id='source-host'><div data-message-author-role='user' data-message-id='PUBLIC-user'>PUBLIC source</div></div></article><article data-turn-key='PUBLIC-assistant-turn'><div data-message-author-role='assistant' data-message-id='PUBLIC-assistant'><div class='markdown'>PUBLIC answer</div><button data-testid='copy-turn-action-button'>Copy</button></div></article></main>"#;
        while let Ok(event) = events.recv().await {
            if event["method"] != "Fetch.requestPaused" || event["sessionId"] != session {
                continue;
            }
            browser.with_session(session.clone()).call("Fetch.fulfillRequest",json!({"requestId":event["params"]["requestId"],"responseCode":200,
                "responseHeaders":[{"name":"Content-Type","value":"text/html"}],"body":STANDARD.encode(body)}),timeout,&CancellationToken::new()).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let mounted = async {
        let url = "https://chatgpt.com/c/PUBLIC-conversation";
        page.call("Page.navigate",json!({"url":url}),timeout,&cancel).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if page.evaluate("location.href==='https://chatgpt.com/c/PUBLIC-conversation' && document.readyState==='complete' && Boolean(document.querySelector('#source-host'))",timeout,&cancel).await? == true { break; }
            ensure!(tokio::time::Instant::now() < deadline, "Synthetic completed page did not load");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        page.evaluate_with_arguments("(cfg)=>{const root={return:null,stateNode:{}};root.stateNode.current=root;document.querySelector('#view').__reactFiber$PUBLIC={return:root,memoizedProps:{scrollStateConversationId:'PUBLIC-conversation',isChatConversation:true,isTemporaryChat:true,isWorkConversation:false,isSharedConversation:false}};document.querySelector('#source-host').__reactProps$PUBLIC={children:[null,null,{props:{children:[null,{props:{item:{type:'user-message',messageId:'PUBLIC-user',message:'PUBLIC source'}}}]}}]};sessionStorage.setItem('oracle:target-claim',JSON.stringify({id:cfg.claimId,retiring:false}));return true;}",
            &json!([{"claimId":claim_id}]),timeout,&cancel).await?;
        Ok::<_,anyhow::Error>(())
    }.await;
    let disabled = page
        .call("Fetch.disable", json!({}), timeout, &cancel)
        .await;
    interceptor.abort();
    let _ = interceptor.await;
    mounted?;
    disabled?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn exact_blank_closure_rechecks_inventory_and_observes_exit_without_a_reply() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        for case in [
            "closed",
            "delayed-absence",
            "foreign-initial",
            "foreign-before",
            "navigated-before",
            "absent-before",
            "foreign-after",
            "still-present",
            "lost-ack",
            "native-exit",
        ] {
            let home = tempfile::tempdir().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut child = (case == "native-exit")
                .then(|| super::super::tests::synthetic_child(home.path(), true));
            let identity = child
                .as_ref()
                .map(|child| ProcessIdentity::read(child.id()).unwrap().unwrap())
                .unwrap_or_else(|| ProcessIdentity::current().unwrap());
            let owner = FreshOwner {
                version: 1,
                kind: "fresh-browser-disposal".into(),
                session_id: "PUBLIC".into(),
                controller_identity: ProcessIdentity::current().unwrap(),
                profile: home.path().to_owned(),
                profile_identity: NativeEntry::open(home.path(), true)
                    .unwrap()
                    .identity
                    .clone(),
                parent_identity: NativeEntry::open(home.path(), true).unwrap().identity,
                process_identity: identity.clone(),
                endpoint: AttachmentEndpoint {
                    host: "127.0.0.1".into(),
                    port,
                    browser_ws_endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC"),
                },
                startup_targets: BTreeSet::from(["PUBLIC-blank".into()]),
                launch_sha256: sha256(b"PUBLIC"),
            };
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut peer = tokio_tungstenite::accept_async(socket).await.unwrap();
                let blank = json!({"targetId":"PUBLIC-blank","type":"page","url":"about:blank"});
                let foreign =
                    json!({"targetId":"PUBLIC-foreign","type":"page","url":"about:blank"});
                let mut inventory = 0;
                let mut closes = vec![];
                while let Some(Ok(Message::Text(frame))) = peer.next().await {
                    let request: Value = serde_json::from_str(&frame).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "Target.getTargets" => {
                            inventory += 1;
                            if inventory == 3 && case == "native-exit" {
                                let child = child.as_mut().unwrap();
                                child.kill().unwrap();
                                child.wait().unwrap();
                                // Deliberately leave the read-only CDP request unanswered.
                                continue;
                            }
                            let rows = match (inventory, case) {
                                (1, "foreign-initial") => json!([blank, foreign]),
                                (1, _) => json!([blank]),
                                (2, "foreign-before") => json!([blank, foreign]),
                                (2, "navigated-before") => {
                                    json!([{"targetId":"PUBLIC-blank","type":"page","url":"https://example.com/"}])
                                }
                                (2, "absent-before") => json!([]),
                                (2, _) => json!([blank]),
                                (_, "foreign-after") => json!([foreign]),
                                (_, "still-present") => json!([blank]),
                                (3..=5, "delayed-absence") => json!([blank]),
                                _ => json!([]),
                            };
                            json!({"targetInfos":rows})
                        }
                        "Target.closeTarget" => {
                            assert_eq!(request["params"]["targetId"], "PUBLIC-blank");
                            closes.push(request["params"]["targetId"].clone());
                            if case == "lost-ack" {
                                continue;
                            }
                            json!({"success":true})
                        }
                        other => panic!("Unexpected disposal command: {other}"),
                    };
                    peer.send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                }
                closes
            });
            let cancel = CancellationToken::new();
            let browser = Cdp::connect(
                &owner.endpoint.browser_ws_endpoint,
                Duration::from_secs(2),
                &cancel,
            )
            .await
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let result = close_proven_blanks(
                &browser,
                &owner.startup_targets,
                None,
                &owner.process_identity,
                deadline,
            )
            .await;
            let budget = deadline.saturating_duration_since(Instant::now());
            browser
                .disconnect_and_wait(Duration::from_secs(1))
                .await
                .unwrap();
            let closes = tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                result.is_ok(),
                matches!(
                    case,
                    "closed" | "delayed-absence" | "absent-before" | "lost-ack" | "native-exit"
                ),
                "{case}: {result:?}"
            );
            assert_eq!(
                closes.len(),
                usize::from(matches!(
                    case,
                    "closed"
                        | "delayed-absence"
                        | "foreign-after"
                        | "still-present"
                        | "lost-ack"
                        | "native-exit"
                )),
                "{case}"
            );
            if case == "native-exit" {
                assert_eq!(identity.liveness(), Liveness::Dead);
                assert!(
                    budget > Duration::from_secs(1),
                    "Lost reply consumed settlement budget"
                );
            }
        }
    }
    #[test]
    fn fresh_survivor_requires_unchanged_launch_retirement_and_exact_blank_target() {
        let home = tempfile::tempdir().unwrap();
        let profile = home
            .path()
            .join(format!("alcuin-browser-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&profile).unwrap();
        let store = SessionStore::new(home.path().join("PUBLIC-state"));
        let meta = store
            .create(
                json!({"prompt":"PUBLIC fixture", "model":"gpt-6-pro", "engine":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        let controller = ProcessIdentity::current().unwrap();
        let owner = json!(FreshOwner {
            version: 1,
            kind: "fresh-browser-disposal".into(),
            session_id: id.clone(),
            controller_identity: controller.clone(),
            profile: profile.clone(),
            profile_identity: NativeEntry::open(&profile, true).unwrap().identity,
            parent_identity: NativeEntry::open(home.path(), true).unwrap().identity,
            process_identity: controller.clone(),
            endpoint: AttachmentEndpoint {
                host: "127.0.0.1".into(),
                port: 9222,
                browser_ws_endpoint: "ws://127.0.0.1:9222/devtools/browser/PUBLIC-fixture".into()
            },
            startup_targets: BTreeSet::new(),
            launch_sha256: sha256(b"PUBLIC launch")
        });
        let journal = crate::browser_disposition::TargetJournal::open(&store).unwrap();
        let creator = journal.record_creation(&crate::browser_disposition::TargetCreation::allocated("PUBLIC-retired"),
            &json!({"host":"127.0.0.1","port":9222,"browserWSEndpoint":"ws://127.0.0.1:9222/devtools/browser/PUBLIC-fixture","targetId":"PUBLIC-retired","claimId":uuid::Uuid::new_v4().to_string()}),
            &controller, Some(&id), Some(json!({"id":uuid::Uuid::new_v4().to_string(),"profileDir":profile}))).unwrap();
        complete_disposal_fixture(
            &store,
            id.clone(),
            controller,
            owner.clone(),
            creator.clone(),
        );
        let path = store.home().join("target-dispositions").join(format!(
            "{}.json",
            creator["allocationId"].as_str().unwrap()
        ));
        let mut record = journal.read(&creator["allocationId"]).unwrap();
        record["retirement"] = json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"commitSha256":sha256(&serde_json::to_vec(&record["commit"]).unwrap())});
        crate::storage::atomic_json(&path, &record).unwrap();
        let parsed: FreshOwner = serde_json::from_value(owner).unwrap();
        let blank = json!({"targetInfos":[{"targetId":"PUBLIC-survivor","type":"page","url":"about:blank"}]});
        assert!(untouched_targets(&blank, &parsed.startup_targets, None).is_err());
        record_fresh_survivor(
            &store,
            &record,
            "PUBLIC-survivor",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let saved = store.stored_for_update(&id).unwrap();
        let survivor = saved_survivor(&SessionStore::new(store.home().to_owned()), &saved).unwrap();
        untouched_targets(&blank, &parsed.startup_targets, survivor.as_deref()).unwrap();
        let mut foreign = blank.clone();
        foreign["targetInfos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"targetId":"PUBLIC-foreign","type":"page","url":"about:blank"}));
        assert!(untouched_targets(&foreign, &parsed.startup_targets, survivor.as_deref()).is_err());
        let mut navigated = blank.clone();
        navigated["targetInfos"][0]["url"] = json!("https://example.com/");
        assert!(
            untouched_targets(&navigated, &parsed.startup_targets, survivor.as_deref()).is_err()
        );
        assert!(
            record_fresh_survivor(
                &store,
                &record,
                "PUBLIC-replacement",
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        for changed in ["owner", "creator", "retirement", "target"] {
            let mut meta = saved.clone();
            let receipt = &mut meta["browser"]["runtime"]["freshDisposalSurvivor"];
            match changed {
                "owner" => {
                    receipt["freshProfileOwner"]["launchSha256"] =
                        json!(sha256(b"PUBLIC different launch"))
                }
                "creator" => receipt["creator"]["target"]["targetId"] = json!("PUBLIC-other"),
                "retirement" => {
                    receipt["retirement"]["id"] = json!(uuid::Uuid::new_v4().to_string())
                }
                _ => receipt["targetId"] = json!(""),
            }
            assert!(
                saved_survivor(&store, &meta).is_err(),
                "Accepted changed {changed}"
            );
        }
    }
}
