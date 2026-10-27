//! Reconcile durable target creators without allocating a consultation or adopting a lease.
use crate::{
    artifacts::sha256,
    browser,
    browser_chrome::AttachmentEndpoint,
    browser_disposition::TargetJournal,
    browser_profile::ProfileRegistry,
    browser_runner::result_expression,
    cdp::Cdp,
    process::{Liveness, ProcessIdentity},
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn remaining(deadline: Instant) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    ensure!(!duration.is_zero(), "Target disposition deadline reached");
    Ok(duration)
}

pub(crate) async fn reconcile(
    store: &SessionStore,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
) -> Result<()> {
    reconcile_with_disposal(store, browser, endpoint, false).await
}
/// Called after a successful run has released admission and will use no more targets.
pub(crate) async fn reconcile_after_run(
    store: &SessionStore,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
) -> Result<()> {
    reconcile_with_disposal(store, browser, endpoint, true).await
}
async fn reconcile_with_disposal(
    store: &SessionStore,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    dispose_current_endpoint: bool,
) -> Result<()> {
    let journal = TargetJournal::open(store)?;
    let cancel = CancellationToken::new();
    let disposed = journal
        .reconcile(
            |record, deadline| {
                let own_completed = journal.is_current_completed_owner(&record);
                Box::pin(retire(
                    store,
                    &journal,
                    browser,
                    endpoint,
                    record,
                    deadline,
                    own_completed,
                    None,
                    &cancel,
                ))
            },
            |creator, disposed, deadline| {
                if !creator["lease"].is_object()
                    || disposed["decision"] != "retired"
                    || disposed["absence"] != "confirmed-absent"
                {
                    return Ok(());
                }
                let path = creator["lease"]["profileDir"]
                    .as_str()
                    .context("Creator profile is missing")?;
                ProfileRegistry::open_existing(Path::new(path))?.release_disposed(
                    &journal,
                    creator,
                    disposed,
                    deadline,
                    journal.is_current_completed_owner(&journal.read(&creator["allocationId"])?),
                )?;
                settle_unused_authentication(store, &journal, creator, disposed, deadline)?;
                crate::browser_runner::settle_completed_retirement(
                    store, &journal, creator, disposed, deadline,
                )?;
                Ok(())
            },
        )
        .await?;
    for result in disposed {
        let Ok(record) = journal.read(&result["allocationId"]) else {
            continue;
        };
        if let Some(id) = record["creator"]["sessionId"].as_str() {
            let Ok(saved) = store.stored_for_update(id) else {
                continue;
            };
            // This connection is about to request admission. Do not retire its browser
            // underneath that new run; a completed recovery settles the same endpoint.
            let current = |owner: &str| {
                saved["browser"]["runtime"][owner]["endpoint"]["browserWsEndpoint"]
                    == endpoint.browser_ws_endpoint
            };
            if dispose_current_endpoint || !current("freshProfileOwner") {
                crate::browser_runner::settle_saved_fresh(store, id).await;
            }
            // A persistent owner whose first idle attempt missed is retried here too,
            // once and without the exit grace: a run may be waiting on this reconcile.
            if dispose_current_endpoint || !current("persistentBrowserOwner") {
                crate::browser_runner::settle_saved_idle_now(store, id).await;
            }
        }
    }
    Ok(())
}
fn settle_unused_authentication(
    store: &SessionStore,
    journal: &TargetJournal,
    creator: &Value,
    disposed: &Value,
    deadline: Instant,
) -> Result<()> {
    let Some(id) = creator["sessionId"].as_str() else {
        return Ok(());
    };
    let record = journal.read(&creator["allocationId"])?;
    if !record["commit"].is_null() {
        return Ok(());
    }
    ensure!(
        record["creator"] == *creator
            && record["disposition"] == *disposed
            && disposed["decision"] == "retired"
            && disposed["absence"] == "confirmed-absent",
        "Unused authentication target has no exact durable absence receipt"
    );
    let model = store.require(id)?["model"]
        .as_str()
        .context("Unused session has no model")?
        .to_owned();
    store.update_session_and_model(id, &model, |session, run| {
        remaining(deadline)?;
        let runtime = &session["browser"]["runtime"];
        if runtime["preNavigationCleanupPending"] != true {return Ok(None)}
        let undispatched = if let Some(dispatch) = session.get("dispatch") {
            let request = crate::browser_runner::request(store, id)?;
            *dispatch == json!({"state":"not-started","reservationId":request.reservation_id})
                && Uuid::parse_str(&request.reservation_id).is_ok()
        } else {true};
        ensure!(matches!(session["status"].as_str(),Some("running"|"error"|"cancelled"))
            && undispatched
            && runtime["userCommitReceipt"].is_null()
            && runtime["submittedPromptHash"].is_null()
            && runtime["ownedRecoveryTarget"]["creator"] == *creator
            && runtime["chromeTargetId"] == creator["target"]["targetId"]
            && runtime["chromePort"] == creator["target"]["port"]
            && runtime["browserWSEndpoint"] == creator["target"]["browserWSEndpoint"]
            && runtime["targetClaimId"] == creator["target"]["claimId"]
            && runtime["tabLeaseId"] == creator["lease"]["id"]
            && runtime["chromeHost"].as_str().zip(creator["target"]["host"].as_str())
                .is_some_and(|(actual,expected)| browser::normalise_host(actual)==browser::normalise_host(expected)),
            "Unused authentication session changed; retaining recovery");
        require_session_controller(session, None)?;
        let mut browser = session["browser"].clone();
        browser["runtime"]["preNavigationCleanupPending"] = json!(false);
        browser["runtime"]["targetDispositionPending"] = json!(false);
        browser["runtime"]["targetRetiredAt"] = json!(crate::sessions::timestamp());
        let mut changes = json!({"browser":browser,"recoveryRequired":false});
        let model_patch = if session["status"] == "running" {
            changes["status"] = json!("error");
            changes["errorMessage"] = json!("Browser worker stopped before dispatch; unused target absence confirmed");
            changes["completedAt"] = json!(crate::sessions::timestamp());
            run.is_none_or(|run| !crate::sessions::terminal(run["status"].as_str().unwrap_or("")))
                .then(|| json!({"status":"error","errorMessage":changes["errorMessage"],"completedAt":changes["completedAt"]}))
        } else {None};
        Ok(Some((changes, model_patch)))
    })?;
    Ok(())
}

