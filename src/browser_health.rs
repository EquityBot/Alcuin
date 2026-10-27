//! Advisory inspection in a fresh owned tab, without selecting or sending.
use crate::{
    browser_chrome::{ChromeConnection, ChromeLaunch},
    browser_profile::{MAX_TABS, ProfileRegistry},
    browser_renderer::RendererReservation,
    browser_target::{AllocationState, TargetAcquisition},
    cdp::Cdp,
    config::Environment,
    policy::{self, ThinkingTime},
    sessions::timestamp,
    storage::DirectoryLock,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
const LOGIN: &str = include_str!("../browser-scripts/health-login.js");
pub(crate) const GUARD: &str = include_str!("../browser-scripts/health-guard.js");
fn capture(result: &mut Value, status: &str, reason: Option<&str>) {
    result["status"] = json!(status);
    result["capturedAt"] = json!(timestamp());
    if let Some(reason) = reason {
        result["reason"] = json!(reason);
    } else {
        result.as_object_mut().unwrap().remove("reason");
    }
}
fn nonempty(value: &Value) -> Value {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .map_or(Value::Null, |s| json!(s))
}
fn model(value: &Value) -> Value {
    let label = value["label"]
        .as_str()
        .map(policy::trim)
        .filter(|s| !s.is_empty())
        .map_or(Value::Null, |s| json!(s));
    let owner = nonempty(&value["ownerKey"]);
    let count = value["checkedCount"].as_f64().unwrap_or(0.0);
    let status = if matches!(value["status"].as_str(), Some("pending" | "button-missing")) {
        "pending"
    } else if value["status"] == "already-selected"
        && !label.is_null()
        && !owner.is_null()
        && count == 1.0
    {
        "observed"
    } else {
        "ambiguous"
    };
    json!({"status":status,"label":label,"ownerKey":owner,"checkedCount":count})
}
fn version(value: &Value) -> Value {
    value
        .as_str()
        .filter(|s| {
            s.split_once('.').is_some_and(|(left, right)| {
                !left.is_empty()
                    && !right.is_empty()
                    && left.bytes().all(|b| b.is_ascii_digit())
                    && right.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .map_or(Value::Null, |s| json!(s))
}
fn effort(value: &Value) -> Value {
    let mut status = value["status"]
        .as_str()
        .filter(|status| {
            matches!(
                *status,
                "ready"
                    | "pending"
                    | "capability_unavailable"
                    | "selection_unknown"
                    | "controls_broken"
            )
        })
        .unwrap_or("selection_unknown");
    let selected = value["selectedLevel"]
        .as_str()
        .filter(|s| {
            matches!(
                *s,
                "light" | "standard" | "extended" | "extra-high" | "pro" | "heavy"
            )
        })
        .map_or(Value::Null, |s| json!(s));
    let owner = nonempty(&value["ownerKey"]);
    let capability = nonempty(&value["capabilityKey"]);
    let basis = value["basis"].as_str().map_or(Value::Null, |s| json!(s));
    if status == "ready"
        && (owner.is_null()
            || selected.is_null()
            || !match basis.as_str() {
                Some("slider-label-and-index") => true,
                // Only the direct Power slider proves Pro.
                Some("checked-tier-row") => selected != "pro",
                _ => false,
            })
        || status == "capability_unavailable"
            && (owner.is_null()
                || capability.is_null()
                || !matches!(
                    basis.as_str(),
                    Some("slider-range" | "disabled-slider" | "disabled-tier-row")
                ))
    {
        status = "selection_unknown";
    }
    let variant = version(&value["variantCanonicalVersion"]);
    let kind = if !variant.is_null() {
        value["observedModelKind"]
            .as_str()
            .filter(|s| matches!(*s, "thinking" | "instant"))
            .map_or(Value::Null, |s| json!(s))
    } else {
        Value::Null
    };
    json!({"status":status,"selectedLevel":selected,"ownerKey":owner,"canonicalVersion":version(&value["canonicalVersion"]),"variantCanonicalVersion":variant,"observedModelKind":kind,"basis":basis,"capabilityKey":capability})
}
/// GPT-6's top Power step also checks the Pro model radio, so Pro counts only for level pro.
fn latest(value: &Value, level: &str) -> bool {
    value["status"] == "observed"
        && match value["label"].as_str() {
            Some("GPT-6" | "Latest" | "最新" | "최신") => true,
            Some("Pro") => level == "pro",
            _ => false,
        }
}
pub(crate) fn tab_limit(value: &Value) -> usize {
    let number = match value {
        Value::String(s) => {
            let s = policy::trim(s);
            let end = s
                .char_indices()
                .find(|(i, c)| !c.is_ascii_digit() && !(*i == 0 && matches!(c, '+' | '-')))
                .map_or(s.len(), |(i, _)| i);
            s[..end].parse::<f64>().ok()
        }
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        _ => value.as_f64(),
    };
    number
        .filter(|n| n.is_finite() && *n > 0.0)
        .map_or(MAX_TABS, |n| (n as usize).max(1))
}
async fn pause(cancel: &CancellationToken) -> Result<()> {
    tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Browser health cancelled"),_=tokio::time::sleep(Duration::from_millis(200))=>Ok(())}
}
async fn guard(
    page: &Cdp,
    result: &mut Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    let value = page
        .evaluate_with_arguments(GUARD, &json!([{}]), timeout, cancel)
        .await?;
    ensure!(
        value["throttled"].is_boolean() && value["modelsLoadFailed"].is_boolean(),
        "Browser health guard failed"
    );
    if value["throttled"] == true {
        capture(result, "throttled", None);
        return Ok(false);
    }
    if value["modelsLoadFailed"] == true {
        capture(result, "controls_broken", None);
        return Ok(false);
    }
    Ok(true)
}
async fn access(
    page: &Cdp,
    result: &mut Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    loop {
        let value = page
            .evaluate_with_arguments(LOGIN, &json!([{"timeoutMs":2000}]), timeout, cancel)
            .await?;
        if !guard(page, result, timeout, cancel).await? {
            return Ok(false);
        }
        if value["pageUrl"] == "about:blank" {
            pause(cancel).await?;
            continue;
        }
        let origin = value["pageUrl"]
            .as_str()
            .and_then(|s| reqwest::Url::parse(s).ok())
            .map(|u| u.origin().ascii_serialization());
        if origin.as_deref() != Some("https://chatgpt.com") {
            capture(result, "controls_broken", None);
            return Ok(false);
        }
        if value["cfBlocked"] == true {
            capture(result, "controls_broken", Some("challenge_detected"));
            return Ok(false);
        }
        if value["domLoginCta"] == true
            || value["onAuthPage"] == true
            || value["sessionResolved"] == true && value["sessionAuthenticated"] == false
        {
            result["observations"]["authenticated"] = json!(false);
            capture(result, "sign_in_required", None);
            return Ok(false);
        }
        if value["sessionResolved"] != true || value["sessionAuthenticated"] != true {
            capture(result, "controls_broken", None);
            return Ok(false);
        }
        result["observations"]["authenticated"] = json!(true);
        return Ok(true);
    }
}
pub(crate) async fn inspect_page(
    page: &Cdp,
    result: &mut Value,
    level: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    for method in ["Page.enable", "Runtime.enable"] {
        page.call(method, json!({}), timeout, cancel).await?;
    }
    // Picker inspection is privacy-guarded, so health opens the Temporary Chat landing as runs do.
    let url = crate::browser_privacy::landing_url("https://chatgpt.com/")?;
    let expected = crate::browser_dom::navigation(&url)?;
    let navigation = page
        .call("Page.navigate", json!({"url":url}), timeout, cancel)
        .await?;
    ensure!(
        navigation.get("errorText").is_none(),
        "Browser health navigation failed"
    );
    if !access(page, result, timeout, cancel).await? {
        return Ok(());
    }
    let mut reservation =
        RendererReservation::acquire(page.clone(), Uuid::new_v4().to_string(), timeout, cancel)
            .await?;
    let outcome = async {
        crate::browser_dom::wait_initial_navigation(&reservation, &expected, timeout, cancel)
            .await?;
        loop {
            if !guard(page, result, timeout, cancel).await? {
                return Ok(());
            }
            let first_model = model(&reservation.inspect_current_model(timeout, cancel).await?);
            if first_model["status"] == "pending" {
                pause(cancel).await?;
                continue;
            }
            if !latest(&first_model, level) {
                capture(result, "selection_unknown", None);
                return Ok(());
            }
            let first_effort = effort(
                &reservation
                    .inspect_current_effort(level, timeout, cancel)
                    .await?,
            );
            if first_effort["status"] == "pending" {
                pause(cancel).await?;
                continue;
            }
            if !matches!(
                first_effort["status"].as_str(),
                Some("ready" | "capability_unavailable")
            ) {
                capture(result, first_effort["status"].as_str().unwrap(), None);
                return Ok(());
            }
            let final_model = model(&reservation.inspect_current_model(timeout, cancel).await?);
            let final_effort = effort(
                &reservation
                    .inspect_current_effort(level, timeout, cancel)
                    .await?,
            );
            if !access(page, result, timeout, cancel).await? {
                return Ok(());
            }
            if first_model != final_model
                || first_effort != final_effort
                || !latest(&final_model, level)
            {
                capture(result, "selection_unknown", None);
                return Ok(());
            }
            result["observations"]["modelVerified"] = json!(true);
            result["observations"]["effortVerified"] = json!(final_effort["status"] == "ready");
            capture(result, final_effort["status"].as_str().unwrap(), None);
            return Ok::<_, anyhow::Error>(());
        }
    }
    .await;
    let released = reservation
        .release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
        .await;
    outcome?;
    released
}
pub async fn inspect(
    input: &Value,
    environment: &Environment,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(
        (Duration::from_millis(1)..=Duration::from_secs(300)).contains(&timeout),
        "Browser health timeout must be between 1ms and 5m."
    );
    policy::assert_model_allowed(input["desiredModel"].as_str())?;
    let level = serde_json::to_value(
        ThinkingTime::parse(input["thinkingTime"].as_str().unwrap_or("pro"))
            .context("Unknown browser thinking time")?,
    )?;
    let home = environment
        .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("Browser health has no user home")?;
    let profile_path = crate::config::manual_login_profile_dir(
        [input["manualLoginProfileDir"].as_str()],
        environment,
        home,
    )?;
    let started = Instant::now();
    let mut result = json!({"status":"controls_broken","capturedAt":timestamp(),"elapsedMs":0,"advisory":true,"cleanupComplete":false,"observations":{"browserConnected":false,"authenticated":false,"modelVerified":false,"effortVerified":false}});
    let mut phase = "admission";
    let mut lease = None;
    let mut heartbeat = None;
    let mut bringup = None;
    let mut connection = None;
    let mut target = None;
    let acquisition = TargetAcquisition::default();
    let external_cancel = cancel;
    let operation_cancel = cancel.child_token();
    let cancel = &operation_cancel;
    let mut timed_out = false;
    let outcome = {
        let operation = async {
            let profile = ProfileRegistry::open(&profile_path)?;
            let acquired = Arc::new(
                profile
                    .acquire_admission(
                        &format!("health-{}", Uuid::new_v4()),
                        tab_limit(&input["maxConcurrentTabs"]),
                        timeout,
                        cancel,
                        true,
                    )
                    .await?,
            );
            heartbeat = Some(acquired.heartbeat());
            lease = Some(acquired.clone());
            phase = "browser";
            let Some(executable) = input["chromePath"]
                .as_str()
                .filter(|s| !policy::trim(s).is_empty())
                .map(PathBuf::from)
                .or_else(|| crate::browser_detect::chrome(environment))
            else {
                capture(&mut result, "browser_missing", None);
                return Ok(());
            };
            let mut chrome = ChromeConnection::launch_health(
                profile.clone(),
                ChromeLaunch {
                    executable: executable.clone(),
                    cwd: cwd.into(),
                    environment: environment.clone(),
                    debug_port: None,
                    timeout: timeout.saturating_sub(started.elapsed()),
                    keep_browser: true,
                    headless: false,
                    hide_window: false,
                    keep_window_behind: cfg!(windows),
                },
                input["chromePath"]
                    .as_str()
                    .is_some_and(|s| !policy::trim(s).is_empty()),
                cancel,
            )
            .await?;
            chrome.detach_kept_process()?;
            if !chrome
                .verify_health_identity(
                    input["chromePath"]
                        .as_str()
                        .filter(|s| !policy::trim(s).is_empty())
                        .map(|_| executable.as_path()),
                    timeout.saturating_sub(started.elapsed()),
                    cancel,
                )
                .await?
            {
                connection = Some(chrome);
                capture(
                    &mut result,
                    "controls_broken",
                    Some("browser_identity_unverified"),
                );
                return Ok(());
            }
            let browser = chrome.browser.clone();
            let endpoint = chrome.endpoint.clone();
            connection = Some(chrome);
            result["observations"]["browserConnected"] = json!(true);
            bringup = Some(DirectoryLock::acquire_until(
                &profile.path().join("oracle-browser-bringup.lock"),
                started + timeout,
            )?);
            let owned = acquisition
                .acquire(
                    browser,
                    acquired.clone(),
                    cancel,
                    timeout.saturating_sub(started.elapsed()),
                )
                .await?;
            target = Some(owned);
            acquired.update(&json!({"chromeHost":endpoint.host,"chromePort":endpoint.port}))?;
            phase = "page";
            inspect_page(
                target.as_ref().unwrap().page(),
                &mut result,
                level.as_str().unwrap(),
                timeout,
                cancel,
            )
            .await
        };
        tokio::pin!(operation);
        tokio::select! {biased;
            result=&mut operation=>result,
            _=tokio::time::sleep(timeout)=>{timed_out=true;operation_cancel.cancel();operation.await}
        }
    };
    match outcome {
        Ok(()) => {}
        Err(error) => {
            capture(
                &mut result,
                "controls_broken",
                if phase == "admission" && (timed_out || started.elapsed() >= timeout) {
                    Some("admission_deadline_exceeded")
                } else if phase == "browser"
                    && error.chain().any(|cause| {
                        cause
                            .downcast_ref::<crate::browser_chrome::ChromeIdentityRefusal>()
                            .is_some()
                    })
                {
                    Some("browser_identity_unverified")
                } else {
                    None
                },
            );
        }
    }
    let cleanup_deadline = Instant::now() + Duration::from_millis(750);
    let mut clean = if let Some(owned) = target.as_mut() {
        owned
            .close_preserving_browser(cleanup_deadline.saturating_duration_since(Instant::now()))
            .await
            .is_ok()
    } else {
        matches!(
            acquisition.state().await,
            AllocationState::Idle | AllocationState::Absent
        )
    };
    if clean
        && target.is_none()
        && acquisition.state().await == AllocationState::Idle
        && let Some(lease) = &lease
    {
        clean = lease.release_before(Some(cleanup_deadline)).is_ok();
    }
    if let Some(lock) = bringup {
        clean &= lock.release_until(cleanup_deadline).is_ok();
    }
    if let Some(chrome) = connection
        && matches!(
            acquisition.state().await,
            AllocationState::Idle | AllocationState::Absent | AllocationState::Transferred(_)
        )
    {
        clean &= chrome
            .browser
            .disconnect_and_wait(
                Duration::from_millis(100)
                    .min(cleanup_deadline.saturating_duration_since(Instant::now())),
            )
            .await
            .is_ok();
    }
    drop(heartbeat);
    result["cleanupComplete"] = json!(phase != "admission" && clean);
    result["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    ensure!(!external_cancel.is_cancelled(), "Browser health cancelled");
    Ok(result)
}
pub fn format(result: &Value) -> String {
    let status = result["status"].as_str().unwrap_or("controls_broken");
    let reason = result["reason"].as_str();
    let summary = format!(
        "Browser health: {status}{}. Advisory snapshot; cleanup {}.",
        reason.map_or(String::new(), |s| format!(" ({s})")),
        if result["cleanupComplete"] == true {
            "confirmed"
        } else {
            "unconfirmed"
        }
    );
    let recovery = match (reason, status) {
        (Some("challenge_detected"), _) => {
            "A security challenge was detected while checking access. Inspect the dedicated browser profile, complete any visible verification manually, then rerun browser-health."
        }
        (Some("admission_deadline_exceeded"), _) => {
            "Health did not obtain browser admission before its deadline. Allow active work or cleanup to finish and retry. If this persists, inspect profile admission; contention is unconfirmed."
        }
        (Some("browser_identity_unverified"), _) => {
            "Browser identity or profile ownership could not be verified. Allow active work and cleanup to finish, then inspect the dedicated profile manually. Restart that profile only when safe, or use a new dedicated profile with manual sign-in. Rerun browser-health."
        }
        (_, "sign_in_required") => {
            "Sign in manually in the dedicated browser profile, then rerun browser-health."
        }
        (_, "browser_missing") => {
            "Set --browser-chrome-path to an installed supported Chromium browser, then rerun browser-health."
        }
        (_, "throttled") => {
            "Allow the account limit to clear before retrying, and reduce concurrent consultations."
        }
        (_, "controls_broken") => {
            "Health could not verify the browser state. Inspect the local browser setup before retrying."
        }
        _ => "",
    };
    if recovery.is_empty() {
        summary
    } else {
        format!("{summary}\n{recovery}")
    }
}

#[cfg(test)]
#[path = "browser_health_tests.rs"]
mod tests;
