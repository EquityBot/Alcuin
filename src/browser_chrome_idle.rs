//! Retirement of a newly launched persistent browser's exact startup page.
//! This capability never removes the profile or signals a process.
use super::*;
use anyhow::ensure;
use serde_json::json;
use std::time::Instant;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct IdleStartup {
    pub(super) target: Option<String>,
    pub(super) url: String,
}
const STARTUP_PREFIX: &str =
    "data:text/html,%3C!doctype%20html%3E%3Ctitle%3EIndustrious%3C/title%3E#industrious-startup-";
/// Launch-record key for the startup nonce page.
pub(super) const STARTUP_KEY: &str = "industriousStartupUrl";
// Launches recorded before the rename to Industrious (23 October 2026) keep their original
// nonce page and record key; saved sessions must still match them.
const LEGACY_STARTUP_PREFIX: &str =
    "data:text/html,%3C!doctype%20html%3E%3Ctitle%3EBarman%3C/title%3E#barman-startup-";
const LEGACY_STARTUP_KEY: &str = "barmanStartupUrl";
pub(super) fn startup_url() -> String {
    format!("{STARTUP_PREFIX}{}", uuid::Uuid::new_v4())
}
pub(super) fn is_startup_url(url: &str) -> bool {
    [STARTUP_PREFIX, LEGACY_STARTUP_PREFIX]
        .iter()
        .any(|prefix| {
            url.strip_prefix(prefix).is_some_and(|id| {
                uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
            })
        })
}
/// The startup URL a launch record names, under its current or pre-rename key.
pub(super) fn recorded_startup_url(extra: &serde_json::Map<String, Value>) -> Option<&Value> {
    extra
        .get(STARTUP_KEY)
        .or_else(|| extra.get(LEGACY_STARTUP_KEY))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !left.is_zero(),
        "Persistent browser idle cleanup deadline reached"
    );
    Ok(left)
}
fn idle_internal(row: &Value) -> bool {
    browser_internal(row)
        || matches!(
            row["type"].as_str(),
            Some("service_worker" | "shared_worker" | "worker")
        ) && row["url"].as_str().is_some_and(|raw| {
            reqwest::Url::parse(raw).is_ok_and(|url| {
                // Provider workers outlive their closed pages and hold no window.
                url.scheme() == "https"
                    && matches!(url.host_str(), Some("grok.com" | "chatgpt.com"))
                    && url.port_or_known_default() == Some(443)
                    && url.username().is_empty()
                    && url.password().is_none()
            })
        })
}
impl IdleStartup {
    pub(super) fn pending(url: &str) -> Self {
        Self {
            target: None,
            url: url.into(),
        }
    }
    pub(super) async fn observe(
        browser: &Cdp,
        url: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        ensure!(
            is_startup_url(url),
            "Persistent startup URL has no native launch nonce"
        );
        let deadline = Instant::now() + timeout;
        loop {
            let rows = browser
                .call("Target.getTargets", json!({}), remaining(deadline)?, cancel)
                .await?;
            let matching = rows["targetInfos"]
                .as_array()
                .context("Persistent launch target inventory is unavailable")?
                .iter()
                .filter(|row| row["type"] == "page" && row["url"] == url)
                .collect::<Vec<_>>();
            ensure!(
                matching.len() <= 1,
                "Persistent launch startup target is ambiguous"
            );
            if let Some(row) = matching.first() {
                let target = row["targetId"]
                    .as_str()
                    .filter(|id| crate::browser_target::valid_id(id))
                    .context("Persistent launch startup target ID is invalid")?;
                return Ok(Self {
                    target: Some(target.into()),
                    url: url.into(),
                });
            }
            tokio::select! { biased; _=cancel.cancelled()=>bail!("Persistent startup observation cancelled"),
            _=tokio::time::sleep(remaining(deadline)?.min(Duration::from_millis(25)))=>{} }
        }
    }
    pub(super) fn require_inventory(&self, rows: &Value) -> Result<()> {
        let rows = rows["targetInfos"]
            .as_array()
            .context("Persistent browser target inventory is unavailable")?;
        let mut ids = std::collections::BTreeSet::new();
        ensure!(
            rows.iter().all(|row| row["targetId"]
                .as_str()
                .is_some_and(|id| crate::browser_target::valid_id(id) && ids.insert(id))
                && row["type"].is_string()
                && row["url"].is_string()
                && (row["type"] == "page"
                    && row["targetId"].as_str() == self.target.as_deref()
                    && row["url"] == self.url
                    || idle_internal(row))),
            "Persistent Chrome contains a foreign, changed or unretired target"
        );
        Ok(())
    }
    /// A launch may publish its nonce page after its original observation timed
    /// out. Rediscover only that nonce; a changed known target never gets rebound.
    pub(super) async fn resolve(&mut self, browser: &Cdp, deadline: Instant) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        let rows = browser
            .call(
                "Target.getTargets",
                json!({}),
                remaining(deadline)?,
                &CancellationToken::new(),
            )
            .await?;
        let matching = rows["targetInfos"]
            .as_array()
            .context("Persistent startup inventory is unavailable")?
            .iter()
            .filter(|row| row["type"] == "page" && row["url"] == self.url)
            .collect::<Vec<_>>();
        ensure!(matching.len() <= 1, "Persistent startup nonce is ambiguous");
        if let Some(row) = matching.first() {
            self.target = Some(
                row["targetId"]
                    .as_str()
                    .filter(|id| crate::browser_target::valid_id(id))
                    .context("Persistent startup target is invalid")?
                    .into(),
            );
        }
        self.require_inventory(&rows)
    }
}