pub(crate) async fn retire_committed(
    store: &SessionStore,
    journal: &TargetJournal,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    creator: &Value,
) -> Result<Value> {
    retire_committed_guarded(
        store,
        journal,
        browser,
        endpoint,
        creator,
        None,
        &CancellationToken::new(),
    )
    .await
}
pub(crate) async fn retire_committed_guarded(
    store: &SessionStore,
    journal: &TargetJournal,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    creator: &Value,
    check: Option<&(dyn Fn() -> Result<()> + Send + Sync)>,
    cancel: &CancellationToken,
) -> Result<Value> {
    if let Some(check) = check {
        check()?;
    }
    let record = journal.read(&creator["allocationId"])?;
    ensure!(
        record["creator"] == *creator,
        "Target creator changed before remote retirement"
    );
    journal.verify(&record)?;
    let current = ProcessIdentity::current()?;
    let allow_current = creator["controllerPid"] == current.pid
        && creator["controllerStartedAtMs"].as_f64() == Some(current.started_at_ms)
        && record.get("nativeOwnerIdentity").is_none_or(|identity| {
            serde_json::from_value::<ProcessIdentity>(identity.clone())
                .is_ok_and(|identity| identity == current)
        });
    let deadline = Instant::now() + Duration::from_secs(5);
    let disposed = journal
        .retire_finalised(creator, |commit| {
            let commit = commit.clone();
            async move {
                // retire_finalised persists its commit-bound attempt before
                // invoking us. Use that locked snapshot, including the attempt.
                let record = journal.read(&creator["allocationId"])?;
                ensure!(
                    record["creator"] == *creator && record["commit"] == commit,
                    "Target commitment changed before remote retirement"
                );
                Box::pin(retire(
                    store,
                    journal,
                    browser,
                    endpoint,
                    record,
                    deadline,
                    allow_current,
                    check,
                    cancel,
                ))
                .await
            }
        })
        .await?;
    if disposed["decision"] == "retired"
        && disposed["absence"] == "confirmed-absent"
        && let Some(path) = creator["lease"]["profileDir"].as_str()
    {
        ProfileRegistry::open_existing(Path::new(path))?.release_disposed(
            journal,
            creator,
            &disposed,
            Instant::now() + Duration::from_secs(5),
            true,
        )?;
    }
    Ok(disposed)
}

