//! A persistent launch's saved close authority survives its worker. It permits
//! exact empty-page retirement, never profile deletion or a process signal.
use super::*;
use crate::{
    artifacts::sha256,
    sessions::{SessionStore, merge_object},
};
use anyhow::ensure;
use idle_retirement::IdleStartup;
use serde_json::json;
use std::time::Instant;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdleOwner {
    version: u8,
    kind: String,
    session_id: String,
    controller_identity: ProcessIdentity,
    profile: PathBuf,
    profile_identity: NativeIdentity,
    parent_identity: NativeIdentity,
    process_identity: ProcessIdentity,
    endpoint: AttachmentEndpoint,
    startup: IdleStartup,
    launch_sha256: String,
}
/// Closing began, but the budget ended before native exit was confirmed. Holds the
/// browser and its observed helpers, which usually finish exiting by themselves.
#[derive(Clone, Debug)]
pub(crate) struct ExitPending(Vec<ProcessIdentity>);
impl std::fmt::Display for ExitPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Persistent Chrome closing began, but its natural exit is unconfirmed")
    }
}
impl ExitPending {
    fn new(browser: &ProcessIdentity, observed: &[ProcessIdentity]) -> Self {
        Self(observed.iter().chain([browser]).cloned().collect())
    }
    pub(crate) fn of(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<Self>().cloned()
    }
    /// Waits up to `grace` for every recorded process to end, without CDP, locks or signals.
    pub(crate) async fn wait(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while self
            .0
            .iter()
            .any(|process| process.liveness() != Liveness::Dead)
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
#[cfg(test)]
thread_local! {
    /// Makes the next closing attempt end as if natural exit outlasted its budget.
    static TEST_SLOW_EXIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !left.is_zero(),
        "Persistent browser disposal deadline reached"
    );
    Ok(left)
}
fn launch(profile: &Path) -> Result<(LaunchRecord, String)> {
    let raw = read_record(&profile.join("oracle-chrome-native.json"))?
        .context("Persistent launch record is missing")?;
    Ok((
        serde_json::from_value(raw.clone())?,
        sha256(&serde_json::to_vec(&raw)?),
    ))
}
impl ChromeConnection {
    /// Names the session holding this launch's saved close authority, so a later run that
    /// borrows the browser can retry an idle cleanup that missed. A hint only: every
    /// identity is verified again from the saved owner before anything closes.
    pub(crate) fn record_idle_owner(&self, session: &str) {
        if let Some(profile) = &self.profile
            && let Err(error) = profile.set_idle_owner(session)
        {
            eprintln!("Persistent idle owner hint was not recorded: {error:#}");
        }
    }
    /// Called once this run holds no admission and will use no more targets.
    pub(crate) fn release_attachment(&mut self) {
        self.attachment = None;
    }
    pub(crate) fn idle_owner_hint(&self) -> Option<String> {
        self.profile.as_ref()?.idle_owner()
    }
    pub(crate) fn idle_disposal_owner(
        &self,
        session: &str,
        controller: &ProcessIdentity,
    ) -> Result<Option<Value>> {
        let Some(startup) = &self.idle_startup else {
            return Ok(None);
        };
        ensure!(
            !self.keep_browser && self.fresh.is_none() && self.temporary_profile.is_none(),
            "Persistent close authority requires an owned non-disposable launch"
        );
        let profile = self
            .profile
            .as_ref()
            .context("Persistent profile is missing")?;
        profile.check()?;
        let identity = self
            .identity
            .as_ref()
            .context("Persistent native identity is missing")?;
        let profile_identity = NativeEntry::open(profile.path(), true)?.identity;
        let parent_identity = NativeEntry::open(
            profile
                .path()
                .parent()
                .context("Persistent profile parent is missing")?,
            true,
        )?
        .identity;
        let (launch, digest) = launch(profile.path())?;
        ensure!(
            launch.version == 1
                && launch.profile_identity == profile_identity
                && launch.process_identity == *identity
                && launch.endpoint.as_ref() == Some(&self.endpoint)
                && launch.extra.get(idle_retirement::STARTUP_KEY) == Some(&json!(startup.url))
                && idle_retirement::is_startup_url(&startup.url)
                && self
                    .child
                    .as_ref()
                    .is_some_and(|child| child.id() == identity.pid)
                && identity.liveness() == Liveness::Alive
                && *controller == ProcessIdentity::current()?,
            "Persistent close authority no longer matches its native launch"
        );
        Ok(Some(serde_json::to_value(IdleOwner {
            version: 1,
            kind: "persistent-browser-idle".into(),
            session_id: session.into(),
            controller_identity: controller.clone(),
            profile: profile.path().into(),
            profile_identity,
            parent_identity,
            process_identity: identity.clone(),
            endpoint: self.endpoint.clone(),
            startup: startup.clone(),
            launch_sha256: digest,
        })?))
    }
}
fn require_session(
    store: &SessionStore,
    session: &str,
    owner: &IdleOwner,
    receipt: &Value,
    completed: bool,
) -> Result<(Value, Value)> {
    let meta = store.stored_for_update(session)?;
    ensure!(
        owner.version == 1
            && owner.kind == "persistent-browser-idle"
            && owner.session_id == session
            && owner.profile.is_absolute()
            && idle_retirement::is_startup_url(&owner.startup.url)
            && owner
                .startup
                .target
                .as_deref()
                .is_none_or(crate::browser_target::valid_id)
            && meta["browser"]["config"]["keepBrowser"] != true
            && meta["browser"]["runtime"]["persistentBrowserOwner"] == *receipt,
        "Persistent disposal owner or keep preference changed"
    );
    let creator = &meta["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    let record = journal.read(&creator["allocationId"])?;
    ensure!(
        record["creator"] == *creator
            && creator["sessionId"] == session
            && record["nativeOwnerIdentity"] == json!(owner.controller_identity)
            && record["commit"]["sessionId"] == session
            && matches!(
                record["commit"]["kind"].as_str(),
                Some("session" | "research-report" | "grok-session")
            )
            && record["finalisedAt"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && creator["target"]["browserWSEndpoint"] == owner.endpoint.browser_ws_endpoint
            && creator["target"]["host"] == owner.endpoint.host
            && creator["target"]["port"] == owner.endpoint.port,
        "Persistent disposal is not bound to the finalised target creator"
    );
    if completed {
        ensure!(
            meta["status"] == "completed"
                && meta["dispatch"]["state"] == "completed"
                && meta["recoveryRequired"] == false
                && meta["browser"]["runtime"]["targetDispositionPending"] == false
                && meta["browser"]["runtime"]["targetRetiredAt"].is_string()
                && record["disposition"]["decision"] == "retired"
                && record["disposition"]["absence"] == "confirmed-absent"
                && record["disposition"]["target"] == creator["target"],
            "Persistent disposal requires completed, confirmed target retirement"
        );
    }
    fresh_retirement::require_profile_binding(
        &owner.profile,
        &owner.profile_identity,
        &creator["lease"]["profileDir"],
    )?;
    journal.verify(&record)?;
    Ok((meta, record))
}
fn survivor(owner: &Value, record: &Value, id: &str) -> Result<Value> {
    ensure!(
        crate::browser_target::valid_id(id)
            && record["creator"]["target"]["targetId"] != id
            && record["retirement"]["version"] == 1
            && record["retirement"]["id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            && record["retirement"]["commitSha256"]
                == sha256(&serde_json::to_vec(&record["commit"])?),
        "Persistent survivor has no exact retirement binding"
    );
    Ok(
        json!({"version":1,"persistentBrowserOwner":owner,"creator":record["creator"],"retirement":record["retirement"],"targetId":id}),
    )
}
/// Called only for the exact acknowledged replacement created by retirement.
pub(crate) fn record_idle_survivor(
    store: &SessionStore,
    record: &Value,
    id: &str,
    deadline: Instant,
) -> Result<()> {
    let Some(session) = record["creator"]["sessionId"].as_str() else {
        return Ok(());
    };
    let saved = store.stored_for_update(session)?;
    let receipt = &saved["browser"]["runtime"]["persistentBrowserOwner"];
    if !receipt.is_object() {
        return Ok(());
    }
    let owner: IdleOwner = serde_json::from_value(receipt.clone())?;
    let (saved, original) = require_session(store, session, &owner, receipt, false)?;
    ensure!(original == *record, "Persistent survivor journal changed");
    let value = survivor(receipt, record, id)?;
    store.update_until(session, deadline, |current| {
        ensure!(current == &saved, "Persistent survivor owner changed");
        let previous = &current["browser"]["runtime"]["persistentDisposalSurvivor"];
        ensure!(previous.is_null() || previous == &value, "Persistent survivor cannot be replaced");
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentDisposalSurvivor":value}))}))})))
    })?;
    Ok(())
}
fn require_inventory(startup: &IdleStartup, survivor: Option<&str>, rows: &Value) -> Result<()> {
    // Remove only the exact proven survivor from a copy. The original inventory
    // must still have unique IDs; duplicate survivors cannot bypass that check.
    startup.require_inventory(&json!({"targetInfos":rows["targetInfos"].as_array().context("Persistent target inventory is missing")?
        .iter().filter(|row| !(row["type"] == "page" && row["url"] == "about:blank" && row["targetId"].as_str() == survivor)).cloned().collect::<Vec<_>>()}))?;
    let mut ids = std::collections::BTreeSet::new();
    ensure!(
        rows["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["targetId"]
                .as_str()
                .is_some_and(|id| crate::browser_target::valid_id(id) && ids.insert(id))),
        "Persistent target inventory contains duplicate or invalid IDs"
    );
    Ok(())
}
pub(crate) async fn retire_saved_idle(
    store: &SessionStore,
    session: &str,
    timeout: Duration,
) -> Result<Option<Value>> {
    let deadline = Instant::now() + timeout;
    let meta = store.stored_for_update(session)?;
    let receipt = &meta["browser"]["runtime"]["persistentBrowserOwner"];
    if !receipt.is_object() {
        return Ok(None);
    }
    let mut owner: IdleOwner = serde_json::from_value(receipt.clone())?;
    require_session(store, session, &owner, receipt, true)?;
    owner.endpoint.validate()?;
    let exclusion = startup_admission(remaining(deadline)?, &CancellationToken::new()).await?;
    let parent = NativeEntry::open(
        owner
            .profile
            .parent()
            .context("Persistent profile parent is missing")?,
        true,
    )?;
    ensure!(
        parent.identity == owner.parent_identity,
        "Persistent profile parent changed"
    );
    let profile = ProfileRegistry::open_existing(&owner.profile)?;
    ensure!(
        NativeEntry::open(&owner.profile, true)?.identity == owner.profile_identity,
        "Persistent profile identity changed"
    );
    let bringup = DirectoryLock::acquire_async(
        &owner.profile.join("oracle-browser-bringup.lock"),
        remaining(deadline)?,
        &CancellationToken::new(),
    )
    .await?;
    let admission_profile = Arc::clone(&profile);
    let admission =
        tokio::task::spawn_blocking(move || admission_profile.empty_disposal_lock(deadline))
            .await
            .context("Persistent admission worker failed")??;
    let (recorded_launch, digest) = launch(&owner.profile)?;
    ensure!(
        digest == owner.launch_sha256
            && recorded_launch.version == 1
            && recorded_launch.profile_identity == owner.profile_identity
            && recorded_launch.process_identity == owner.process_identity
            && recorded_launch.endpoint.as_ref() == Some(&owner.endpoint)
            && idle_retirement::recorded_startup_url(&recorded_launch.extra)
                == Some(&json!(owner.startup.url))
            && NativeEntry::open(&recorded_launch.executable, false)?.identity
                == recorded_launch.executable_identity,
        "Persistent launch record or executable changed"
    );
    let (meta, record) = require_session(store, session, &owner, receipt, true)?;
    if let Some(target) =
        meta["browser"]["runtime"]["persistentBrowserDisposal"].get("startupTarget")
    {
        let target = target
            .as_str()
            .filter(|id| crate::browser_target::valid_id(id))
            .context("Saved persistent startup target is invalid")?;
        ensure!(
            owner
                .startup
                .target
                .as_deref()
                .is_none_or(|known| known == target),
            "Saved persistent startup target changed"
        );
        owner.startup.target = Some(target.into());
    }
    let mut observed: Vec<ProcessIdentity> =
        meta["browser"]["runtime"]["persistentBrowserDisposal"]["observedProcesses"]
            .as_array()
            .map(|_| {
                serde_json::from_value(
                    meta["browser"]["runtime"]["persistentBrowserDisposal"]["observedProcesses"]
                        .clone(),
                )
            })
            .transpose()?
            .unwrap_or_default();
    match owner.process_identity.liveness() {
        Liveness::Alive => {
            ensure!(
                read_devtools_endpoint(&owner.profile)?.as_ref() == Some(&owner.endpoint),
                "Persistent Chrome endpoint changed"
            );
            let browser = owner
                .endpoint
                .connect_admitted(remaining(deadline)?, &CancellationToken::new())
                .await?;
            let result = async {
                observed = fresh_retirement::qualify_processes(&browser,&owner.profile,&owner.process_identity,deadline).await?;
                // Pending startup discovery accepts only the exact nonce recorded
                // at process launch. A separately proven last-page survivor is
                // checked below, rather than becoming a general blank allowance.
                if owner.startup.target.is_none() {
                    let rows = browser.call("Target.getTargets",json!({}),remaining(deadline)?,&CancellationToken::new()).await?;
                    let found=rows["targetInfos"].as_array().context("Persistent startup inventory is missing")?.iter()
                        .filter(|row|row["type"]=="page" && row["url"]==owner.startup.url).collect::<Vec<_>>();
                    ensure!(found.len()<=1,"Persistent startup nonce is ambiguous");
                    if let Some(row)=found.first() {owner.startup.target=Some(row["targetId"].as_str().filter(|id|crate::browser_target::valid_id(id)).context("Persistent startup ID is invalid")?.into());}
                }
                let survivor_receipt = &meta["browser"]["runtime"]["persistentDisposalSurvivor"];
                let survivor_id = if survivor_receipt.is_null() {None} else {
                    let id=survivor_receipt["targetId"].as_str().context("Persistent survivor target is missing")?;
                    ensure!(*survivor_receipt==survivor(receipt,&record,id)?,"Persistent survivor provenance changed");Some(id)
                };
                let completed = require_session(store,session,&owner,receipt,true)?.0;
                store.update_until(session,deadline,|current|{
                    ensure!(current==&completed,"Persistent disposal outcome changed before close");
                    let mut disposal=json!({"version":1,"disposition":"closing","profileRetained":true,"observedProcesses":observed});
                    if let Some(target)=&owner.startup.target {disposal["startupTarget"]=json!(target);}
                    Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentBrowserDisposal":disposal}))}))})))
                })?;
                require_session(store,session,&owner,receipt,true)?;
                profile.check()?;parent.require_matches(owner.profile.parent().context("Persistent profile parent is missing")?)?;
                ensure!(launch(&owner.profile)?.1==owner.launch_sha256,"Persistent launch changed before close");
                // close_proven_targets accepts the inventory once before any close and again
                // just before each close. Failures before a second acceptance are refusals,
                // not a pending exit, and earn no retry.
                let accepted = std::sync::atomic::AtomicU32::new(0);
                let closed = fresh_retirement::close_proven_targets(&browser,&owner.process_identity,deadline,&|rows| {
                    require_inventory(&owner.startup,survivor_id,rows)?;
                    accepted.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                }).await;
                let pending = || ExitPending::new(&owner.process_identity,&observed);
                if let Err(error) = closed {
                    return Err(if accepted.into_inner()>=2 {error.context(pending())} else {error});
                }
                #[cfg(test)]
                if TEST_SLOW_EXIT.replace(false) {
                    return Err(anyhow::anyhow!("PUBLIC injected slow natural exit").context(pending()));
                }
                fresh_retirement::wait_native_exit(&owner.process_identity,deadline).await.context(pending())?;
                Ok::<_,anyhow::Error>(())
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
        Liveness::Unknown => bail!("Persistent Chrome native identity is unknown; retaining it"),
    }
    let release = |admission: DirectoryLock, bringup: DirectoryLock, exclusion: DirectoryLock| {
        // The exit waits may have used the whole budget; releasing needs its own.
        let deadline = Instant::now() + Duration::from_secs(1);
        admission.release_until(deadline)?;
        bringup.release_until(deadline)?;
        exclusion.release_until(deadline)
    };
    if observed.is_empty() {
        // Chrome exited before any attempt saved its helpers (closed by the user or crashed).
        // Nothing remains to close; record that without claiming a natural exit.
        ensure!(
            profile_processes(&owner.profile).is_ok_and(|owners| owners.is_empty()),
            "Persistent native descendant exit evidence is unavailable"
        );
        profile.check()?;
        require_session(store, session, &owner, receipt, true)?;
        release(admission, bringup, exclusion)?;
        return Ok(Some(
            json!({"version":1,"disposition":"exited-unverified","profileRetained":true,"naturalExit":false}),
        ));
    }
    loop {
        if observed
            .iter()
            .all(|process| process.liveness() == Liveness::Dead)
            && profile_processes(&owner.profile).is_ok_and(|owners| owners.is_empty())
        {
            break;
        }
        let left =
            remaining(deadline).context(ExitPending::new(&owner.process_identity, &observed))?;
        tokio::time::sleep(left.min(Duration::from_millis(25))).await;
    }
    profile.check()?;
    require_session(store, session, &owner, receipt, true)?;
    release(admission, bringup, exclusion)?;
    let mut disposal = json!({"version":1,"disposition":"exited","profileRetained":true,"naturalExit":true,"observedProcesses":observed});
    if let Some(target) = owner.startup.target {
        disposal["startupTarget"] = json!(target);
    }
    Ok(Some(disposal))
}

#[cfg(test)]
async fn saved_fixture(connection: &ChromeConnection, store: &SessionStore) -> Result<String> {
    let created=store.create(json!({"prompt":"PUBLIC persistent cleanup fixture","model":"gpt-6-pro","engine":"browser"}),
        connection.profile.as_ref().unwrap().path(),None)?;
    let id = created["id"].as_str().unwrap().to_owned();
    let controller = ProcessIdentity::current()?;
    let owner = connection
        .idle_disposal_owner(&id, &controller)?
        .context("No idle launch capability")?;
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    let creation = crate::browser_disposition::TargetCreation::allocated("PUBLIC-idle-retired");
    let creator=journal.record_creation(&creation,&json!({"host":connection.endpoint.host,"port":connection.endpoint.port,
        "browserWSEndpoint":connection.endpoint.browser_ws_endpoint,"targetId":"PUBLIC-idle-retired","claimId":uuid::Uuid::new_v4().to_string()}),
        &controller,Some(&id),Some(json!({"id":uuid::Uuid::new_v4().to_string(),"profileDir":connection.profile.as_ref().unwrap().path()})))?;
    fresh_retirement::prepare_disposal_fixture(
        store,
        id.clone(),
        controller,
        Value::Null,
        creator.clone(),
    );
    journal
        .retire_finalised(&creator, |_| async {
            Ok((
                "confirmed-absent".into(),
                "PUBLIC fixture retired target".into(),
            ))
        })
        .await?;
    store.update(&id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],
        &json!({"persistentBrowserOwner":owner,"targetRetiredAt":"PUBLIC retired"}))}))}))))?;
    let original = store.stored_for_update(&id)?;
    let parsed: IdleOwner = serde_json::from_value(owner.clone())?;
    require_session(store, &id, &parsed, &owner, true)?;
    for changed in ["dispatch", "output", "pending", "keep", "recovery", "owner"] {
        let mut replacement = original.clone();
        match changed {
            "dispatch" => {
                replacement["dispatch"]["reservationId"] = json!(uuid::Uuid::new_v4().to_string())
            }
            "output" => {
                replacement["browser"]["capturedAnswer"]["sha256"] =
                    json!(sha256(b"PUBLIC changed"))
            }
            "pending" => {
                replacement["browser"]["runtime"]["targetDispositionPending"] = json!(true)
            }
            "keep" => replacement["browser"]["config"]["keepBrowser"] = json!(true),
            "recovery" => replacement["recoveryRequired"] = json!(true),
            _ => {
                replacement["browser"]["runtime"]["persistentBrowserOwner"]["sessionId"] =
                    json!("PUBLIC-foreign")
            }
        }
        store.update(&id, |_| Ok(Some(replacement.clone())))?;
        ensure!(
            require_session(store, &id, &parsed, &owner, true).is_err(),
            "Saved idle accepted changed {changed}"
        );
        store.update(&id, |_| Ok(Some(original.clone())))?;
    }
    Ok(id)
}

