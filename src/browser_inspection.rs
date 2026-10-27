//! Historical observation does not transfer the original consultation's ownership.
//! Reference: src/cli/browserTabs.ts.
use crate::{
    browser_chrome::AttachmentEndpoint,
    browser_harvest,
    browser_recovery::{self, RecoveredConversation},
    browser_sessions::{self, HarvestSnapshot},
    browser_tabs::{self, HarvestCompletionGate},
    config::Environment,
    policy,
    sessions::SessionStore,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct InspectionOptions {
    pub reference: Option<String>,
    pub recover: bool,
    pub close_recovered: bool,
    /// Receives the reference progress line before a missing tab is reopened.
    pub progress: Option<fn(&str)>,
    /// Receives the reference legacy-session warning.
    pub warning: Option<fn(&str)>,
}
impl Default for InspectionOptions {
    fn default() -> Self {
        Self {
            reference: None,
            recover: true,
            close_recovered: false,
            progress: None,
            warning: None,
        }
    }
}
fn input_timeout(meta: &Value) -> Duration {
    Duration::from_millis(
        meta["browser"]["config"]["inputTimeoutMs"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(60_000),
    )
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let time = deadline.saturating_duration_since(Instant::now());
    ensure!(!time.is_zero(), crate::native_reply::TIMEOUT);
    Ok(time)
}
fn recoverable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.to_string() == "CDP endpoint is unavailable."
            || cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::NotConnected
                )
            })
            || cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_connect)
            || [
                "No ChatGPT tab matched",
                "No live ChatGPT tabs found",
                "Could not connect",
            ]
            .iter()
            .any(|text| cause.to_string().contains(text))
    })
}
fn source_metadata(store: &SessionStore, id: &str) -> Result<Value> {
    let saved = store.require(id)?;
    if crate::browser_attempt::internal(&saved) {
        let parent = store.require(
            saved["browser"]["attempt"]["parentId"]
                .as_str()
                .context("Provider inspection has no parent")?,
        )?;
        let child = crate::browser_attempt::active_target(store, &parent)?
            .context("Provider inspection has no active attempt")?;
        ensure!(child == saved, "Provider inspection attempt changed");
        Ok(child)
    } else {
        Ok(crate::browser_attempt::active_target(store, &saved)?.unwrap_or(saved))
    }
}

struct InspectionTarget {
    endpoint: AttachmentEndpoint,
    reference: String,
    pinned: bool,
    recovered: Option<RecoveredConversation>,
}
impl InspectionTarget {
    async fn open(
        store: &SessionStore,
        id: &str,
        meta: &Value,
        options: &InspectionOptions,
        environment: &Environment,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let operation_cancel = cancel.child_token();
        let operation = Self::open_inner(
            store,
            id,
            meta,
            options,
            environment,
            deadline,
            &operation_cancel,
        );
        tokio::pin!(operation);
        tokio::select! {biased;
            result=&mut operation=>result,
            _=tokio::time::sleep_until(deadline)=>{
                operation_cancel.cancel();
                match operation.await {
                    Ok(target)=>{
                        if let Some(resource)=target.recovered {resource.finish(true).await.context("Late observation cleanup is unconfirmed")?;}
                        bail!("Browser attachment deadline reached")
                    },
                    Err(error)=>Err(error.context("Browser attachment timed out after observation cleanup")),
                }
            }
        }
    }
    async fn open_inner(
        store: &SessionStore,
        id: &str,
        meta: &Value,
        options: &InspectionOptions,
        environment: &Environment,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        ensure!(!cancel.is_cancelled(), "Browser inspection cancelled");
        if let Some((endpoint, reference)) =
            crate::browser_attempt::pinned_target(meta, options.reference.as_deref())?
        {
            let target = browser_tabs::resolve_target(
                &endpoint,
                Some(&reference),
                true,
                remaining(deadline)?,
                cancel,
            )
            .await?;
            ensure!(
                target["targetId"] == reference,
                "Original provider inspection target changed"
            );
            return Ok(Self {
                endpoint,
                reference,
                pinned: true,
                recovered: None,
            });
        }
        let reference = options
            .reference
            .as_deref()
            .unwrap_or_else(|| browser_sessions::session_tab_ref(meta));
        let mut recorded = None;
        let initial = async {
            recorded =
                browser_sessions::session_endpoint(meta, remaining(deadline)?, cancel).await?;
            let endpoint = match &recorded {
                Some(endpoint) => endpoint.clone(),
                None => {
                    AttachmentEndpoint::inspect("127.0.0.1", 9222, remaining(deadline)?, cancel)
                        .await?
                }
            };
            browser_tabs::resolve_target(
                &endpoint,
                Some(reference),
                options.reference.is_none(),
                remaining(deadline)?,
                cancel,
            )
            .await?;
            Ok::<_, anyhow::Error>(endpoint)
        }
        .await;
        match initial {
            Ok(endpoint) => Ok(Self {
                endpoint,
                reference: reference.into(),
                pinned: false,
                recovered: None,
            }),
            Err(error) => {
                if cancel.is_cancelled()
                    || !options.recover
                    || options.reference.is_some()
                    || !recoverable(&error)
                {
                    return Err(error);
                }
                if let Some(progress) = options.progress {
                    progress(&format!(
                        "No live ChatGPT tab matched session \"{id}\". Attempting recovery by reopening the saved conversation URL."
                    ));
                }
                let recovered = browser_recovery::reopen_within(
                    store,
                    id,
                    recorded.as_ref(),
                    environment,
                    cancel,
                    remaining(deadline)?,
                )
                .await?;
                Ok(Self {
                    endpoint: recovered.endpoint.clone(),
                    reference: recovered.reference.clone(),
                    pinned: false,
                    recovered: Some(recovered),
                })
            }
        }
    }
    async fn finish(self, close: bool, result: Result<HarvestSnapshot>) -> Result<HarvestSnapshot> {
        let cleanup = match self.recovered {
            Some(resource) => resource.finish(close).await,
            None => Ok(()),
        };
        match (result, cleanup) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup)) => {
                Err(cleanup.context("Recovered observation disposition is unconfirmed"))
            }
            (Err(error), Err(cleanup)) => Err(error.context(format!(
                "Recovered observation disposition is unconfirmed: {cleanup:#}"
            ))),
        }
    }
}