impl ChromeConnection {
    pub(crate) fn has_idle_startup(&self) -> bool {
        self.idle_startup.is_some()
    }
    pub(crate) async fn settle_idle(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let startup = self
            .idle_startup
            .as_mut()
            .context("Persistent browser has no owned startup capability")?;
        ensure!(
            !self.keep_browser && self.fresh.is_none() && self.temporary_profile.is_none(),
            "Persistent browser was requested to remain open"
        );
        let child = self
            .child
            .as_mut()
            .context("Borrowed browser has no idle retirement authority")?;
        let identity = self
            .identity
            .as_ref()
            .context("Persistent browser launch identity is unavailable")?;
        ensure!(
            child.id() == identity.pid
                && child.try_wait()?.is_none()
                && identity.liveness() == Liveness::Alive,
            "Persistent launch native identity changed"
        );
        let exclusion = startup_admission(remaining(deadline)?, &CancellationToken::new()).await?;
        let profile = self
            .profile
            .as_ref()
            .context("Persistent browser profile is unavailable")?;
        profile.check()?;
        let bringup = DirectoryLock::acquire_async(
            &profile.path().join("oracle-browser-bringup.lock"),
            remaining(deadline)?,
            &CancellationToken::new(),
        )
        .await?;
        let admission_profile = Arc::clone(profile);
        let admission =
            tokio::task::spawn_blocking(move || admission_profile.empty_disposal_lock(deadline))
                .await
                .context("Persistent admission worker failed")??;
        let record: LaunchRecord = serde_json::from_value(
            read_record(&profile.path().join("oracle-chrome-native.json"))?
                .context("Persistent launch record is unavailable")?,
        )?;
        ensure!(
            record.version == 1
                && record.process_identity == *identity
                && record.endpoint.as_ref() == Some(&self.endpoint)
                && record.profile_identity == NativeEntry::open(profile.path(), true)?.identity
                && record.executable_identity
                    == NativeEntry::open(&record.executable, false)?.identity
                && read_devtools_endpoint(profile.path())?.as_ref() == Some(&self.endpoint),
            "Persistent browser launch record changed"
        );
        let observed =
            fresh_retirement::qualify_processes(&self.browser, profile.path(), identity, deadline)
                .await?;
        startup.resolve(&self.browser, deadline).await?;
        fresh_retirement::close_proven_targets(&self.browser, identity, deadline, &|rows| {
            startup.require_inventory(rows)
        })
        .await?;
        fresh_retirement::wait_native_exit(identity, deadline).await?;
        while !observed
            .iter()
            .all(|process| process.liveness() == Liveness::Dead)
            || !profile_processes(profile.path()).is_ok_and(|owners| owners.is_empty())
        {
            tokio::time::sleep(remaining(deadline)?.min(Duration::from_millis(25))).await;
        }
        profile.check()?;
        let mut child = self
            .child
            .take()
            .context("Persistent child disappeared before reaping")?;
        // Native death was already observed, so waiting cannot signal or kill it.
        tokio::task::spawn_blocking(move || child.wait())
            .await
            .context("Persistent child reaper failed")??;
        self.idle_startup = None;
        // The exit waits may have used the whole budget; releasing needs its own.
        let release = Instant::now() + Duration::from_secs(1);
        admission.release_until(release)?;
        bringup.release_until(release)?;
        exclusion.release_until(release)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_startup_accepts_only_the_exact_native_nonce_target() {
        let url = startup_url();
        assert!(is_startup_url(&url));
        let startup = IdleStartup {
            target: Some("PUBLIC-startup".into()),
            url: url.clone(),
        };
        let exact = json!({"targetInfos":[{"targetId":"PUBLIC-startup","type":"page","url":url}]});
        startup.require_inventory(&exact).unwrap();
        startup
            .require_inventory(&json!({"targetInfos":[]}))
            .unwrap();
        for (kind, origin) in ["service_worker", "shared_worker", "worker"]
            .into_iter()
            .flat_map(|kind| ["grok.com", "chatgpt.com"].map(|origin| (kind, origin)))
        {
            let worker = json!({"targetId":"PUBLIC-provider-worker","type":kind,"url":format!("https://{origin}/sw.js")});
            startup
                .require_inventory(&json!({"targetInfos":[exact["targetInfos"][0],worker]}))
                .unwrap();
            for raw in [
                format!("https://{origin}.evil/sw.js"),
                format!("http://{origin}/sw.js"),
                format!("https://user@{origin}/sw.js"),
                format!("https://{origin}:444/sw.js"),
                format!("blob:https://{origin}/PUBLIC"),
                "https://evil.example/sw.js".into(),
            ] {
                let mut foreign = worker.clone();
                foreign["url"] = json!(raw);
                assert!(
                    startup
                        .require_inventory(
                            &json!({"targetInfos":[exact["targetInfos"][0],foreign]})
                        )
                        .is_err(),
                    "{kind} {raw}"
                );
            }
        }
        for changed in [
            json!({"targetInfos":[{"targetId":"PUBLIC-startup","type":"page","url":"about:blank"}]}),
            json!({"targetInfos":[{"targetId":"PUBLIC-foreign","type":"page","url":url}]}),
            json!({"targetInfos":[{"targetId":"PUBLIC-startup","type":"page","url":"https://grok.com/"}]}),
            json!({"targetInfos":[exact["targetInfos"][0],exact["targetInfos"][0]]}),
        ] {
            assert!(startup.require_inventory(&changed).is_err(), "{changed}");
        }
        for invalid in [
            "about:blank",
            "about:blank#industrious-startup-PUBLIC",
            "about:blank#industrious-startup-00000000-0000-4000-8000-000000000000/extra",
        ] {
            assert!(!is_startup_url(invalid));
        }
    }
    #[test]
    fn launches_recorded_before_the_rename_still_match() {
        let id = "00000000-0000-4000-8000-000000000000";
        let legacy = format!("{LEGACY_STARTUP_PREFIX}{id}");
        assert!(is_startup_url(&legacy));
        assert!(!is_startup_url(&format!(
            "{LEGACY_STARTUP_PREFIX}{id}/extra"
        )));
        let mut extra = serde_json::Map::new();
        extra.insert(LEGACY_STARTUP_KEY.into(), json!(legacy));
        assert_eq!(recorded_startup_url(&extra), Some(&json!(legacy)));
        let current = startup_url();
        extra.insert(STARTUP_KEY.into(), json!(current));
        assert_eq!(recorded_startup_url(&extra), Some(&json!(current)));
        assert_eq!(recorded_startup_url(&serde_json::Map::new()), None);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    #[ignore = "Uses disposable synthetic persistent profiles and installed Chrome or Linux Chromium"]
    async fn native_persistent_idle_browser_exits_without_removing_its_profile() -> Result<()> {
        let home = tempfile::Builder::new()
            .prefix("PUBLIC-idle-native-")
            .tempdir()?;
        let mut environment = Environment::new();
        #[cfg(windows)]
        let executable = {
            for key in [
                "SystemRoot",
                "PATH",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "USERPROFILE",
                "TEMP",
                "TMP",
            ] {
                if let Ok(value) = std::env::var(key) {
                    environment.insert(key.into(), value);
                }
            }
            crate::browser_detect::chrome(&environment).context("Installed Chrome is required")?
        };
        #[cfg(target_os = "linux")]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            let executable = home.path().join("chrome");
            fs::write(
                &executable,
                "#!/bin/sh\nexec /usr/lib/chromium/chromium '--host-resolver-rules=MAP * ~NOTFOUND' --disable-component-update --renderer-process-limit=2 \"$@\"\n",
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            environment.insert("HOME".into(), home.path().to_string_lossy().into());
            environment.insert("PATH".into(), "/usr/bin:/bin".into());
            environment.insert(
                "DISPLAY".into(),
                std::env::var("DISPLAY").context("Run this fixture under disposable Xvfb")?,
            );
            executable
        };
        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(10);
        for case in [
            "clean",
            "foreign-blank",
            "navigated",
            "lease",
            "borrowed",
            "explicit-keep",
            "bringup-contention",
            "admission-contention",
            "saved-restart",
            "saved-late-startup",
            "saved-survivor",
            "saved-after-exit",
            "saved-changed-launch",
            "saved-borrowed-ack",
            "saved-slow-exit",
            "saved-borrowed-failed",
            "saved-remote-ack",
        ] {
            let profile = home.path().join(case);
            fs::create_dir_all(profile.join("Default"))?;
            let sentinel = profile.join("PUBLIC-profile-sentinel.txt");
            fs::write(&sentinel, b"PUBLIC retained login profile fixture")?;
            let config = json!({"manualLogin":true,"manualLoginProfileDir":profile,"chromePath":executable,"cookieSync":false,
                "manualLoginCookieSync":false,"headless":false,"hideWindow":true,"keepBrowser":false,"fallbackProvider":"none","maxConcurrentTabs":1});
            let configured = ChromeConnection::configured_grok(
                &config,
                &environment,
                home.path(),
                timeout,
                &cancel,
            )
            .await?;
            let mut connection = configured.connection;
            let identity = connection
                .identity
                .clone()
                .context("No exact owned native idle process")?;
            let capability = connection
                .idle_startup
                .as_ref()
                .context("Native startup nonce was not observed")?;
            let startup_id = capability
                .target
                .clone()
                .context("No observed fixture startup target")?;
            let startup_url = capability.url.clone();
            let checked=async {
                ensure!(connection.owns_process() && is_startup_url(&startup_url),"Native idle fixture did not own its launch");
                if case.starts_with("saved-") {
                    let store=crate::sessions::SessionStore::new(home.path().join(format!("sessions-{case}")));
                    // A borrowed connection reuses this launch and holds its own marker.
                    let borrower=if matches!(case,"saved-borrowed-failed"|"saved-remote-ack") {
                        Some(ChromeConnection::configured_grok(&config,&environment,home.path(),timeout,&cancel).await?.connection)
                    } else {None};
                    idle_saved::check_native_saved(&mut connection,&store,case,borrower).await?;
                    ensure!(fs::read(&sentinel)?==b"PUBLIC retained login profile fixture" && profile.is_dir(),"Saved idle cleanup removed profile bytes");
                    eprintln!("PUBLIC saved idle persistent case passed: {case}; native process {} dead; profile retained",identity.pid);
                    return Ok::<_,anyhow::Error>(());
                }
                match case {
                    "foreign-blank"=>{
                        let foreign=connection.browser.call("Target.createTarget",json!({"url":"about:blank"}),timeout,&cancel).await?;
                        ensure!(connection.settle_idle(Duration::from_secs(2)).await.is_err(),"Foreign blank did not retain native Chrome");
                        let rows=connection.browser.call("Target.getTargets",json!({}),timeout,&cancel).await?;
                        for id in [startup_id.as_str(),foreign["targetId"].as_str().context("No foreign fixture target")?] {
                            ensure!(rows["targetInfos"].as_array().unwrap().iter().any(|row|row["targetId"]==id),"Idle refusal closed a retained target");
                        }
                        let foreign=foreign["targetId"].as_str().unwrap();
                        connection.browser.close_owned_target(foreign,timeout,&cancel).await?;
                        // Close acknowledgement can precede inventory removal on Linux;
                        // wait for it so retirement tests the restored state.
                        let deadline=Instant::now()+timeout;
                        while connection.browser.call("Target.getTargets",json!({}),timeout,&cancel).await?["targetInfos"]
                            .as_array().context("No fixture target inventory")?.iter().any(|row|row["targetId"]==foreign) {
                            ensure!(Instant::now()<deadline,"Closed foreign fixture target stayed in the inventory");
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                    "navigated"=>{
                        let page=connection.browser.attach_target(&startup_id,timeout,&cancel).await?;
                        page.call("Page.navigate",json!({"url":"about:blank#PUBLIC-user-navigation"}),timeout,&cancel).await?;
                        ensure!(connection.settle_idle(Duration::from_secs(2)).await.is_err(),"Navigated startup did not retain native Chrome");
                        ensure!(page.evaluate("location.href",timeout,&cancel).await?=="about:blank#PUBLIC-user-navigation","Navigated startup was closed");
                        page.call("Page.navigate",json!({"url":startup_url}),timeout,&cancel).await?;
                        // Navigation acknowledgement can precede target inventory
                        // publication, especially on Linux. Observe the fixture's
                        // exact restoration before testing successful retirement.
                        ensure!(IdleStartup::observe(&connection.browser,&startup_url,timeout,&cancel).await?.target.as_deref()==Some(startup_id.as_str()),
                            "Fixture startup restoration changed the target identity");
                    }
                    "lease"=>{
                        let lease=Arc::new(configured.profile.acquire_with_limit("PUBLIC-idle",1,timeout,&cancel).await?);
                        ensure!(connection.settle_idle(Duration::from_secs(2)).await.is_err(),"Active admission did not retain native Chrome");
                        lease.release_unissued(Instant::now()+Duration::from_secs(1)).await?;
                    }
                    "borrowed"=>{
                        let borrowed=ChromeConnection::configured_grok(&config,&environment,home.path(),timeout,&cancel).await?;
                        let mut borrowed=borrowed.connection;
                        ensure!(borrowed.borrowed() && !borrowed.has_idle_startup(),"Reused process gained idle ownership");
                        ensure!(borrowed.settle_idle(Duration::from_secs(1)).await.is_err(),"Borrowed process gained close authority");
                        borrowed.browser.disconnect_and_wait(Duration::from_secs(1)).await?;
                    }
                    "explicit-keep"=>{
                        connection.keep_browser=true;
                        ensure!(connection.settle_idle(Duration::from_secs(1)).await.is_err(),"Explicit keep preference was ignored");
                        connection.keep_browser=false;
                    }
                    "bringup-contention" | "admission-contention"=>{
                        let lock=DirectoryLock::acquire(&profile.join(if case=="bringup-contention" {"oracle-browser-bringup.lock"} else {"oracle-tab-leases.lock"}),Duration::from_secs(1))?;
                        let started=Instant::now();
                        let (result,timer_elapsed)=tokio::join!(connection.settle_idle(Duration::from_secs(1)),async {
                            tokio::time::sleep(Duration::from_millis(20)).await;started.elapsed()
                        });
                        ensure!(result.is_err() && timer_elapsed<Duration::from_millis(500),"Contended idle lock blocked the async runtime: {timer_elapsed:?}");
                        lock.release()?;
                    }
                    _=>{}
                }
                ensure!(identity.liveness()==Liveness::Alive,"Retained native process exited before authorised cleanup");
                connection.settle_idle(Duration::from_secs(8)).await?;
                ensure!(identity.liveness()==Liveness::Dead && connection.child.is_none(),"Idle native process did not exit naturally");
                ensure!(fs::read(&sentinel)?==b"PUBLIC retained login profile fixture" && profile.is_dir(),"Idle cleanup removed persistent profile bytes");
                eprintln!("PUBLIC idle persistent case passed: {case}; native process {} dead; profile retained",identity.pid);
                Ok::<_,anyhow::Error>(())
            }.await;
            // Only this fixture's synthetic launch may be stopped after a failed
            // assertion. Production settle_idle never invokes this cleanup path.
            if checked.is_err()
                && let Some(child) = connection.child.as_mut()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = connection
                .browser
                .disconnect_and_wait(Duration::from_secs(1))
                .await;
            checked?;
        }
        Ok(())
    }
}