pub(crate) fn claimed_expression(action: &str) -> String {
    // Same visible stop controls as browser-scripts/result-prelude.js.
    let stop = r#"[data-testid="stop-button"], [data-testid="composer-stop-button"], form button[aria-label*="stop" i]:not([aria-label*="dictat" i]):not([aria-label*="voice" i]):not([aria-label*="read" i])"#;
    format!(
        r#"(cfg)=>{{ /* alcuin-dom:claimed-{action} */ {ownership}
        try {{
            const claim=readOracleTargetClaim();
            if(cfg.action==='rollback') {{if(claim?.id===cfg.claimId&&claim.retirementId===cfg.retirementId){{claim.retiring=false;delete claim.retirementId;writeOracleTargetClaim(claim);}}return true;}}
            const url=new URL(location.href);
            const generating=Array.from(document.querySelectorAll({stop})).some(node=>{{const r=node.getBoundingClientRect();return r.width>0&&r.height>0;}});
            if(window!==window.top||!claim||claim.id!==cfg.claimId||claim.submission||claim.inspection||generating||url.origin!=='https://chatgpt.com'||url.pathname.match(/(?:^|\/)c\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1]!==cfg.conversationId)return false;
            if(cfg.action==='inspect')return claim.retiring===true&&claim.retirementId===cfg.retirementId;
            if(claim.retiring)return claim.retirementId===cfg.retirementId;
            claim.retiring=true;claim.retirementId=cfg.retirementId;writeOracleTargetClaim(claim);return true;
        }}catch{{return false;}}
    }}"#,
        ownership = include_str!("../browser-scripts/ownership-prelude.js"),
        stop = json!(stop)
    )
}
pub(crate) fn authorise_claimed(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    capture: &crate::browser_historical::HistoricalTarget,
    deadline: Instant,
) -> Result<()> {
    let current = store.stored_for_update(id)?;
    ensure!(
        current == *saved
            && current["status"] == "completed"
            && current["browser"]["runtime"]["ownedRecoveryTarget"]["creator"].is_null()
            && capture.matches(&current),
        "Creatorless completed target authority changed"
    );
    ensure!(
        store
            .model_runs(id)?
            .iter()
            .any(|run| run["model"] == current["model"] && run["status"] == "completed"),
        "Creatorless completed model evidence changed"
    );
    let receipt: crate::artifacts::CaptureReceipt =
        serde_json::from_value(current["browser"]["capturedAnswer"].clone())?;
    ensure!(
        receipt.session_id == id,
        "Creatorless answer belongs to another session"
    );
    let answer = crate::artifacts::verify_capture(store, &receipt)?;
    crate::browser_historical::verify_completed_outputs(
        &current,
        &answer,
        &CancellationToken::new(),
    )?;
    require_target_controllers(
        store,
        Some(id),
        &serde_json::to_value(capture)?,
        None,
        deadline,
        false,
    )
}
pub(crate) async fn retire_claimed_completed(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    capture: &crate::browser_historical::HistoricalTarget,
    browser: &Cdp,
) -> Result<Value> {
    let claim = capture
        .claim_id
        .as_deref()
        .context("Creatorless target claim is unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let operation_deadline = deadline - Duration::from_millis(100);
    authorise_claimed(store, id, saved, capture, operation_deadline)?;
    let binding = json!({"target":capture,"capture":saved["browser"]["capturedAnswer"],"completedAt":saved["completedAt"]});
    let saved = store.update_until(id, operation_deadline, |current| {
        ensure!(current == saved, "Creatorless completion changed before retirement intent");
        let previous = &current["browser"]["runtime"]["creatorlessRetirement"];
        if !previous.is_null() {
            ensure!(previous["version"] == 1 && previous["binding"] == binding
                && previous["id"].as_str().is_some_and(|id| Uuid::parse_str(id).is_ok()),
                "Creatorless retirement intent no longer matches its completed target");
            return Ok(None);
        }
        Ok(Some(json!({"browser":crate::sessions::merge_object(&current["browser"], &json!({"runtime":crate::sessions::merge_object(&current["browser"]["runtime"],
            &json!({"creatorlessRetirement":{"version":1,"id":Uuid::new_v4().to_string(),"binding":binding}}))}))})))
    })?;
    let saved = &saved;
    let rows = targets(browser, operation_deadline).await?;
    if rows.iter().all(|row| row["targetId"] != capture.target_id) {
        authorise_claimed(store, id, saved, capture, operation_deadline)?;
        return Ok(
            json!({"decision":"retired","absence":"confirmed-absent","reason":"already-absent",
                "retirement":saved["browser"]["runtime"]["creatorlessRetirement"]}),
        );
    }
    let page = browser
        .attach_target(
            &capture.target_id,
            remaining(operation_deadline)?,
            &CancellationToken::new(),
        )
        .await?;
    if !rows
        .iter()
        .any(|row| row["type"] == "page" && row["targetId"] != capture.target_id)
    {
        let replacement = browser
            .call(
                "Target.createTarget",
                json!({"url":"about:blank"}),
                remaining(operation_deadline)?,
                &CancellationToken::new(),
            )
            .await?;
        ensure!(
            replacement["targetId"]
                .as_str()
                .is_some_and(crate::browser_target::valid_id)
                && replacement["targetId"] != capture.target_id,
            "Unable to preserve the browser's final page"
        );
    }
    authorise_claimed(store, id, saved, capture, operation_deadline)?;
    let marker = saved["browser"]["runtime"]["creatorlessRetirement"]["id"]
        .as_str()
        .context("Creatorless retirement identity is missing")?;
    let arguments = |action: &str| json!([{"action":action,"claimId":claim,"conversationId":capture.conversation_id,"retirementId":marker}]);
    let checked = async {
        let locked = page
            .evaluate_with_arguments(
                &claimed_expression("mark"),
                &arguments("mark"),
                remaining(operation_deadline)?,
                &CancellationToken::new(),
            )
            .await?;
        ensure!(
            locked == true,
            "Creatorless exact retirement marker was not confirmed"
        );
        authorise_claimed(store, id, saved, capture, operation_deadline)?;
        ensure!(
            page.evaluate_with_arguments(
                &claimed_expression("inspect"),
                &arguments("inspect"),
                remaining(operation_deadline)?,
                &CancellationToken::new()
            )
            .await?
                == true,
            "Creatorless retirement marker or conversation changed"
        );
        authorise_claimed(store, id, saved, capture, operation_deadline)?;
        remaining(operation_deadline)
    }
    .await;
    let close_timeout = match checked {
        Ok(timeout) => timeout,
        Err(error) => {
            if let Ok(timeout) = remaining(deadline) {
                let _ = page
                    .evaluate_with_arguments(
                        &claimed_expression("rollback"),
                        &arguments("rollback"),
                        timeout,
                        &CancellationToken::new(),
                    )
                    .await;
            }
            return Err(error);
        }
    };
    let _ = browser
        .close_owned_target(&capture.target_id, close_timeout, &CancellationToken::new())
        .await;
    loop {
        if targets(browser, operation_deadline)
            .await?
            .iter()
            .all(|row| row["targetId"] != capture.target_id)
        {
            return Ok(
                json!({"decision":"retired","absence":"confirmed-absent","reason":"exact-target-absent",
                    "retirement":saved["browser"]["runtime"]["creatorlessRetirement"]}),
            );
        }
        tokio::time::sleep(Duration::from_millis(25).min(remaining(operation_deadline)?)).await;
    }
}

pub(crate) fn require_controllers(
    store: &SessionStore,
    creator: &Value,
    deadline: Instant,
    allow_current_owner: bool,
) -> Result<()> {
    require_target_controllers(
        store,
        creator["sessionId"].as_str(),
        &creator["target"],
        Some(creator),
        deadline,
        allow_current_owner,
    )
}
pub(crate) fn require_target_controllers(
    store: &SessionStore,
    current_session: Option<&str>,
    target: &Value,
    creator: Option<&Value>,
    deadline: Instant,
    allow_current_owner: bool,
) -> Result<()> {
    remaining(deadline)?;
    let mut profiles = std::collections::BTreeSet::new();
    if let Some(id) = current_session {
        let session = store.require(id)?;
        require_session_controller(&session, creator.filter(|_| allow_current_owner))?;
        for value in [
            &session["browser"]["runtime"]["userDataDir"],
            &session["browser"]["runtime"]["chromeProfileRoot"],
            &session["browser"]["runtime"]["profileDir"],
            &session["browser"]["config"]["manualLoginProfileDir"],
        ] {
            if let Some(path) = value.as_str().filter(|path| !path.is_empty()) {
                profiles.insert(path.to_owned());
            }
        }
    }
    for session in store.list(true)? {
        remaining(deadline)?;
        if session["id"].as_str() == current_session {
            continue;
        }
        let runtime = &session["browser"]["runtime"];
        if session_target(runtime, target) {
            require_session_controller(&session, None)?;
        }
    }
    if let Some(path) = creator.and_then(|creator| creator["lease"]["profileDir"].as_str()) {
        profiles.insert(path.to_owned());
    }
    for path in profiles {
        remaining(deadline)?;
        let profile = ProfileRegistry::open_existing(Path::new(&path))?;
        let registry = profile.records_until(deadline)?;
        for lease in registry["leases"].as_array().into_iter().flatten() {
            if !lease_target(lease, target) {
                continue;
            }
            let own = creator.is_some_and(|creator| {
                lease["id"] == creator["lease"]["id"]
                    && lease["pid"] == creator["controllerPid"]
                    && lease["processStartedAtMs"] == creator["controllerStartedAtMs"]
                    && lease["sessionId"] == *crate::browser_disposition::creator_admission(creator)
                    && lease["targetDispositionAllocationId"] == creator["allocationId"]
            });
            if own {
                continue;
            }
            let dead = if let Some(native) = lease.get("processIdentity") {
                serde_json::from_value::<ProcessIdentity>(native.clone())
                    .is_ok_and(|identity| identity.liveness() == Liveness::Dead)
            } else {
                lease["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .is_some_and(|pid| {
                        ProcessIdentity::read(pid).is_ok_and(|identity| identity.is_none())
                    })
            };
            ensure!(
                dead,
                "Another live or unknown browser lease owns this target"
            );
        }
    }
    remaining(deadline)?;
    Ok(())
}
fn require_session_controller(session: &Value, creator: Option<&Value>) -> Result<()> {
    for (pid, started) in [
        (
            session["lifecycle"]["workerPid"].as_u64(),
            &session["lifecycle"]["workerProcessStartedAtMs"],
        ),
        (
            session["browser"]["runtime"]["controllerPid"].as_u64(),
            &session["browser"]["runtime"]["controllerProcessStartedAtMs"],
        ),
    ]
    .into_iter()
    .filter_map(|(pid, started)| pid.map(|pid| (pid, started)))
    {
        if let Some(creator) = creator {
            let current = ProcessIdentity::current()?;
            if creator["controllerPid"] == current.pid
                && creator["controllerStartedAtMs"].as_f64() == Some(current.started_at_ms)
                && pid == u64::from(current.pid)
                && started.as_f64() == Some(current.started_at_ms)
            {
                continue;
            }
        }
        ensure!(
            ProcessIdentity::read(u32::try_from(pid)?).is_ok_and(|identity| {
                identity.is_none_or(|actual| {
                    creator.is_none()
                        && started
                            .as_f64()
                            .filter(|start| start.is_finite() && *start > 0.0)
                            .is_some_and(|start| start != actual.started_at_ms)
                })
            }),
            "Another live or unknown session controller owns this target"
        );
    }
    Ok(())
}
fn session_target(record: &Value, target: &Value) -> bool {
    record["chromeTargetId"] == target["targetId"]
        && record["chromePort"] == target["port"]
        && target["host"].as_str().is_some_and(|host| {
            browser::normalise_host(record["chromeHost"].as_str().unwrap_or("127.0.0.1"))
                == browser::normalise_host(host)
        })
}
fn lease_target(record: &Value, target: &Value) -> bool {
    record["chromeTargetId"] == target["targetId"]
        && (record["chromePort"].is_null()
            || record["chromePort"] == 0
            || record["chromePort"] == target["port"])
        && record["chromeHost"].as_str().is_none_or(|host| {
            host.is_empty()
                || target["host"].as_str().is_some_and(|expected| {
                    browser::normalise_host(host) == browser::normalise_host(expected)
                })
        })
}
async fn targets(browser: &Cdp, deadline: Instant) -> Result<Vec<Value>> {
    let raw = browser
        .call(
            "Target.getTargets",
            json!({}),
            remaining(deadline)?,
            &CancellationToken::new(),
        )
        .await?;
    let rows = raw["targetInfos"]
        .as_array()
        .context("Target absence evidence is incomplete")?;
    ensure!(
        rows.iter().all(|row| row["targetId"]
            .as_str()
            .is_some_and(crate::browser_target::valid_id)),
        "Target absence evidence contains malformed identities"
    );
    Ok(rows.clone())
}
pub(crate) fn research_retirement_config(
    record: &Value,
    session: &Value,
    marker: Option<&str>,
) -> Value {
    let native_marked = marker.is_some() && record.get("nativeOwnerIdentity").is_some();
    let owner = if native_marked {
        session["browser"]["runtime"]["submissionOwner"].clone()
    } else {
        json!({"claimId":record["creator"]["target"]["claimId"]})
    };
    json!({"disposition":!native_marked,"owner":owner,
        "receipt":session["browser"]["runtime"]["userCommitReceipt"],"retirementId":marker})
}