pub async fn harvest(
    store: &SessionStore,
    id: &str,
    options: &InspectionOptions,
    environment: &Environment,
    cancel: &CancellationToken,
) -> Result<HarvestSnapshot> {
    crate::browser_attempt::settle_unsent_switch(store, id, cancel).await?;
    let meta = source_metadata(store, id)?;
    let source_id = meta["id"]
        .as_str()
        .context("Inspection source ID missing")?;
    browser_tabs::ready_for_session(&meta, &Value::Null, options.reference.is_some())?;
    if options.reference.is_none()
        && meta["browser"]["runtime"]
            .get("submittedPromptHash")
            .is_none()
        && let Some(warning) = options.warning
    {
        warning(
            "Legacy browser session: submitted-turn identity is unavailable; verifying latest user/assistant pairing and positive completion.",
        );
    }
    let deadline = Instant::now()
        .checked_add(input_timeout(&meta))
        .context("Invalid browser inspection deadline")?;
    let operation_cancel = cancel.child_token();
    let operation = async {
        let target = InspectionTarget::open(
            store,
            source_id,
            &meta,
            options,
            environment,
            deadline,
            &operation_cancel,
        )
        .await?;
        let snapshot = async {
            browser_sessions::harvest_target_within(
                store,
                source_id,
                &target.endpoint,
                &target.reference,
                options.reference.is_some(),
                &operation_cancel,
                remaining(deadline)?,
            )
            .await
        }
        .await;
        target.finish(options.close_recovered, snapshot).await
    };
    tokio::pin!(operation);
    tokio::select! {biased;
        result=&mut operation=>result,
        _=tokio::time::sleep_until(deadline)=>{
            operation_cancel.cancel();
            let result=operation.await;
            match result {Err(error)=>Err(error.context(crate::native_reply::TIMEOUT)),Ok(_)=>bail!(crate::native_reply::TIMEOUT)}
        }
    }
}
fn answer(tab: &Value) -> &str {
    tab.get("lastAssistantMarkdown")
        .filter(|v| !v.is_null())
        .unwrap_or(&tab["lastAssistantText"])
        .as_str()
        .unwrap_or("")
}
fn paired(tab: &Value) -> bool {
    tab["assistantFollowsLatestUser"] == true && !policy::trim(answer(tab)).is_empty()
}
fn submitted_source_available(meta: &Value, tab: &Value) -> bool {
    let runtime = &meta["browser"]["runtime"];
    if runtime.get("submittedPromptHash").is_none() {
        return true;
    }
    let (id, source) = match runtime.get("submittedPromptHashFormat") {
        Some(format) if format == "original-source-v1" => (
            &tab["lastUserOriginalSourceMessageId"],
            tab["lastUserOriginalSourceText"].is_string(),
        ),
        Some(_) => return true,
        None => (
            &tab["lastUserMessageId"],
            ["lastUserTextRaw", "lastUserText", "lastUserContentText"]
                .iter()
                .any(|field| tab[*field].is_string()),
        ),
    };
    id.as_str().is_some_and(|id| !policy::trim(id).is_empty()) && source
}