/// Native component coverage starts with durable completion, then abandons the
/// original CDP connection. Production retirement must reconnect by its receipt.
#[cfg(test)]
pub(super) async fn check_native_saved(
    connection: &mut ChromeConnection,
    store: &SessionStore,
    case: &str,
    mut borrower: Option<ChromeConnection>,
) -> Result<()> {
    let timeout = Duration::from_secs(8);
    let cancel = CancellationToken::new();
    let identity = connection.identity.clone().context("No fixture process")?;
    if case == "saved-late-startup" {
        connection.idle_startup.as_mut().unwrap().target = None;
    }
    let id = saved_fixture(connection, store).await?;
    let original = store.stored_for_update(&id)?;
    if case == "saved-survivor" {
        let replacement = connection
            .browser
            .call(
                "Target.createTarget",
                json!({"url":"about:blank"}),
                timeout,
                &cancel,
            )
            .await?;
        let survivor_id = replacement["targetId"]
            .as_str()
            .context("No fixture survivor")?;
        let journal = crate::browser_disposition::TargetJournal::open(store)?;
        let creator = &original["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        let record = journal.read(&creator["allocationId"])?;
        journal.retirement_id(creator, &record["commit"])?;
        let record = journal.read(&creator["allocationId"])?;
        record_idle_survivor(store, &record, survivor_id, Instant::now() + timeout)?;
        let startup = connection.idle_startup.as_ref().unwrap();
        connection
            .browser
            .close_owned_target(startup.target.as_deref().unwrap(), timeout, &cancel)
            .await?;
    }
    if case == "saved-changed-launch" {
        let path = connection
            .profile
            .as_ref()
            .unwrap()
            .path()
            .join("oracle-chrome-native.json");
        let original_record = read_record(&path)?.unwrap();
        let mut changed = original_record.clone();
        changed["PUBLICChanged"] = json!(true);
        atomic_json(&path, &changed)?;
        ensure!(
            retire_saved_idle(store, &id, Duration::from_secs(1))
                .await
                .is_err(),
            "Changed native launch was accepted"
        );
        ensure!(
            identity.liveness() == Liveness::Alive,
            "Changed launch refusal closed Chrome"
        );
        atomic_json(&path, &original_record)?;
    }
    let mut borrowed = None;
    if case == "saved-borrowed-ack" {
        // A borrowed remote run's answer tab holds the launcher's idle browser open
        // until acknowledgement retires it; the profile hint then retries the launcher.
        connection.record_idle_owner(&id);
        let answer = connection
            .browser
            .call(
                "Target.createTarget",
                json!({"url":"about:blank"}),
                timeout,
                &cancel,
            )
            .await?;
        let answer = answer["targetId"]
            .as_str()
            .context("No fixture answer tab")?
            .to_owned();
        crate::browser_runner::settle_saved_idle(store, &id).await;
        ensure!(
            identity.liveness() == Liveness::Alive
                && store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"]
                    ["disposition"]
                    == "retained",
            "Borrowed answer tab did not retain the idle browser"
        );
        let profile = connection.profile.as_ref().unwrap().path().to_owned();
        let created = store.create(
            json!({"prompt":"PUBLIC borrowed remote run","model":"gpt-6-pro","engine":"browser"}),
            &profile,
            None,
        )?;
        let created = created["id"].as_str().unwrap().to_owned();
        store.update(&created, |current| {
            Ok(Some(
                json!({"browser":merge_object(&current["browser"],&json!({"runtime":
                {"ownedRecoveryTarget":{"creator":{"lease":{"profileDir":profile}}}}}))}),
            ))
        })?;
        connection
            .browser
            .close_owned_target(&answer, timeout, &cancel)
            .await?;
        let deadline = Instant::now() + timeout;
        while connection
            .browser
            .call("Target.getTargets", json!({}), timeout, &cancel)
            .await?["targetInfos"]
            .as_array()
            .context("No fixture target inventory")?
            .iter()
            .any(|row| row["targetId"] == answer)
        {
            ensure!(
                Instant::now() < deadline,
                "Retired fixture answer tab stayed in the inventory"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        borrowed = Some(created);
    }
    let mut failed_borrower = None;
    if case == "saved-borrowed-failed" {
        // A borrowed run fails after its own tab is gone. Its marker refused the launcher
        // while it ran; its post-run cleanup must still retry the launcher.
        connection.record_idle_owner(&id);
        let borrower = borrower.take().context("No borrowed fixture connection")?;
        ensure!(
            borrower.borrowed() && borrower.idle_owner_hint().as_deref() == Some(id.as_str()),
            "Fixture borrower did not reuse the hinted launch"
        );
        crate::browser_runner::settle_saved_idle(store, &id).await;
        ensure!(
            identity.liveness() == Liveness::Alive
                && store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"]
                    ["disposition"]
                    == "retained",
            "Active borrowed run did not retain the idle browser"
        );
        let created = store.create(
            json!({"prompt":"PUBLIC failed borrowed run","model":"gpt-6-pro","engine":"browser"}),
            connection.profile.as_ref().unwrap().path(),
            None,
        )?;
        failed_borrower = Some((created["id"].as_str().unwrap().to_owned(), borrower));
    }
    let mut remote_ack = None;
    if case == "saved-remote-ack" {
        // A borrowed remote run leaves its owned answer tab, still holding its lease, as the
        // launcher's only other page until the client acknowledges the saved result.
        connection.record_idle_owner(&id);
        let mut borrower = borrower.take().context("No borrowed fixture connection")?;
        ensure!(
            borrower.borrowed() && borrower.idle_owner_hint().as_deref() == Some(id.as_str()),
            "Fixture borrower did not reuse the hinted launch"
        );
        let profile = connection.profile.clone().context("No fixture profile")?;
        let run = uuid::Uuid::new_v4().to_string();
        let remote = crate::remote_result::session_id(&run)?;
        store.create(
            json!({"prompt":"PUBLIC source","model":"gpt-6-pro","engine":"browser"}),
            profile.path(),
            Some(&remote),
        )?;
        let lease = Arc::new(
            profile
                .acquire_with_limit(&remote, 2, timeout, &cancel)
                .await?,
        );
        let target = crate::browser_target::TargetAcquisition::default()
            .acquire(borrower.browser.clone(), lease, &cancel, timeout)
            .await?;
        let creation = target
            .creation()
            .context("Missing fixture answer allocation")?;
        target.lease().update(&json!({"targetDispositionAllocationId":creation.allocation_id(),
            "chromeHost":borrower.endpoint.host,"chromePort":borrower.endpoint.port,"chromeTargetId":target.id()}))?;
        let journal = crate::browser_disposition::TargetJournal::open(store)?;
        let creator = journal.record_creation(creation,
            &json!({"host":borrower.endpoint.host,"port":borrower.endpoint.port,"browserWSEndpoint":borrower.endpoint.browser_ws_endpoint,
                "targetId":target.id(),"claimId":uuid::Uuid::new_v4().to_string()}),
            &ProcessIdentity::current()?, Some(&remote),
            Some(json!({"id":target.lease().id(),"profileDir":crate::native_fs::canonical_wire_path(target.lease().profile().path())?})))?;
        fresh_retirement::mount_completed_page(
            &borrower.browser,
            target.page(),
            &creator["target"]["claimId"],
        )
        .await?;
        // A run leaves its answer tab in front of the launcher's startup page.
        borrower
            .browser
            .call(
                "Target.activateTarget",
                json!({"targetId":target.id()}),
                timeout,
                &cancel,
            )
            .await?;
        let record = crate::remote_result::native_ack_fixture(store, &run, &creator)?;
        // The run ends: its marker is released, and its launcher retry refuses the lease the
        // answer tab holds until acknowledgement.
        crate::browser_runner::settle_borrowed_run(store, &remote, &mut borrower, true, true).await;
        ensure!(
            identity.liveness() == Liveness::Alive
                && store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"]
                    ["disposition"]
                    == "retained",
            "Unacknowledged answer tab did not retain the idle browser"
        );
        borrower
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await?;
        remote_ack = Some((remote, record, creator, target));
    }
    connection
        .browser
        .disconnect_and_wait(Duration::from_secs(1))
        .await?;
    if let Some((remote, record, creator, target)) = remote_ack {
        // The host replies after the commit; retirement and the launcher retry follow.
        let acknowledgement = crate::remote_result::commit_acknowledgement(store, &record)?;
        let journal = crate::browser_disposition::TargetJournal::open(store)?;
        let committed = journal.read(&creator["allocationId"])?;
        ensure!(
            committed["commit"].is_object()
                && committed["finalisedAt"].is_string()
                && committed["disposition"]["decision"] == "pending"
                && identity.liveness() == Liveness::Alive,
            "Remote ACK commit closed or retired before its reply: {}",
            committed["disposition"]
        );
        let completed = store.stored_for_update(&remote)?;
        let warning = acknowledgement.finish(store, &record).await?;
        ensure!(
            warning.is_none(),
            "Remote ACK cleanup warned: {warning:?}; journal {}",
            journal.read(&creator["allocationId"])?["disposition"]
        );
        let retired = journal.read(&creator["allocationId"])?;
        ensure!(
            retired["disposition"]["decision"] == "retired"
                && retired["disposition"]["absence"] == "confirmed-absent"
                && retired["commit"] == committed["commit"],
            "Remote answer tab retirement differs: {}",
            retired["disposition"]
        );
        let disposal =
            &store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"];
        ensure!(
            identity.liveness() == Liveness::Dead
                && disposal["disposition"] == "exited"
                && disposal["naturalExit"] == true,
            "Acknowledged remote run did not retry the idle launcher: {disposal}"
        );
        let settled = store.stored_for_update(&remote)?;
        ensure!(
            settled["browser"]["runtime"]["targetDispositionPending"] == false
                && settled["browser"]["runtime"]["targetRetiredAt"].is_string()
                && settled["recoveryRequired"] == false
                && target.lease().profile().records()?["leases"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
            "Remote ACK retirement was not settled or its lease released"
        );
        for key in ["status", "completedAt", "dispatch"] {
            ensure!(
                settled[key] == completed[key],
                "Remote ACK cleanup changed {key}"
            );
        }
        ensure!(
            settled["browser"]["capturedAnswer"] == completed["browser"]["capturedAnswer"]
                && settled["browser"]["remoteDelivery"] == completed["browser"]["remoteDelivery"],
            "Remote ACK cleanup changed the delivered capture"
        );
        drop(target);
    }
    if let Some((created, mut borrower)) = failed_borrower {
        crate::browser_runner::settle_borrowed_run(store, &created, &mut borrower, true, false)
            .await;
        let _ = borrower
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await;
        ensure!(
            identity.liveness() == Liveness::Dead,
            "Failed borrowed run did not retry the idle launcher: {}",
            store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"]
        );
    }
    if let Some(borrowed) = borrowed {
        crate::browser_runner::settle_deferred_fresh(store, Some(&borrowed)).await;
        ensure!(
            identity.liveness() == Liveness::Dead,
            "Acknowledged borrowed run did not retry the idle launcher: {}",
            store.stored_for_update(&id)?["browser"]["runtime"]["persistentBrowserDisposal"]
        );
    }
    if case == "saved-after-exit" {
        // First cleanup saves qualified helper identities before Chrome exits;
        // the next call must verify them without a live CDP endpoint.
        let outcome = retire_saved_idle(store, &id, timeout)
            .await?
            .context("No saved idle result")?;
        store.update(&id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentBrowserDisposal":outcome}))}))}))))?;
    }
    if case == "saved-slow-exit" {
        // The first attempt closes the startup page, then ends before exit is confirmed.
        // With no later run, only the bounded same-process retry can record the exit.
        TEST_SLOW_EXIT.set(true);
    }
    crate::browser_runner::settle_saved_idle(store, &id).await;
    ensure!(
        !TEST_SLOW_EXIT.replace(false),
        "Slow natural exit was not injected"
    );
    let settled = store.stored_for_update(&id)?;
    ensure!(
        settled["browser"]["runtime"]["persistentBrowserDisposal"]["disposition"] == "exited"
            && settled["browser"]["runtime"]["persistentBrowserDisposal"]["profileRetained"]
                == true
            && identity.liveness() == Liveness::Dead,
        "Saved persistent launch did not exit: {}",
        settled["browser"]["runtime"]["persistentBrowserDisposal"]
    );
    for key in ["status", "dispatch", "options", "capturedAnswer"] {
        ensure!(
            settled[key] == original[key],
            "Saved idle cleanup changed {key}"
        );
    }
    ensure!(
        settled["browser"]["capturedAnswer"] == original["browser"]["capturedAnswer"],
        "Saved idle cleanup changed provider capture"
    );
    if let Some(mut child) = connection.child.take() {
        tokio::task::spawn_blocking(move || child.wait()).await??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grace waits on every recorded process, ends early once all are dead and
    /// never outlasts its bound; only an ExitPending error carries one.
    #[tokio::test]
    async fn exit_grace_waits_for_live_processes_within_its_bound() {
        let live = ProcessIdentity::current().unwrap();
        let pending = ExitPending::new(&live, std::slice::from_ref(&live));
        let started = Instant::now();
        pending.wait(Duration::from_millis(150)).await;
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(150) && waited < Duration::from_secs(2));
        let started = Instant::now();
        ExitPending(vec![]).wait(Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = anyhow::anyhow!("PUBLIC inner").context(pending);
        assert_eq!(ExitPending::of(&error).map(|found| found.0.len()), Some(2));
        assert!(format!("{error:#}").contains("PUBLIC inner"));
        assert!(ExitPending::of(&anyhow::anyhow!("PUBLIC refusal")).is_none());
    }
}