async fn observe(
    store: &SessionStore,
    record: &Value,
    page: &Cdp,
    marker: Option<&str>,
    deadline: Instant,
) -> Result<Value> {
    let commit = &record["commit"];
    if commit["kind"] == "grok-session" {
        let session = store.require(
            commit["sessionId"]
                .as_str()
                .context("Committed Grok session is missing")?,
        )?;
        return crate::browser_grok::observe_retirement(
            store,
            &session,
            page,
            marker.context("Grok retirement needs its exact completed marker")?,
            remaining(deadline)?,
        )
        .await;
    }
    if commit["kind"] == "research-report" {
        let session = store.require(
            commit["sessionId"]
                .as_str()
                .context("Committed research session is missing")?,
        )?;
        let cfg = research_retirement_config(record, &session, marker);
        let observed = crate::browser_research::observe_report(
            page,
            &session,
            &cfg,
            remaining(deadline)?,
            &CancellationToken::new(),
        )
        .await?
        .report
        .context("Committed research report is unavailable")?;
        ensure!(
            observed["report"] == commit["report"]
                && observed["stopExists"] == false
                && observed["strongThinkingActive"] == false
                && observed["completionVisible"] == true,
            "Committed research report changed"
        );
        return Ok(observed);
    }
    let native = commit["kind"] == "native";
    let legacy = commit["kind"] == "session" && record.get("nativeOwnerIdentity").is_none();
    ensure!(
        native
            || commit.get("submittedPromptHashFormat").is_none()
            || commit["submittedPromptHashFormat"] == "original-source-v1",
        "Committed prompt fingerprint format is unsupported"
    );
    let user_id = if native {
        commit["userMessageId"].clone()
    } else if legacy {
        Value::Null
    } else {
        store.require(
            commit["sessionId"]
                .as_str()
                .context("Committed session is missing")?,
        )?["browser"]["runtime"]["userCommitReceipt"]["messageId"]
            .clone()
    };
    let owner = if !legacy && !native && marker.is_some() {
        store.require(
            commit["sessionId"]
                .as_str()
                .context("Committed session is missing")?,
        )?["browser"]["runtime"]["submissionOwner"]
            .clone()
    } else {
        json!({"claimId":record["creator"]["target"]["claimId"]})
    };
    let observed = page.evaluate_with_arguments(&result_expression(),
        &json!([{"disposition":legacy || native || marker.is_none(),"native":native,"legacyDisposition":legacy,"originalSourceRequired":!legacy || commit["submittedPromptHashFormat"]=="original-source-v1","owner":owner,
            "receipt":{"messageId":user_id,"conversation":{"origin":"https://chatgpt.com","conversationId":commit["conversationId"]}},
            "retirementId":marker}]), remaining(deadline)?, &CancellationToken::new()).await?;
    ensure!(
        observed["conversationId"] == commit["conversationId"]
            && observed["stopExists"] == false
            && observed["strongThinkingActive"] == false
            && observed["completionVisible"] == true
            && observed["assistantFollowsLatestUser"] == true
            && browser::assistant_identity(&observed)
                .is_some_and(|id| id == &commit["assistantMessageId"]),
        "Committed assistant or conversation changed"
    );
    if native {
        ensure!(
            observed["lastUserMessageId"] == commit["userMessageId"]
                && observed["lastUserText"] == commit["userText"],
            "Native committed user message changed"
        );
    } else {
        let original = commit["submittedPromptHashFormat"] == "original-source-v1";
        let text = if original {
            observed["lastUserOriginalSourceText"].as_str()
        } else {
            observed["lastUserTextRaw"]
                .as_str()
                .or_else(|| observed["lastUserText"].as_str())
        }
        .context("Committed user source is unavailable")?;
        let id = observed[if original {
            "lastUserOriginalSourceMessageId"
        } else {
            "lastUserMessageId"
        }]
        .as_str()
        .context("Committed user identity is unavailable")?;
        ensure!(
            browser::prompt_fingerprint(text, id) == commit["submittedPromptHash"],
            "Committed user source changed"
        );
    }
    Ok(observed)
}
fn copy_expression() -> String {
    let (_, body) = include_str!("../browser-scripts/copy-markdown.js")
        .split_once("return (() => {")
        .expect("Retained copy expression body");
    let body = body
        .replace(
            "const TIMEOUT_MS = 10000;",
            "const TIMEOUT_MS = cfg.timeoutMs;",
        )
        .replace(
            "const conversationChanged = () =>",
            "const conversationChanged = () => !oracleSubmissionIsActive() ||",
        )
        .replace("const deadline = Date.now() + TIMEOUT_MS;", "")
        .replace("}, TIMEOUT_MS);", "}, Math.max(1, deadline - Date.now()));");
    format!(
        "(cfg) => {{ /* alcuin-dom:disposition-copy */ {} const deadline=Date.now()+cfg.timeoutMs; const oracleSubmissionIsActive=()=>{{ try {{const claim=readOracleTargetClaim();return window===window.top && Date.now()<deadline && claim?.id===cfg.claimId && !claim.submission && !claim.inspection && (cfg.retirementId ? claim.retiring===true && claim.retirementId===cfg.retirementId : !claim.retiring);}}catch{{return false;}}}}; return (() => {{{body}",
        include_str!("../browser-scripts/ownership-prelude.js")
    )
}
async fn authorise(
    store: &SessionStore,
    journal: &TargetJournal,
    record: &Value,
    page: &Cdp,
    marker: Option<&str>,
    deadline: Instant,
    allow_current_owner: bool,
) -> Result<()> {
    journal.verify(record)?;
    let observed = observe(store, record, page, marker, deadline).await?;
    let commit = &record["commit"];
    let text = observed["text"]
        .as_str()
        .context("Committed assistant text is unavailable")?;
    let content = if commit["kind"] == "research-report"
        || commit["kind"] == "grok-session"
        || commit["kind"] == "session" && commit["assistantContent"]["representation"] == "text"
    {
        text.to_owned()
    } else {
        let copied = page.evaluate_with_arguments(&copy_expression(), &json!([{
            "claimId":record["creator"]["target"]["claimId"], "retirementId":marker,
            "timeoutMs":remaining(deadline)?.as_millis().saturating_sub(10).max(1),
            "receipt":{"conversation":{"origin":"https://chatgpt.com","conversationId":commit["conversationId"]}},
            "meta":{"messageId":observed["lastAssistantMessageId"],"turnId":observed["lastAssistantTurnId"]}
        }]), remaining(deadline)?, &CancellationToken::new()).await?;
        let after = observe(store, record, page, marker, deadline).await?;
        ensure!(
            after == observed,
            "Source or assistant changed during retirement Markdown capture"
        );
        ensure!(
            copied["status"] != "conversation-changed",
            "Retirement Markdown ownership changed"
        );
        if copied["success"] == true {
            copied["markdown"]
                .as_str()
                .context("Retirement Markdown is unavailable")?
                .to_owned()
        } else {
            crate::policy::trim(text).to_owned()
        }
    };
    let expected = if commit["kind"] == "native" {
        &commit["output"]["sha256"]
    } else if commit["kind"] == "research-report" {
        &commit["reportContent"]["sha256"]
    } else {
        &commit["assistantContent"]["sha256"]
    };
    ensure!(
        sha256(content.as_bytes()) == *expected,
        "Committed assistant content changed"
    );
    require_controllers(store, &record["creator"], deadline, allow_current_owner)?;
    journal.verify(record)
}

