//! Exact target allocation authority. Neither a socket nor an existing page grants retirement.
use crate::{browser_disposition::TargetCreation, browser_profile::TabLease, cdp::Cdp};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const LATE_CLEANUP: Duration = Duration::from_millis(750);
#[cfg(test)]
thread_local! {
    /// Shortens the watch on a close the fixture refuses. The watch only reads the target list,
    /// so unlike the command budget it has no durable write to leave room for.
    pub(crate) static TEST_CLOSE_WAIT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}
fn allocation_remaining(deadline: std::time::Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "Target allocation deadline reached; recovery retained"
    );
    Ok(remaining)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllocationState {
    Idle,
    Pending,
    Transferred(String),
    Absent,
    Unconfirmed(Option<String>),
}

pub struct TargetAcquisition {
    state: Arc<Mutex<AllocationState>>,
}
impl Default for TargetAcquisition {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(AllocationState::Idle)),
        }
    }
}
impl TargetAcquisition {
    pub async fn state(&self) -> AllocationState {
        self.state.lock().await.clone()
    }
    pub async fn acquire(
        &self,
        browser: Cdp,
        lease: Arc<TabLease>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<OwnedTarget> {
        self.acquire_with_intent(browser, lease, None, cancel, timeout)
            .await
    }
    pub async fn acquire_observation(
        &self,
        browser: Cdp,
        lease: Arc<TabLease>,
        url: &str,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<OwnedTarget> {
        anyhow::ensure!(
            crate::browser::conversation_id(url).is_some(),
            "Observation requires a saved provider conversation URL"
        );
        let intent = json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"url":url});
        self.acquire_with_intent(browser, lease, Some(intent), cancel, timeout)
            .await
    }
    async fn acquire_with_intent(
        &self,
        browser: Cdp,
        lease: Arc<TabLease>,
        observation: Option<Value>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<OwnedTarget> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .context("Invalid allocation deadline")?;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Target allocation cancelled before reservation"
        );
        let mut allocation = self.state.lock().await;
        anyhow::ensure!(
            *allocation == AllocationState::Idle,
            "Target allocation was already attempted; refusing replay"
        );
        lease.begin_allocation(observation.as_ref(), deadline)?;
        *allocation = AllocationState::Pending;
        drop(allocation);
        let state = self.state.clone();
        let token = cancel.clone();
        let (reply, answer) = oneshot::channel();
        // Keep observing allocation after caller cancellation. The raw acknowledgement supplies
        // the exact late target identity; a lost acknowledgement leaves admission retained.
        tokio::spawn(async move {
            let result = async {
                let raw = browser
                    .call(
                        "Target.createTarget",
                        json!({"url":"about:blank"}),
                        allocation_remaining(deadline)?,
                        &CancellationToken::new(),
                    )
                    .await?;
                let id = raw["targetId"]
                    .as_str()
                    .filter(|id| valid_id(id))
                    .context("Target allocation returned no valid target identity")?
                    .to_owned();
                *state.lock().await = AllocationState::Unconfirmed(Some(id.clone()));
                lease.update_until(&json!({"chromeTargetId":id}), deadline)?;
                let page = browser
                    .attach_target(
                        &id,
                        allocation_remaining(deadline)?,
                        &CancellationToken::new(),
                    )
                    .await?;
                let target = OwnedTarget {
                    browser: browser.clone(),
                    page,
                    id: id.clone(),
                    lease: lease.clone(),
                    retired: false,
                    creation: Some(match &observation {
                        Some(intent) => TargetCreation::observation(
                            &id,
                            intent["id"].as_str().unwrap(),
                            intent["url"].as_str().unwrap(),
                        ),
                        None => TargetCreation::allocated(&id),
                    }),
                };
                if token.is_cancelled() {
                    target.retire_unused(LATE_CLEANUP).await?;
                    *state.lock().await = AllocationState::Absent;
                    bail!("Target allocation cancelled; exact late target retirement confirmed");
                }
                *state.lock().await = AllocationState::Transferred(id);
                Ok(target)
            }
            .await;
            if result.is_err() && *state.lock().await == AllocationState::Pending {
                *state.lock().await = AllocationState::Unconfirmed(None);
            }
            // If the caller has gone away, positively retire only an unused allocated blank target.
            if let Err(Ok(target)) = reply.send(result)
                && target.retire_unused(LATE_CLEANUP).await.is_ok()
            {
                *state.lock().await = AllocationState::Absent;
            }
        });
        tokio::select! { biased;
            _ = cancel.cancelled() => bail!("Target allocation was cancelled; allocation/retirement may still be pending. Recovery is retained"),
            result = answer => result.context("Target allocation worker lost its acknowledgement")?,
        }
    }
}
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
pub struct OwnedTarget {
    browser: Cdp,
    page: Cdp,
    id: String,
    lease: Arc<TabLease>,
    retired: bool,
    creation: Option<TargetCreation>,
}
impl OwnedTarget {
    pub async fn handover_open_observation(
        &self,
        url: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        let creation = self
            .require_observation(url, false, deadline, cancel)
            .await?;
        self.lease
            .handover_observation(creation, &self.id, url, deadline, cancel)
    }
    pub(crate) async fn close_observation(&mut self, url: &str, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        self.require_observation(url, true, deadline, &CancellationToken::new())
            .await?;
        self.close_preserving_browser(deadline.saturating_duration_since(std::time::Instant::now()))
            .await
    }
    async fn require_observation(
        &self,
        url: &str,
        allow_blank: bool,
        deadline: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<&TargetCreation> {
        let creation = self
            .creation
            .as_ref()
            .filter(|creation| creation.is_observation_for(&self.id))
            .context("Only a newly allocated observation can be handed over")?;
        self.lease
            .require_observation(creation, &self.id, url, deadline, cancel)?;
        let remaining = || {
            let time = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!time.is_zero(), "Observation handover deadline reached");
            Ok::<_, anyhow::Error>(time)
        };
        let info = self.require_until(deadline, cancel).await?;
        anyhow::ensure!(
            (info["url"] == url || allow_blank && info["url"] == "about:blank")
                && crate::browser::conversation_id(url).is_some(),
            "Observation URL changed; retaining admission"
        );
        let expression = format!(
            "(() => {{ /* industrious-dom:observation-handover */ {} let claim;try{{claim=readOracleTargetClaim();}}catch(error){{if(location.href!=='about:blank'||error.name!=='SecurityError')throw error;claim=null;}}return {{url:location.href,unclaimed:claim===null}}; }})()",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        let observed = self
            .page
            .evaluate(&expression, remaining()?, cancel)
            .await?;
        anyhow::ensure!(
            observed["url"] == info["url"] && observed["unclaimed"] == true,
            "Observation renderer changed or is claimed; retaining admission"
        );
        self.lease
            .require_observation(creation, &self.id, url, deadline, cancel)?;
        Ok(creation)
    }
    fn require_close_lease(&self, deadline: std::time::Instant) -> Result<()> {
        if let Some(creation) = &self.creation
            && let Some(url) = creation.observation_url()
        {
            self.lease.require_observation(
                creation,
                &self.id,
                url,
                deadline,
                &CancellationToken::new(),
            )
        } else {
            self.lease.require_until(deadline)
        }
    }
    pub fn creation(&self) -> Option<&TargetCreation> {
        self.creation.as_ref()
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn page(&self) -> &Cdp {
        &self.page
    }
    pub fn browser(&self) -> &Cdp {
        &self.browser
    }
    pub fn lease(&self) -> &Arc<TabLease> {
        &self.lease
    }
    pub(crate) async fn attach_unsubmitted_cleanup(
        &self,
        browser: &Cdp,
        deadline: std::time::Instant,
    ) -> Result<Cdp> {
        anyhow::ensure!(!self.retired, "Cleanup target has already been retired");
        self.lease
            .require_unsubmitted_target(&self.id, deadline)
            .await?;
        let remaining = || {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!remaining.is_zero(), "Cleanup attachment deadline reached");
            Ok::<_, anyhow::Error>(remaining)
        };
        let cancel = CancellationToken::new();
        let page = browser
            .attach_target(&self.id, remaining()?, &cancel)
            .await?;
        let info = page
            .call("Target.getTargetInfo", json!({}), remaining()?, &cancel)
            .await?;
        anyhow::ensure!(
            info["targetInfo"]["targetId"] == self.id && info["targetInfo"]["type"] == "page",
            "Cleanup target attachment changed; preserving foreign target"
        );
        self.lease
            .require_unsubmitted_target(&self.id, deadline)
            .await?;
        Ok(page)
    }
    pub(crate) async fn recover(
        browser: Cdp,
        lease: Arc<TabLease>,
        target_id: String,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        lease.require()?;
        let page = browser.attach_target(&target_id, timeout, cancel).await?;
        let target = Self {
            browser,
            page,
            id: target_id,
            lease,
            retired: false,
            creation: None,
        };
        target.require(timeout, cancel).await?;
        Ok(target)
    }
    pub async fn require(&self, timeout: Duration, cancel: &CancellationToken) -> Result<Value> {
        self.require_until(std::time::Instant::now() + timeout, cancel)
            .await
    }
    async fn require_until(
        &self,
        deadline: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(!cancel.is_cancelled(), "Owned target operation cancelled");
        self.lease.require_until(deadline)?;
        anyhow::ensure!(!self.retired, "Owned target has already been retired");
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Owned target identity deadline reached"
        );
        let info = self
            .page
            .call("Target.getTargetInfo", json!({}), remaining, cancel)
            .await?;
        anyhow::ensure!(
            info["targetInfo"]["targetId"] == self.id && info["targetInfo"]["type"] == "page",
            "Target attachment identity changed; preserving foreign target"
        );
        Ok(info["targetInfo"].clone())
    }
    async fn retire_unused(mut self, timeout: Duration) -> Result<()> {
        let cancel = CancellationToken::new();
        let deadline = std::time::Instant::now() + timeout;
        let info = self.require_until(deadline, &cancel).await?;
        anyhow::ensure!(
            info["url"] == "about:blank",
            "Allocated target changed before unused retirement; retaining recovery"
        );
        self.close_preserving_browser(allocation_remaining(deadline)?)
            .await
    }
    pub(crate) async fn retire_unclaimed_blank(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        self.require_unclaimed_blank(deadline).await?;
        self.close_preserving_browser_until(deadline, true).await
    }
    fn require_blank_admission(&self, deadline: std::time::Instant) -> Result<()> {
        let creation = self
            .creation
            .as_ref()
            .filter(|creation| creation.is_allocated_for(&self.id))
            .context("Blank retirement requires the original fresh target allocation")?;
        self.lease.require_until(deadline)?;
        let registry = self.lease.profile().records_until(deadline)?;
        let record = registry["leases"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|record| record["id"] == self.lease.id())
            .context("Blank retirement admission is missing")?;
        anyhow::ensure!(
            record["chromeTargetId"] == self.id
                && record["promptSubmitted"] != true
                && record["observationIntent"].is_null()
                && (record["targetDispositionAllocationId"].is_null()
                    || record["targetDispositionAllocationId"] == creation.allocation_id()),
            "Blank retirement allocation or submission state changed"
        );
        Ok(())
    }
    async fn require_unclaimed_blank(&self, deadline: std::time::Instant) -> Result<()> {
        self.require_blank_admission(deadline)?;
        let info = self
            .require_until(deadline, &CancellationToken::new())
            .await?;
        anyhow::ensure!(
            info["url"] == "about:blank",
            "Unused target navigated; retaining its exact ownership"
        );
        let expression = format!(
            "(() => {{ /* industrious-dom:unused-blank */ {} let claim;try{{claim=readOracleTargetClaim();}}catch(error){{if(location.href!=='about:blank'||error.name!=='SecurityError')throw error;claim=null;}}return claim===null && window===window.top && location.href==='about:blank' && window.name==='' && document.title==='' && !!document.body && document.body.childElementCount===0 && document.body.textContent.trim()===''; }})()",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "Blank retirement deadline reached");
        anyhow::ensure!(
            self.page
                .evaluate(&expression, remaining, &CancellationToken::new())
                .await?
                == true,
            "Unused blank renderer changed or was claimed; retaining its exact ownership"
        );
        // Re-read mutable admission state after the renderer response as well.
        self.require_blank_admission(deadline)
    }
    pub(crate) async fn close_preserving_browser(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        self.close_preserving_browser_until(deadline, false).await
    }
    async fn close_preserving_browser_until(
        &mut self,
        deadline: std::time::Instant,
        require_blank: bool,
    ) -> Result<()> {
        self.require_close_lease(deadline)?;
        let remaining = || {
            let time = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!time.is_zero(), "Browser preservation deadline reached");
            Ok::<_, anyhow::Error>(time)
        };
        let rows = self
            .browser
            .call(
                "Target.getTargets",
                json!({}),
                remaining()?,
                &CancellationToken::new(),
            )
            .await?;
        let rows = rows["targetInfos"]
            .as_array()
            .context("Browser preservation target list is incomplete")?;
        anyhow::ensure!(
            rows.iter()
                .all(|row| row["targetId"].as_str().is_some_and(valid_id)),
            "Browser preservation target list is malformed"
        );
        if !rows
            .iter()
            .any(|row| row["type"] == "page" && row["targetId"] != self.id)
        {
            let survivor = self
                .browser
                .call(
                    "Target.createTarget",
                    json!({"url":"about:blank"}),
                    remaining()?,
                    &CancellationToken::new(),
                )
                .await?;
            anyhow::ensure!(
                survivor["targetId"].as_str().is_some_and(valid_id)
                    && survivor["targetId"] != self.id,
                "Browser survivor page is unconfirmed"
            );
        }
        if require_blank {
            self.require_unclaimed_blank(deadline).await?;
        }
        self.close_until(deadline).await?;
        self.lease
            .confirm_target_absent_before(&self.id, Some(deadline))
    }
    pub(crate) fn release_retired_lease(&self) -> Result<()> {
        anyhow::ensure!(
            self.retired,
            "Cannot release admission before confirmed target absence"
        );
        self.lease.confirm_target_absent(&self.id)
    }
    pub(crate) async fn close_observed(&mut self, timeout: Duration) -> Result<()> {
        #[cfg(test)]
        let timeout = TEST_CLOSE_WAIT
            .get()
            .map_or(timeout, |wait| timeout.min(wait));
        if self
            .creation
            .as_ref()
            .and_then(TargetCreation::observation_url)
            .is_some()
        {
            return self.close_until(std::time::Instant::now() + timeout).await;
        }
        self.lease.require()?;
        let deadline = Instant::now() + timeout;
        let cancel = CancellationToken::new();
        // One exact close attempt. An uncertain close may still have succeeded, so inspect absence.
        let _ = self
            .browser
            .close_owned_target(&self.id, timeout, &cancel)
            .await;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "Owned target retirement is unconfirmed; retaining recovery"
            );
            let raw = self
                .browser
                .call("Target.getTargets", json!({}), remaining, &cancel)
                .await?;
            let targets = raw["targetInfos"]
                .as_array()
                .context("Target retirement has no complete target list")?;
            anyhow::ensure!(
                targets
                    .iter()
                    .all(|t| t["targetId"].as_str().is_some_and(valid_id)),
                "Target retirement list is malformed"
            );
            if targets.iter().all(|t| t["targetId"] != self.id) {
                // Retire in one write. A capacity-released recovery record cannot pass through
                // the invalid intermediate state capacityReleased=true/pending=false.
                self.retired = true;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25).min(remaining)).await;
        }
    }
    pub(crate) async fn observe_absence_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<bool> {
        self.require_close_lease(deadline)?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Owned target absence deadline reached"
        );
        let raw = self
            .browser
            .call(
                "Target.getTargets",
                json!({}),
                remaining,
                &CancellationToken::new(),
            )
            .await?;
        let rows = raw["targetInfos"]
            .as_array()
            .context("Target absence evidence is incomplete")?;
        anyhow::ensure!(
            rows.iter()
                .all(|row| row["targetId"].as_str().is_some_and(valid_id)),
            "Target absence evidence is malformed"
        );
        let absent = rows.iter().all(|row| row["targetId"] != self.id);
        if absent {
            self.retired = true;
        }
        Ok(absent)
    }
    pub(crate) async fn close_until(&mut self, deadline: std::time::Instant) -> Result<()> {
        self.require_close_lease(deadline)?;
        if let Some(url) = self
            .creation
            .as_ref()
            .and_then(TargetCreation::observation_url)
            .map(str::to_owned)
        {
            self.require_observation(&url, true, deadline, &CancellationToken::new())
                .await?;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "Owned target close deadline reached");
        let _ = self
            .browser
            .close_owned_target(&self.id, remaining, &CancellationToken::new())
            .await;
        loop {
            if self.observe_absence_until(deadline).await? {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "Owned target retirement is unconfirmed"
            );
            tokio::time::sleep(Duration::from_millis(25).min(remaining)).await;
        }
    }
}
#[cfg(test)]
#[path = "browser_target_tests.rs"]
mod tests;
