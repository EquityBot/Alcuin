//! Fresh observation targets never replace the original saved execution or retirement identity.
use crate::{
    artifacts::sha256,
    browser_chrome::{AttachmentEndpoint, ChromeConnection, ChromeLaunch},
    browser_profile::{LeaseHeartbeat, ProfileRegistry},
    browser_sessions::{recovery_profile, recovery_url},
    browser_target::{AllocationState, OwnedTarget, TargetAcquisition},
    config::Environment,
    sessions::SessionStore,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub struct RecoveredConversation {
    pub endpoint: AttachmentEndpoint,
    pub reference: String,
    url: String,
    target: OwnedTarget,
    connection: ChromeConnection,
    _heartbeat: LeaseHeartbeat,
}
impl RecoveredConversation {
    pub async fn finish(mut self, close: bool) -> Result<()> {
        let result = if close {
            self.target
                .close_observation(&self.url, Duration::from_millis(750))
                .await
        } else {
            self.target
                .handover_open_observation(
                    &self.url,
                    Duration::from_millis(750),
                    &CancellationToken::new(),
                )
                .await
                .map(|_| ())
        };
        let disconnected = self.connection.leave_open(Duration::from_millis(500)).await;
        result?;
        disconnected
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let time = deadline.saturating_duration_since(Instant::now());
    ensure!(!time.is_zero(), "Conversation reopening deadline reached");
    Ok(time)
}
pub async fn reopen(
    store: &SessionStore,
    id: &str,
    existing: Option<&AttachmentEndpoint>,
    environment: &Environment,
    cancel: &CancellationToken,
) -> Result<RecoveredConversation> {
    reopen_within(store, id, existing, environment, cancel, Duration::MAX).await
}
pub(crate) async fn reopen_within(
    store: &SessionStore,
    id: &str,
    existing: Option<&AttachmentEndpoint>,
    environment: &Environment,
    cancel: &CancellationToken,
    budget: Duration,
) -> Result<RecoveredConversation> {
    let metadata = store.require(id)?;
    let config = &metadata["browser"]["config"];
    let timeout = Duration::from_millis(
        config["inputTimeoutMs"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(60_000),
    )
    .min(budget);
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid reopening deadline")?;
    let operation_cancel = cancel.child_token();
    let operation = reopen_inner(
        store,
        id,
        &metadata,
        existing,
        environment,
        deadline,
        &operation_cancel,
    );
    tokio::pin!(operation);
    tokio::select! {biased;
        result=&mut operation=>result,
        _=tokio::time::sleep(timeout)=>{
            operation_cancel.cancel();
            match operation.await {
                Ok(resource)=>{let cleanup=resource.finish(true).await;bail!("Conversation reopening timed out; late observation cleanup {}",if cleanup.is_ok(){"confirmed"}else{"unconfirmed; admission retained"});},
                Err(error)=>Err(error.context("Conversation reopening timed out after observation cleanup")),
            }
        }
    }
}
#[allow(clippy::too_many_arguments)] // The deadline belongs to the whole reopen, including fallback and admission.
async fn reopen_inner(
    store: &SessionStore,
    id: &str,
    metadata: &Value,
    existing: Option<&AttachmentEndpoint>,
    environment: &Environment,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<RecoveredConversation> {
    let url = recovery_url(metadata)
        .context("Cannot recover conversation: no saved stable provider conversation URL")?;
    let config = &metadata["browser"]["config"];
    ensure!(!cancel.is_cancelled(), "Conversation reopening cancelled");
    if let Some(endpoint) = existing {
        if let Ok(connection) =
            ChromeConnection::attach(endpoint.clone(), remaining(deadline)?, cancel).await
        {
            // Endpoint-only observation has its own admission; it gains no original profile lease.
            let admission = store
                .home()
                .join("observation-admission")
                .join(sha256(endpoint.browser_ws_endpoint.as_bytes()));
            let profile = match ProfileRegistry::open(&admission) {
                Ok(profile) => profile,
                Err(error) => {
                    connection.leave_open(Duration::from_millis(500)).await?;
                    return Err(error);
                }
            };
            return open_on_connection(connection, profile, id, &url, config, deadline, cancel)
                .await;
        }
        ensure!(!cancel.is_cancelled(), "Conversation reopening cancelled");
    }
    remaining(deadline)?;
    let path = recovery_profile(metadata)?;
    let profile = ProfileRegistry::open(path)?;
    let executable = config["chromePath"]
        .as_str()
        .filter(|s| !crate::policy::trim(s).is_empty())
        .map(Into::into)
        .or_else(|| crate::browser_detect::chrome(environment))
        .context("No Chrome installation detected for recorded-profile recovery")?;
    let cwd = Path::new(
        metadata["cwd"]
            .as_str()
            .context("Saved browser working directory is missing")?,
    );
    let mut connection = ChromeConnection::launch_or_reuse(
        profile.clone(),
        ChromeLaunch {
            executable,
            cwd: cwd.into(),
            environment: environment.clone(),
            debug_port: None,
            timeout: remaining(deadline)?,
            keep_browser: true,
            headless: config["headless"] == true,
            hide_window: config["hideWindow"] == true,
            keep_window_behind: config["keepWindowBehind"] == true,
        },
        cancel,
    )
    .await?;
    connection.detach_kept_process()?;
    open_on_connection(connection, profile, id, &url, config, deadline, cancel).await
}
#[allow(clippy::too_many_arguments)] // One observation keeps its connection, admission and saved URL together.
async fn open_on_connection(
    connection: ChromeConnection,
    profile: Arc<ProfileRegistry>,
    id: &str,
    url: &str,
    config: &Value,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<RecoveredConversation> {
    let acquired = async {
        profile
            .acquire_with_limit(
                id,
                crate::browser_health::tab_limit(&config["maxConcurrentTabs"]),
                remaining(deadline)?,
                cancel,
            )
            .await
    }
    .await;
    let lease = match acquired {
        Ok(lease) => Arc::new(lease),
        Err(error) => {
            connection.leave_open(Duration::from_millis(500)).await?;
            return Err(error);
        }
    };
    let heartbeat = lease.heartbeat();
    let acquisition = TargetAcquisition::default();
    let endpoint = connection.endpoint.clone();
    let result = async {
        lease.update_until(&json!({"chromeHost":endpoint.host,"chromePort":endpoint.port,"browserWSEndpoint":endpoint.browser_ws_endpoint}),deadline)?;
        acquisition.acquire_observation(connection.browser.clone(), lease.clone(), url, cancel, remaining(deadline)?).await
    }.await;
    let mut target = match result {
        Ok(target) => target,
        Err(error) => {
            let mut error = error;
            if acquisition.state().await == AllocationState::Idle
                && let Err(cleanup) = lease
                    .release_before(Some(std::time::Instant::now() + Duration::from_millis(500)))
            {
                error = error.context(format!("Observation admission retained: {cleanup}"));
            }
            // An unacknowledged allocation retains its transport for the bounded late observer.
            if matches!(
                acquisition.state().await,
                AllocationState::Idle | AllocationState::Absent
            ) {
                connection.leave_open(Duration::from_millis(500)).await?;
            }
            return Err(error);
        }
    };
    let navigation = async {
        for method in ["Page.enable", "Runtime.enable"] {
            target
                .page()
                .call(method, json!({}), remaining(deadline)?, cancel)
                .await?;
        }
        let result = target
            .page()
            .call(
                "Page.navigate",
                json!({"url":url}),
                remaining(deadline)?,
                cancel,
            )
            .await?;
        ensure!(
            result.get("errorText").is_none(),
            "Recovered conversation navigation failed"
        );
        loop {
            let observed=target.require(remaining(deadline)?,cancel).await?;
            if observed["url"]==url {break;}
            // A fresh Chromium target can report an empty URL before its first commit.
            ensure!(matches!(observed["url"].as_str(),Some("" | "about:blank")), "Recovered conversation navigated to another URL; admission retained");
            tokio::select! {biased;_=cancel.cancelled()=>bail!("Conversation reopening cancelled"),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
        }
        ensure!(!cancel.is_cancelled(), "Conversation reopening cancelled");
        remaining(deadline)?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = navigation {
        let cleanup = target
            .close_observation(url, Duration::from_millis(750))
            .await;
        connection.leave_open(Duration::from_millis(500)).await?;
        return Err(error.context(match cleanup {
            Ok(()) => "Fresh observation cleanup confirmed".into(),
            Err(cleanup) => {
                format!("Observation cleanup unconfirmed; exact admission retained: {cleanup:#}")
            }
        }));
    }
    Ok(RecoveredConversation {
        reference: target.id().to_owned(),
        endpoint,
        url: url.into(),
        target,
        connection,
        _heartbeat: heartbeat,
    })
}

#[cfg(test)]
#[path = "browser_recovery_tests.rs"]
mod tests;