pub(crate) fn retirement_mark_expression() -> String {
    retirement_mark(false)
}

/// Only a verified completed native dispatch may quarantine its exact original
/// submission and initialise retirement. Historical claims keep the stricter gate.
pub(crate) fn completed_retirement_mark_expression() -> String {
    retirement_mark(true)
}

fn retirement_mark(completed: bool) -> String {
    let child_marker = if completed { "" } else { "!claim.retiring || " };
    let submission_states = if completed {
        "'active','quarantined','releasing'"
    } else {
        "'quarantined','releasing'"
    };
    let quarantine = if completed {
        "if(claim.submission){claim.submission.state='quarantined';if(claim.submission.dispatch==='started')claim.submission.dispatch='ambiguous';}"
    } else {
        ""
    };
    let expression = format!(
        "(cfg)=>{{ /* alcuin-dom:disposition-retire */ {} const claim=readOracleTargetClaim(); const url=new URL(location.href);if(window!==window.top || !claim || claim.id!==cfg.claimId || (claim.retiring && claim.retirementId!==cfg.retirementId) || (claim.submission && ({child_marker}claim.submission.id!==cfg.owner.reservationId || ![{submission_states}].includes(claim.submission.state))) || (claim.inspection && ({child_marker}claim.inspection.parentReservationId!==cfg.owner.reservationId || !['quarantined','clean'].includes(claim.inspection.state))) || url.origin!=='https://chatgpt.com' || url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1]!==cfg.conversationId)return false;{quarantine}claim.retiring=true;claim.retirementId=cfg.retirementId;writeOracleTargetClaim(claim);return true;}}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    if completed {
        expression.replace(
            "alcuin-dom:disposition-retire",
            "alcuin-dom:completed-disposition-retire",
        )
    } else {
        expression
    }
}