struct LiveProgress {
    gate: HarvestCompletionGate,
    hash: Option<String>,
    unchanged_since: Instant,
    freshness_deadline: Instant,
    recovered_deadline: Option<Instant>,
    started: Instant,
}
struct LiveSample {
    changed: bool,
    terminal: bool,
}
impl LiveProgress {
    fn new(now: Instant, timeout: Duration, recovered: bool, stall: Duration) -> Self {
        Self {
            gate: HarvestCompletionGate::default(),
            hash: None,
            unchanged_since: now,
            freshness_deadline: now + timeout,
            recovered_deadline: recovered.then_some(now + stall),
            started: now,
        }
    }
    fn observe(
        &mut self,
        meta: &Value,
        tab: &mut Value,
        explicit: bool,
        now: Instant,
        stall: Duration,
    ) -> Result<Option<LiveSample>> {
        self.gate.observe(
            tab,
            now.saturating_duration_since(self.started)
                .as_millis()
                .min(u64::MAX as u128) as u64,
            3,
            1200,
        );
        let runtime = &meta["browser"]["runtime"];
        if let Some(deadline) = self.recovered_deadline
            && !explicit
            && !submitted_source_available(meta, tab)
        {
            ensure!(
                now < deadline,
                "Recovered ChatGPT conversation did not become ready in time."
            );
            return Ok(None);
        }
        ensure!(
            explicit
                || browser_tabs::matches_submitted_prompt(
                    tab,
                    runtime.get("submittedPromptHash"),
                    runtime.get("submittedPromptHashFormat")
                ),
            "Latest ChatGPT user turn does not match this session prompt; refusing implicit live output."
        );
        if let Some(deadline) = self.recovered_deadline {
            if tab["authenticated"] != true || !paired(tab) {
                ensure!(
                    now < deadline,
                    "Recovered ChatGPT conversation did not become ready in time."
                );
                return Ok(None);
            }
            self.recovered_deadline = None;
        }
        if !explicit && !paired(tab) {
            ensure!(
                tab["stopExists"] == true || now < self.freshness_deadline,
                "No latest assistant answer paired with this session prompt; refusing stale live output."
            );
            return Ok(None);
        }
        let hash: String = Sha1::digest(answer(tab).as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let changed = self.hash.as_ref() != Some(&hash);
        if changed {
            self.hash = Some(hash);
            self.unchanged_since = now;
        }
        let state = if tab["authenticated"] != true {
            "detached"
        } else if tab["completionConfirmed"] == true {
            "completed"
        } else if now.saturating_duration_since(self.unchanged_since) >= stall {
            "stalled"
        } else {
            "running"
        };
        tab["state"] = json!(state);
        Ok(Some(LiveSample {
            changed,
            terminal: state != "running",
        }))
    }
}
async fn inspect(
    target: &InspectionTarget,
    explicit: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    let operation_cancel = cancel.child_token();
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid live observation deadline")?;
    let operation = async {
        let tab = browser_tabs::resolve_target(
            &target.endpoint,
            Some(&target.reference),
            target.pinned || !explicit,
            remaining(deadline)?,
            &operation_cancel,
        )
        .await?;
        ensure!(
            !target.pinned || tab["targetId"] == target.reference,
            "Original provider inspection target changed"
        );
        browser_tabs::inspect(
            &target.endpoint,
            &tab,
            true,
            remaining(deadline)?,
            &operation_cancel,
        )
        .await
    };
    tokio::pin!(operation);
    tokio::select! {biased; result=&mut operation=>result,_=tokio::time::sleep_until(deadline)=>{
        operation_cancel.cancel();
        operation.await.context("Live observation timed out after renderer cleanup").and_then(|_|bail!("Live observation timed out"))
    }}
}
/// A completed session with a captured answer and no recovery or target disposition still pending:
/// nothing is left for a plain live tail to observe or finalise.
pub(crate) fn settled_completion(meta: &Value) -> bool {
    meta["status"] == "completed"
        && !meta["browser"]["capturedAnswer"].is_null()
        && meta["recoveryRequired"] != true
        && meta["browser"]["runtime"]["targetDispositionPending"] != true
}
pub async fn live(
    store: &SessionStore,
    id: &str,
    options: &InspectionOptions,
    environment: &Environment,
    cancel: &CancellationToken,
    status: impl FnMut(&str),
) -> Result<HarvestSnapshot> {
    live_with_timeout(store, id, options, environment, cancel, status, None).await
}
pub(crate) async fn reattach(
    store: &SessionStore,
    id: &str,
    environment: &Environment,
    cancel: &CancellationToken,
    status: impl FnMut(&str),
) -> Result<HarvestSnapshot> {
    crate::browser_attempt::settle_unsent_switch(store, id, cancel).await?;
    let meta = source_metadata(store, id)?;
    let timeout = Duration::from_millis(
        meta["browser"]["config"]["timeoutMs"]
            .as_u64()
            .unwrap_or(120_000),
    );
    live_with_timeout(
        store,
        id,
        &InspectionOptions::default(),
        environment,
        cancel,
        status,
        Some(timeout),
    )
    .await
}
async fn live_with_timeout(
    store: &SessionStore,
    id: &str,
    options: &InspectionOptions,
    environment: &Environment,
    cancel: &CancellationToken,
    mut status: impl FnMut(&str),
    response_timeout: Option<Duration>,
) -> Result<HarvestSnapshot> {
    crate::browser_attempt::settle_unsent_switch(store, id, cancel).await?;
    let meta = source_metadata(store, id)?;
    let source_id = meta["id"]
        .as_str()
        .context("Live inspection source ID missing")?;
    let explicit = options.reference.is_some();
    browser_tabs::ready_for_session(&meta, &Value::Null, explicit)?;
    let timeout = input_timeout(&meta);
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid live attachment deadline")?;
    let target = InspectionTarget::open(
        store,
        source_id,
        &meta,
        options,
        environment,
        deadline,
        cancel,
    )
    .await?;
    let result = async {
        let stall = response_timeout.unwrap_or(Duration::from_secs(60));
        let response_deadline = response_timeout.map(|timeout| Instant::now().checked_add(timeout).context("Invalid recovery response deadline")).transpose()?;
        let mut progress = LiveProgress::new(Instant::now(), timeout, target.recovered.is_some(), stall);
        loop {
            ensure!(!cancel.is_cancelled(), "Live browser inspection cancelled");
            // A plain live tail stops once the owner has settled the session; it is about to retire the tab.
            ensure!(
                response_timeout.is_some() || explicit || !settled_completion(&store.require(id)?),
                "Session {id} completed during the live tail"
            );
            let sample_timeout = match response_deadline {Some(deadline) => timeout.min(remaining(deadline)?), None => timeout};
            let mut tab = inspect(&target, explicit, sample_timeout, cancel).await?;
            if let Some(sample) = progress.observe(&meta, &mut tab, explicit, Instant::now(), stall)? {
                let integrity = if sample.changed || sample.terminal {
                    ensure!(!cancel.is_cancelled(), "Live browser inspection cancelled");
                    Some(browser_harvest::persist(store, source_id, &tab, explicit)?)
                } else { None };
                if sample.changed {
                    let snippet: String = tab["lastAssistantSnippet"].as_str().unwrap_or_else(|| answer(&tab)).chars().take(160).collect();
                    status(&format!("[{}] state={} stop={} send={} model={} snippet={snippet}",crate::sessions::timestamp(),tab["state"].as_str().unwrap_or("unknown"),if tab["stopExists"]==true{"yes"}else{"no"},if tab["sendExists"]==true{"yes"}else{"no"},tab["currentModelLabel"].as_str().filter(|s|!s.is_empty()).unwrap_or("(unknown)")));
                }
                if sample.terminal { return Ok(HarvestSnapshot { endpoint:target.endpoint.clone(), tab, integrity: integrity.context("Final live observation was not persisted")? }); }
            }
            let delay = match response_deadline {Some(deadline) => Duration::from_secs(2).min(remaining(deadline)?), None => Duration::from_secs(2)};
            tokio::select! {biased;_=cancel.cancelled()=>bail!("Live browser inspection cancelled"),_=tokio::time::sleep(delay)=>{}}
        }
    }.await;
    target.finish(options.close_recovered, result).await
}

#[cfg(test)]
#[path = "browser_inspection_tests.rs"]
mod tests;