pub(crate) async fn pending_marker(
    page: &Cdp,
    owner: &Value,
    marker: &str,
    deadline: Instant,
) -> Result<Option<String>> {
    let expression = format!(
        "(cfg)=>{{ /* alcuin-dom:pending-retirement */ {} const claim=readOracleTargetClaim();if(window!==window.top||!claim||claim.id!==cfg.owner.claimId)return 'foreign';if(!claim.retiring)return 'unmarked';if(claim.retirementId!==cfg.retirementId)return 'foreign';if(claim.submission&&(claim.submission.id!==cfg.owner.reservationId||!['quarantined','releasing'].includes(claim.submission.state)))return 'foreign';if(claim.inspection&&(claim.inspection.parentReservationId!==cfg.owner.reservationId||!['quarantined','clean'].includes(claim.inspection.state)))return 'foreign';return 'marked';}}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    let state = page
        .evaluate_with_arguments(
            &expression,
            &json!([{"owner":owner,"retirementId":marker}]),
            remaining(deadline)?,
            &CancellationToken::new(),
        )
        .await?;
    match state.as_str() {
        Some("unmarked") => Ok(None),
        Some("marked") => Ok(Some(marker.to_owned())),
        _ => anyhow::bail!("Pending retirement marker or exact target claim changed"),
    }
}

#[allow(clippy::too_many_arguments)] // Recovery adds a revocable permit to the existing independent retirement witnesses.
async fn retire(
    store: &SessionStore,
    journal: &TargetJournal,
    existing: &Cdp,
    endpoint: &AttachmentEndpoint,
    record: Value,
    deadline: Instant,
    allow_current_owner: bool,
    check: Option<&(dyn Fn() -> Result<()> + Send + Sync)>,
    cancel: &CancellationToken,
) -> Result<(String, String)> {
    if let Some(check) = check {
        check()?;
    }
    let operation_deadline = deadline
        .checked_sub(Duration::from_millis(100))
        .context("Target disposition deadline reached")?;
    let creator = &record["creator"];
    let target = &creator["target"];
    let host = target["host"]
        .as_str()
        .context("Creator endpoint is missing")?;
    let port = u16::try_from(target["port"].as_u64().context("Creator port is missing")?)?;
    let id = target["targetId"]
        .as_str()
        .context("Creator target is missing")?;
    let use_existing = browser::normalise_host(host) == browser::normalise_host(&endpoint.host)
        && port == endpoint.port
        && target
            .get("browserWSEndpoint")
            .is_none_or(|ws| ws.as_str() == Some(&endpoint.browser_ws_endpoint));
    let browser = if use_existing {
        existing.clone()
    } else {
        let saved = match target["browserWSEndpoint"].as_str() {
            Some(ws) => AttachmentEndpoint {
                host: host.into(),
                port,
                browser_ws_endpoint: ws.into(),
            },
            None => {
                AttachmentEndpoint::inspect(
                    host,
                    port,
                    remaining(operation_deadline)?,
                    &CancellationToken::new(),
                )
                .await?
            }
        };
        saved
            .connect(remaining(operation_deadline)?, &CancellationToken::new())
            .await?
    };
    let rows = targets(&browser, operation_deadline).await?;
    require_controllers(store, creator, operation_deadline, allow_current_owner)?;
    if rows.iter().all(|row| row["targetId"] != id) {
        if record["disposition"]["reason"] != "settled-pre-dispatch-close-pending" {
            journal.verify(&record)?;
        }
        return Ok(("confirmed-absent".into(), "already-absent".into()));
    }
    ensure!(
        record["disposition"]["reason"] != "settled-pre-dispatch-close-pending",
        "Present pre-dispatch target requires its original settled owner"
    );
    let page = browser
        .attach_target(
            id,
            remaining(operation_deadline)?,
            &CancellationToken::new(),
        )
        .await?;
    let marker = journal.retirement_id(creator, &record["commit"])?;
    let owner = if let Some(id) = record["commit"]["sessionId"]
        .as_str()
        .filter(|_| record.get("nativeOwnerIdentity").is_some())
    {
        store.require(id)?["browser"]["runtime"]["submissionOwner"].clone()
    } else {
        json!({"claimId":target["claimId"]})
    };
    let resumed = pending_marker(&page, &owner, &marker, operation_deadline).await?;
    let initialise_completed = resumed.is_none()
        && record.get("nativeOwnerIdentity").is_some()
        && matches!(
            record["commit"]["kind"].as_str(),
            Some("session" | "research-report" | "grok-session")
        );
    let resumed = if initialise_completed {
        // Persisted journal attempt and completed dispatch are verified before
        // changing the renderer. Full source/content verification follows marking.
        journal.verify(&record)?;
        require_controllers(store, creator, operation_deadline, allow_current_owner)?;
        if let Some(check) = check {
            check()?;
        }
        let (mark, cfg) = retirement_marker(store, &record, &owner, &marker, true)?;
        ensure!(
            page.evaluate_with_arguments(
                &mark,
                &json!([cfg]),
                remaining(operation_deadline)?,
                cancel
            )
            .await?
                == true,
            "Completed native retirement marker was not confirmed"
        );
        Some(marker.clone())
    } else {
        resumed
    };
    authorise(
        store,
        journal,
        &record,
        &page,
        resumed.as_deref(),
        operation_deadline,
        allow_current_owner,
    )
    .await?;
    let rows = targets(&browser, operation_deadline).await?;
    if !rows
        .iter()
        .any(|row| row["type"] == "page" && row["targetId"] != id)
    {
        if let Some(check) = check {
            check()?;
        }
        let replacement = browser
            .call(
                "Target.createTarget",
                json!({"url":"about:blank"}),
                remaining(operation_deadline)?,
                cancel,
            )
            .await?;
        ensure!(
            replacement["targetId"]
                .as_str()
                .is_some_and(crate::browser_target::valid_id)
                && replacement["targetId"] != id,
            "Unable to preserve the browser's final page"
        );
        crate::browser_chrome::record_fresh_survivor(
            store,
            &record,
            replacement["targetId"]
                .as_str()
                .context("Fresh survivor target is missing")?,
            operation_deadline,
        )?;
        // A missing persistent survivor receipt only retains the idle browser; it must not
        // stop this conversation target from closing.
        if let Err(error) = crate::browser_chrome::record_idle_survivor(
            store,
            &record,
            replacement["targetId"]
                .as_str()
                .context("Persistent survivor target is missing")?,
            operation_deadline,
        ) {
            eprintln!(
                "Persistent survivor receipt was not recorded; its browser stays open: {error:#}"
            );
        }
    }
    let (mark, cfg) = retirement_marker(store, &record, &owner, &marker, false)?;
    require_controllers(store, creator, operation_deadline, allow_current_owner)?;
    let locked = page
        .evaluate_with_arguments(
            &mark,
            &json!([cfg]),
            remaining(operation_deadline)?,
            &CancellationToken::new(),
        )
        .await;
    let checked = async {
        ensure!(
            locked? == true,
            "Exact target retirement marker was not confirmed"
        );
        authorise(
            store,
            journal,
            &record,
            &page,
            Some(&marker),
            operation_deadline,
            allow_current_owner,
        )
        .await
    }
    .await;
    if let Err(error) = checked {
        if let Ok(timeout) = remaining(operation_deadline) {
            let rollback = format!(
                "(cfg)=>{{ /* alcuin-dom:retirement-rollback */ {} const claim=readOracleTargetClaim();if(claim?.id===cfg.claimId && claim.retirementId===cfg.retirementId){{claim.retiring=false;delete claim.retirementId;writeOracleTargetClaim(claim);}}}}",
                include_str!("../browser-scripts/ownership-prelude.js")
            );
            let _ = page
                .evaluate_with_arguments(
                    &rollback,
                    &json!([{"claimId":target["claimId"],"retirementId":marker}]),
                    timeout,
                    &CancellationToken::new(),
                )
                .await;
        }
        return Err(error);
    }
    if let Some(check) = check {
        check()?;
    }
    let _ = browser
        .close_owned_target(id, remaining(operation_deadline)?, cancel)
        .await;
    loop {
        if targets(&browser, operation_deadline)
            .await?
            .iter()
            .all(|row| row["targetId"] != id)
        {
            return Ok(("confirmed-absent".into(), "exact-target-absent".into()));
        }
        tokio::time::sleep(Duration::from_millis(25).min(remaining(operation_deadline)?)).await;
    }
}

fn retirement_marker(
    store: &SessionStore,
    record: &Value,
    owner: &Value,
    marker: &str,
    completed: bool,
) -> Result<(String, Value)> {
    if record["commit"]["kind"] == "grok-session" {
        let session = store.require(
            record["commit"]["sessionId"]
                .as_str()
                .context("Committed Grok session is missing")?,
        )?;
        return Ok((
            crate::browser_grok::retirement_mark(),
            crate::browser_grok::retirement_config(&session, marker),
        ));
    }
    Ok((
        if completed {
            completed_retirement_mark_expression()
        } else {
            retirement_mark_expression()
        },
        json!({"claimId":record["creator"]["target"]["claimId"],"owner":owner,"conversationId":record["commit"]["conversationId"],"retirementId":marker}),
    ))
}

#[cfg(test)]
#[path = "browser_retirement_legacy_tests.rs"]
mod legacy_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{browser_disposition::TargetCreation, storage::atomic_json};
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    #[derive(Clone, Copy)]
    enum Fault {
        None,
        ChangedAfterMarker,
        UncertainClose,
        AssistantTurnOnly,
    }
    #[test]
    fn unused_authentication_settlement_preserves_changed_or_live_sessions() {
        for case in [
            "error",
            "cancelled",
            "running",
            "not-started",
            "live",
            "dispatched",
            "started",
            "wrong-reservation",
            "submission",
            "creator",
            "target",
            "endpoint",
            "claim",
            "lease",
            "completed",
            "unmarked",
            "unconfirmed",
        ] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join(".oracle"));
            let meta = store.create(json!({"prompt":"PUBLIC unused authentication","model":"gpt-6-pro","mode":"browser"}),home.path(),None).unwrap();
            let id = meta["id"].as_str().unwrap();
            let reservation = Uuid::new_v4().to_string();
            let request = crate::browser_runner::BrowserRequest {
                version: 1,
                session_id: id.into(),
                reservation_id: reservation.clone(),
                prompt: "PUBLIC unused authentication".into(),
                prompt_sha256: sha256(b"PUBLIC unused authentication"),
                options_sha256: sha256(&serde_json::to_vec(&meta["options"]).unwrap()),
                upload_paths: vec![],
                staged_uploads: None,
                fallback: None,
                fallback_used: false,
                extra: serde_json::Map::new(),
            };
            atomic_json(
                &store.directory(id).unwrap().join("browser-request.json"),
                &json!(request),
            )
            .unwrap();
            let journal = TargetJournal::open(&store).unwrap();
            let creator = journal.record_creation(&TargetCreation::allocated("UNUSED-PUBLIC"),&json!({"host":"127.0.0.1","port":9222,"browserWSEndpoint":"ws://127.0.0.1:9222/devtools/browser/PUBLIC","targetId":"UNUSED-PUBLIC","claimId":Uuid::new_v4().to_string()}),&ProcessIdentity::current().unwrap(),Some(id),Some(json!({"profileDir":home.path(),"id":Uuid::new_v4().to_string()}))).unwrap();
            let disposed = journal
                .record_disposition(
                    &creator,
                    if case == "unconfirmed" {
                        "unknown"
                    } else {
                        "confirmed-absent"
                    },
                    "unused-authentication-target",
                )
                .unwrap();
            let status = if matches!(case, "error" | "cancelled" | "running" | "completed") {
                case
            } else {
                "error"
            };
            let mut runtime = json!({"preNavigationCleanupPending":true,"targetDispositionPending":true,"controllerPid":u32::MAX,"chromeTargetId":creator["target"]["targetId"],"chromePort":creator["target"]["port"],"chromeHost":creator["target"]["host"],"browserWSEndpoint":creator["target"]["browserWSEndpoint"],"targetClaimId":creator["target"]["claimId"],"tabLeaseId":creator["lease"]["id"],"ownedRecoveryTarget":{"creator":creator},"PUBLIC-unrelated":"preserve"});
            match case {
                "live" => runtime["controllerPid"] = json!(std::process::id()),
                "creator" => {
                    runtime["ownedRecoveryTarget"]["creator"]["createdAt"] =
                        json!("PUBLIC replacement")
                }
                "target" => runtime["chromeTargetId"] = json!("FOREIGN-PUBLIC"),
                "endpoint" => runtime["chromePort"] = json!(9223),
                "claim" => runtime["targetClaimId"] = json!(Uuid::new_v4().to_string()),
                "lease" => runtime["tabLeaseId"] = json!(Uuid::new_v4().to_string()),
                "unmarked" => runtime["preNavigationCleanupPending"] = json!(false),
                "submission" => runtime["userCommitReceipt"] = json!({"PUBLIC":"submitted"}),
                _ => {}
            }
            store.update_session_and_model(id,"gpt-6-pro",|_,_|{
                let mut patch=json!({"status":status,"errorMessage":"PUBLIC original error","recoveryRequired":true,"browser":{"runtime":runtime},"lifecycle":{"workerPid":u32::MAX}});
                if case=="dispatched"{patch["dispatch"]=json!({"state":"uncertain"});}
                if case=="started"{patch["dispatch"]=json!({"state":"started","reservationId":reservation});}
                if case=="not-started"{patch["dispatch"]=json!({"state":"not-started","reservationId":reservation});}
                if case=="wrong-reservation"{patch["dispatch"]=json!({"state":"not-started","reservationId":Uuid::new_v4().to_string()});}
                Ok(Some((patch,Some(json!({"status":status,"errorMessage":"PUBLIC original error"})))))
            }).unwrap();
            let before = store.require(id).unwrap();
            let result = settle_unused_authentication(
                &store,
                &journal,
                &creator,
                &disposed,
                Instant::now() + Duration::from_secs(1),
            );
            let after = store.require(id).unwrap();
            if matches!(case, "error" | "cancelled" | "running" | "not-started") {
                result.unwrap();
                assert_eq!(
                    after["status"],
                    if case == "running" { "error" } else { status }
                );
                assert_eq!(after["recoveryRequired"], false);
                assert_eq!(
                    after["browser"]["runtime"]["preNavigationCleanupPending"],
                    false
                );
                assert_eq!(
                    after["browser"]["runtime"]["targetDispositionPending"],
                    false
                );
                assert_eq!(after["browser"]["runtime"]["PUBLIC-unrelated"], "preserve");
                if case == "running" {
                    assert_eq!(store.model_runs(id).unwrap()[0]["status"], "error");
                } else {
                    assert_eq!(after["errorMessage"], before["errorMessage"]);
                }
            } else {
                assert_eq!(result.is_ok(), case == "unmarked", "{case}: {result:?}");
                assert_eq!(after, before, "{case}");
            }
        }
    }
    #[test]
    fn current_owner_retirement_requires_exact_native_controller_incarnation() {
        let current = ProcessIdentity::current().unwrap();
        let creator =
            json!({"controllerPid":current.pid,"controllerStartedAtMs":current.started_at_ms});
        let session = json!({"lifecycle":{"workerPid":current.pid,"workerProcessStartedAtMs":current.started_at_ms},
            "browser":{"runtime":{"controllerPid":current.pid,"controllerProcessStartedAtMs":current.started_at_ms}}});
        assert!(require_session_controller(&session, None).is_err());
        assert!(require_session_controller(&session, Some(&creator)).is_ok());
        let mut changed = session.clone();
        changed["lifecycle"]["workerProcessStartedAtMs"] = json!(current.started_at_ms + 1.0);
        assert!(require_session_controller(&changed, Some(&creator)).is_err());
        let mut changed_creator = creator;
        changed_creator["controllerStartedAtMs"] = json!(current.started_at_ms + 1.0);
        assert!(require_session_controller(&session, Some(&changed_creator)).is_err());
    }
    #[tokio::test]
    async fn live_legacy_leases_with_missing_endpoint_fields_block_retirement() {
        for (host, port) in [(None, None), (Some("127.0.0.1"), None), (None, Some(9222))] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join(".oracle"));
            let profile = ProfileRegistry::open(&home.path().join("PUBLIC-profile")).unwrap();
            let lease = profile
                .acquire(
                    "PUBLIC-other",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let mut patch = json!({"chromeTargetId":"PUBLIC-target"});
            if let Some(host) = host {
                patch["chromeHost"] = json!(host);
            }
            if let Some(port) = port {
                patch["chromePort"] = json!(port);
            }
            lease.update(&patch).unwrap();
            let creator = json!({"controllerPid":u32::MAX,"controllerStartedAtMs":1.0,
                "target":{"host":"127.0.0.1","port":9222,"targetId":"PUBLIC-target"},
                "lease":{"id":"PUBLIC-unrelated-lease","profileDir":profile.path()}});
            assert!(
                require_controllers(
                    &store,
                    &creator,
                    Instant::now() + Duration::from_secs(1),
                    false
                )
                .is_err()
            );
            assert_eq!(
                profile.records().unwrap()["leases"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            lease.release().unwrap();
        }
    }
    #[test]
    fn another_session_with_a_missing_host_defaults_to_loopback_and_blocks_retirement() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let session = store
            .create(
                json!({"prompt":"PUBLIC other session","model":"gpt-6-pro","mode":"browser"}),
                home.path(),
                Some("PUBLIC-other"),
            )
            .unwrap();
        let id = session["id"].as_str().unwrap();
        store
            .update(id, |_| {
                Ok(Some(json!({"lifecycle":{"workerPid":std::process::id()},
            "browser":{"runtime":{"chromeTargetId":"PUBLIC-target","chromePort":9222}}})))
            })
            .unwrap();
        let creator = json!({"target":{"host":"127.0.0.1","port":9222,"targetId":"PUBLIC-target"}});
        assert!(
            require_controllers(
                &store,
                &creator,
                Instant::now() + Duration::from_secs(1),
                false
            )
            .is_err()
        );
    }
    async fn fixture(present: bool, pending: bool, fault: Fault) -> (Value, Vec<Value>) {
        fixture_owner(present, pending, fault, false).await
    }
    async fn fixture_owner(
        present: bool,
        pending: bool,
        fault: Fault,
        current_owner: bool,
    ) -> (Value, Vec<Value>) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let journal = TargetJournal::open(&store).unwrap();
        let creation = TargetCreation::allocated("PUBLIC-native-target");
        let endpoint = AttachmentEndpoint {
            host: "127.0.0.1".into(),
            port: 9222,
            browser_ws_endpoint: "ws://127.0.0.1:9222/devtools/browser/PUBLIC-browser".into(),
        };
        let creator = journal.record_creation(&creation,
            &json!({"host":endpoint.host,"port":endpoint.port,"browserWSEndpoint":endpoint.browser_ws_endpoint,
                "targetId":"PUBLIC-native-target","claimId":Uuid::new_v4().to_string()}),
            &ProcessIdentity::current().unwrap(), None, None).unwrap();
        let output = home.path().join("PUBLIC-native.md");
        std::fs::write(&output, b"**PUBLIC native answer**").unwrap();
        let commit = json!({"version":1,"kind":"native","allocationId":creator["allocationId"],
            "claimId":creator["target"]["claimId"],"conversationId":"PUBLIC-conversation",
            "output":{"file":output,"bytes":b"**PUBLIC native answer**".len(),"sha256":sha256(b"**PUBLIC native answer**")},
            "userMessageId":"PUBLIC-user","userText":"PUBLIC source","assistantMessageId":if matches!(fault, Fault::AssistantTurnOnly) { "PUBLIC-turn" } else { "PUBLIC-assistant" },
            "committedAt":"PUBLIC commitment"});
        if pending {
            journal
                .record_disposition(&creator, "unknown", "settled-pre-dispatch-close-pending")
                .unwrap();
        } else {
            journal.commit(&creator, &commit).unwrap();
            journal.finalise(&commit).unwrap();
        }
        let mut record = journal.read(&creator["allocationId"]).unwrap();
        if !current_owner {
            record["creator"]["controllerPid"] = json!(u32::MAX);
            record["nativeOwnerIdentity"]["pid"] = json!(u32::MAX);
        }
        atomic_json(
            &store
                .home()
                .join("target-dispositions")
                .join(format!("{}.json", creation.allocation_id())),
            &record,
        )
        .unwrap();
        let (client, server) = tokio::io::duplex(262144);
        let cdp = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let requests = Arc::new(Mutex::new(vec![]));
        let seen = requests.clone();
        let worker = tokio::spawn(async move {
            let mut present = present;
            let mut marked = false;
            let mut closed = false;
            let mut replacement = false;
            while let Some(Ok(frame)) = peer.next().await {
                if !frame.is_text() {
                    break;
                }
                let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                seen.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "Target.getTargets" => {
                        if closed && matches!(fault, Fault::UncertainClose) {
                            peer.send(Message::Text(json!({"id":request["id"],"error":{"message":"PUBLIC lost absence acknowledgement"}}).to_string().into())).await.unwrap();
                            continue;
                        }
                        let mut rows = vec![];
                        if present {
                            rows.push(json!({"targetId":"PUBLIC-native-target","type":"page"}));
                        }
                        if replacement {
                            rows.push(json!({"targetId":"PUBLIC-preserved-page","type":"page"}));
                        }
                        json!({"targetInfos":rows})
                    }
                    "Target.attachToTarget" => json!({"sessionId":"PUBLIC-attachment"}),
                    "Target.createTarget" => {
                        replacement = true;
                        json!({"targetId":"PUBLIC-preserved-page"})
                    }
                    "Target.closeTarget" => {
                        closed = true;
                        present = false;
                        json!({"success":true})
                    }
                    "Runtime.evaluate" => {
                        let source = request["params"]["expression"].as_str().unwrap();
                        let value = if source.contains("alcuin-dom:pending-retirement") {
                            json!(if marked { "marked" } else { "unmarked" })
                        } else if source.contains("alcuin-dom:disposition-retire") {
                            marked = true;
                            json!(true)
                        } else if source.contains("alcuin-dom:retirement-rollback") {
                            marked = false;
                            json!(true)
                        } else if source.contains("alcuin-dom:disposition-copy") {
                            json!({"success":true,"markdown":if marked && matches!(fault, Fault::ChangedAfterMarker) {"**PUBLIC changed**"} else {"**PUBLIC native answer**"}})
                        } else {
                            assert!(source.contains("alcuin-dom:result"));
                            json!({"conversationId":"PUBLIC-conversation","origin":"https://chatgpt.com",
                                "lastUserMessageId":"PUBLIC-user","lastUserText":"PUBLIC source",
                                "lastAssistantMessageId":if matches!(fault, Fault::AssistantTurnOnly) {Value::Null} else {json!("PUBLIC-assistant")},"lastAssistantTurnId":"PUBLIC-turn",
                                "assistantFollowsLatestUser":true,"stopExists":false,"strongThinkingActive":false,"completionVisible":true,
                                "text":if marked && matches!(fault, Fault::ChangedAfterMarker) {"PUBLIC changed"} else {"PUBLIC native answer"}})
                        };
                        json!({"result":{"value":value}})
                    }
                    method => panic!("Unexpected disposition command {method}"),
                };
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        if current_owner {
            retire_committed(&store, &journal, &cdp, &endpoint, &creator)
                .await
                .unwrap();
        } else {
            reconcile(&store, &cdp, &endpoint).await.unwrap();
        }
        let record = journal.read(&creator["allocationId"]).unwrap();
        cdp.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        worker.await.unwrap();
        let observed = requests.lock().unwrap().clone();
        (record, observed)
    }
    #[tokio::test]
    async fn native_reconciliation_has_no_lease_and_preserves_the_last_page() {
        let (record, requests) = fixture(true, false, Fault::None).await;
        assert_eq!(record["disposition"]["decision"], "retired");
        assert_eq!(record["disposition"]["absence"], "confirmed-absent");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "Target.closeTarget")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "Target.createTarget")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn native_retirement_accepts_an_exact_assistant_turn_when_no_message_id_exists() {
        let (record, requests) = fixture_owner(true, false, Fault::AssistantTurnOnly, true).await;
        assert_eq!(record["disposition"]["absence"], "confirmed-absent");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "Target.closeTarget")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn original_owner_retirement_closes_once_and_preserves_changed_or_uncertain_content() {
        for fault in [
            Fault::None,
            Fault::ChangedAfterMarker,
            Fault::UncertainClose,
        ] {
            let (record, requests) = fixture_owner(true, false, fault, true).await;
            let closes = requests
                .iter()
                .filter(|request| request["method"] == "Target.closeTarget")
                .count();
            if matches!(fault, Fault::None) {
                assert_eq!(record["disposition"]["decision"], "retired");
                assert_eq!(record["disposition"]["absence"], "confirmed-absent");
                assert_eq!(closes, 1);
            } else {
                assert_eq!(record["disposition"]["decision"], "retained");
                assert_eq!(record["disposition"]["absence"], "unknown");
                assert_eq!(closes, usize::from(matches!(fault, Fault::UncertainClose)));
            }
        }
    }
    #[tokio::test]
    async fn pending_reconciliation_only_confirms_an_already_absent_target() {
        for present in [false, true] {
            let (record, requests) = fixture(present, true, Fault::None).await;
            assert_eq!(
                record["disposition"]["decision"],
                if present { "retained" } else { "retired" }
            );
            assert!(
                requests
                    .iter()
                    .all(|request| request["method"] == "Target.getTargets")
            );
        }
    }
    #[tokio::test]
    async fn changed_content_rolls_back_before_close_and_uncertain_close_keeps_its_marker() {
        let (record, requests) = fixture(true, false, Fault::ChangedAfterMarker).await;
        assert_eq!(record["disposition"]["decision"], "retained");
        assert!(
            requests
                .iter()
                .all(|request| request["method"] != "Target.closeTarget")
        );
        assert!(requests.iter().any(|request| {
            request["params"]["expression"]
                .as_str()
                .is_some_and(|source| source.contains("alcuin-dom:retirement-rollback"))
        }));
        let (record, requests) = fixture(true, false, Fault::UncertainClose).await;
        assert_eq!(record["disposition"]["decision"], "retained");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "Target.closeTarget")
                .count(),
            1
        );
        assert!(requests.iter().all(|request| {
            !request["params"]["expression"]
                .as_str()
                .is_some_and(|source| source.contains("alcuin-dom:retirement-rollback"))
        }));
    }
}
