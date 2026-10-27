//! Native visible Chrome lifecycle. CDP attachment is interchangeable; profiles are Chrome-owned.
use crate::{
    browser_profile::ProfileRegistry,
    browser_profile_copy::TemporaryProfile,
    cdp::{self, Cdp},
    config::Environment,
    native_fs::{NativeEntry, NativeIdentity},
    process::{Liveness, ProcessIdentity},
    storage::{DirectoryLock, atomic_json, private_file},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
#[path = "browser_chrome_fresh.rs"]
mod fresh_retirement;
#[path = "browser_chrome_idle.rs"]
mod idle_retirement;
#[path = "browser_chrome_idle_saved.rs"]
mod idle_saved;
pub(crate) use fresh_retirement::{record_fresh_survivor, retire_saved_fresh};
pub(crate) use idle_saved::{ExitPending, record_idle_survivor, retire_saved_idle};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentEndpoint {
    pub host: String,
    pub port: u16,
    pub browser_ws_endpoint: String,
}
impl AttachmentEndpoint {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.browser_ws_endpoint)?;
        anyhow::ensure!(
            self.port > 0
                && matches!(url.scheme(), "ws" | "wss")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url
                    .path()
                    .strip_prefix("/devtools/browser/")
                    .is_some_and(|s| !s.is_empty() && !s.contains('/'))
                && url
                    .host_str()
                    .is_some_and(|h| crate::browser::normalise_host(h)
                        == crate::browser::normalise_host(&self.host))
                && url.port_or_known_default() == Some(self.port),
            "CDP endpoint identity is invalid or conflicts with its host/port"
        );
        Ok(())
    }
    pub async fn connect(&self, timeout: Duration, cancel: &CancellationToken) -> Result<Cdp> {
        let started = Instant::now();
        let admission = startup_admission(timeout, cancel).await?;
        let remaining = timeout.saturating_sub(started.elapsed());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Browser connection admission deadline expired"
        );
        let result = self.connect_admitted(remaining, cancel).await;
        admission.release()?;
        result
    }
    async fn connect_admitted(&self, timeout: Duration, cancel: &CancellationToken) -> Result<Cdp> {
        self.validate()?;
        Cdp::connect(&self.browser_ws_endpoint, timeout, cancel).await
    }
    pub async fn inspect(
        host: &str,
        port: u16,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let base = endpoint_base(host, port)?;
        let raw = cdp::endpoint_json(&base, "/json/version", timeout, cancel).await?;
        let endpoint = Self {
            host: host.into(),
            port,
            browser_ws_endpoint: raw["webSocketDebuggerUrl"]
                .as_str()
                .context("Browser version has no browser WebSocket endpoint")?
                .into(),
        };
        endpoint.validate()?;
        Ok(endpoint)
    }
}
#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_STARTUP_LOCK: PathBuf;
}
async fn startup_admission(timeout: Duration, cancel: &CancellationToken) -> Result<DirectoryLock> {
    // shortcut: serialise startup across profiles so aliases cannot attach during cookie seeding.
    // A future endpoint registry can narrow this exclusion without weakening fresh-launch authority.
    #[cfg(not(test))]
    let path = startup_lock_path()?;
    #[cfg(test)]
    let path = TEST_STARTUP_LOCK
        .try_with(Clone::clone)
        .unwrap_or_else(|_| test_startup_lock_path());
    DirectoryLock::acquire_async(&path, timeout, cancel).await
}
/// Unit tests never take the user's machine-wide startup lock. Parallel tests (or a live
/// Alcuin) holding it would consume each other's fixed connection and cleanup budgets.
/// libtest runs each test on its own thread, and the tests use current-thread runtimes, so a
/// per-thread lock keeps each test's own contenders serialised against each other.
#[cfg(test)]
fn test_startup_lock_path() -> PathBuf {
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    std::env::temp_dir().join(format!(
        "alcuin-test-startup-{}-{thread}.lock",
        std::process::id()
    ))
}
#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
fn startup_lock_path() -> Result<PathBuf> {
    // SAFETY: geteuid has no memory or ownership preconditions.
    Ok(PathBuf::from(format!(
        "/tmp/alcuin-browser-startup-{}.lock",
        unsafe { libc::geteuid() }
    )))
}
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
fn startup_lock_path() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut path = [0u16; 260];
    // SAFETY: the fixed buffer meets SHGetFolderPathW's MAX_PATH contract; no token is supplied.
    let status = unsafe {
        windows_sys::Win32::UI::Shell::SHGetFolderPathW(
            std::ptr::null_mut(),
            0x001c,
            std::ptr::null_mut(),
            0,
            path.as_mut_ptr(),
        )
    };
    anyhow::ensure!(
        status >= 0,
        "Native browser startup directory is unavailable"
    );
    let end = path
        .iter()
        .position(|value| *value == 0)
        .context("Native browser startup directory is too long")?;
    anyhow::ensure!(end > 0, "Native browser startup directory is empty");
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&path[..end]))
        .join("Alcuin/browser-startup.lock"))
}
pub(crate) fn endpoint_base(host: &str, port: u16) -> Result<String> {
    anyhow::ensure!(
        port > 0
            && !host.is_empty()
            && !host
                .bytes()
                .any(|c| c.is_ascii_whitespace() || matches!(c, b'/' | b'?' | b'#' | b'@')),
        "Invalid CDP host/port"
    );
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Ok(if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    })
}
#[derive(Clone, Debug)]
pub struct ChromeLaunch {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub environment: Environment,
    pub debug_port: Option<u16>,
    pub timeout: Duration,
    pub keep_browser: bool,
    pub headless: bool,
    pub hide_window: bool,
    pub keep_window_behind: bool,
}
/// A launch refused because the profile's Chrome cannot be proven to be Alcuin's own
/// (reference `HealthChromeIdentityError`).
#[derive(Debug)]
pub(crate) struct ChromeIdentityRefusal(&'static str);
impl std::fmt::Display for ChromeIdentityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ChromeIdentityRefusal {}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchRecord {
    version: u8,
    process_identity: ProcessIdentity,
    profile_identity: NativeIdentity,
    executable: PathBuf,
    executable_identity: NativeIdentity,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    endpoint: Option<AttachmentEndpoint>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}
pub struct ChromeConnection {
    pub endpoint: AttachmentEndpoint,
    pub browser: Cdp,
    profile: Option<Arc<ProfileRegistry>>,
    child: Option<Child>,
    identity: Option<ProcessIdentity>,
    keep_browser: bool,
    bootstrap_lock: Option<DirectoryLock>,
    bootstrap_admission: Option<DirectoryLock>,
    temporary_profile: Option<TemporaryProfile>,
    copied_group: Option<u32>,
    #[cfg(windows)]
    copied_job: Option<crate::bridge_host::startup_job::Job>,
    // Boxed: ChromeConnection is held by many futures polled on the Windows main stack.
    fresh: Option<Box<FreshProfile>>,
    idle_startup: Option<idle_retirement::IdleStartup>,
    // Borrowed attachment marker; the run releases it once it holds no admission.
    attachment: Option<crate::browser_profile::AttachMarker>,
}
/// An ordinary profile created for one launch. Proven disposal alone removes it; any
/// keep-browser request, recovery, foreign target or uncertainty keeps Chrome and the profile.
struct FreshProfile {
    profile: TemporaryProfile,
    startup_targets: std::collections::BTreeSet<String>,
}

struct DisposableLaunch {
    child: Option<Child>,
    profile: Option<TemporaryProfile>,
    group: Option<u32>,
    #[cfg(windows)]
    job: Option<crate::bridge_host::startup_job::Job>,
    attempted: bool,
    // A fresh launch that fails after spawn ends its exact child; nothing else may reuse it.
    kill_on_failure: bool,
}
struct CopiedLaunch {
    profile: TemporaryProfile,
    directory: String,
}
impl DisposableLaunch {
    fn new(mut profile: Option<TemporaryProfile>) -> Self {
        // All process/occupancy refusals now belong to this guard, even before spawn.
        if let Some(profile) = &mut profile {
            profile.launched();
        }
        Self {
            child: None,
            profile,
            group: None,
            #[cfg(windows)]
            job: None,
            attempted: false,
            kill_on_failure: false,
        }
    }
    fn spawn(&mut self, command: &mut Command) -> Result<ProcessIdentity> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(
                0x00000200
                    | 0x00000008
                    | 0x08000000
                    | if self.profile.is_some() {
                        0x00000004
                    } else {
                        0
                    },
            );
            if self.profile.is_some() {
                self.job = Some(crate::bridge_host::startup_job::Job::new()?);
            }
        }
        self.child = Some(command.spawn().context("Unable to launch native Chrome")?);
        if self.profile.is_some() {
            self.group = self.child.as_ref().map(Child::id);
        }
        let child = self.child.as_ref().unwrap();
        let identity = ProcessIdentity::read(child.id())?
            .context("Chrome launch did not establish an exact native process")?;
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.assign(child)?;
            crate::bridge_host::startup_job::resume(child, &identity)?;
        }
        Ok(identity)
    }
    fn cleanup(&mut self) -> Result<()> {
        self.attempted = true;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let Some(profile) = &mut self.profile else {
            return Ok(());
        };
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate()?;
        }
        if let Some(child) = &mut self.child {
            #[cfg(test)]
            println!("PUBLIC copied cleanup pid={}", child.id());
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            while child.try_wait()?.is_none() {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "Copied Chrome parent did not exit; profile retained"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        loop {
            #[cfg(windows)]
            let descendants_absent = match &self.job {
                Some(job) => job.is_empty()?,
                None => self.group.is_none(),
            };
            #[cfg(not(windows))]
            let descendants_absent = copied_group_absent(self.group)?;
            // Exiting processes can make one scan incomplete; only a complete empty scan deletes.
            if descendants_absent
                && profile_processes(profile.path()).is_ok_and(|owners| owners.is_empty())
            {
                break;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Copied Chrome descendants remain alive or occupancy is unknown; profile retained"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        profile.cleanup_until(deadline)?;
        self.profile = None;
        self.child = None;
        Ok(())
    }
}
#[cfg(not(windows))]
fn copied_group_absent(pid: Option<u32>) -> Result<bool> {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let group = i32::try_from(pid).context("Copied Chrome group identity is invalid")?;
        anyhow::ensure!(group > 0, "Copied Chrome group identity is missing");
        // SAFETY: signal zero only observes the session created by our setsid launch.
        // Reuse or surviving descendants can delay/refuse deletion, never grant kill authority.
        if unsafe { libc::kill(-group, 0) } == 0 {
            return Ok(false);
        }
        anyhow::ensure!(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "Copied Chrome process group absence is unknown"
        );
    }
    #[cfg(not(unix))]
    anyhow::ensure!(
        pid.is_none(),
        "Copied Chrome descendant absence is unavailable"
    );
    Ok(true)
}
impl Drop for DisposableLaunch {
    fn drop(&mut self) {
        if !self.attempted
            && self.profile.is_some()
            && let Err(error) = self.cleanup()
        {
            eprintln!("Copied Chrome cleanup unconfirmed; profile retained: {error}");
        }
        // The owned Child pins this exact process (Windows handle, unreaped Unix PID).
        if self.kill_on_failure
            && let Some(child) = &mut self.child
            && let Err(error) = end_child(child, Duration::from_secs(5))
        {
            eprintln!("Failed fresh Chrome launch did not exit; profile retained: {error}");
        }
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            job.close_without_wait();
        }
    }
}
impl Drop for ChromeConnection {
    fn drop(&mut self) {
        // Normal callers use dispose_copy off-thread. This bounded fallback also owns
        // cancellation that drops a future after the disposable browser was launched.
        if self.temporary_profile.is_some() {
            drop(DisposableLaunch {
                child: self.child.take(),
                profile: self.temporary_profile.take(),
                group: self.copied_group.take(),
                #[cfg(windows)]
                job: self.copied_job.take(),
                attempted: false,
                kill_on_failure: false,
            });
        }
    }
}
pub(crate) struct ConfiguredChrome {
    pub profile: Arc<ProfileRegistry>,
    pub connection: ChromeConnection,
    pub source_profile: Option<PathBuf>,
    /// Reference progress lines for the session log.
    pub notes: Vec<String>,
}
impl ChromeConnection {
    pub(crate) fn take_bootstrap_lock(&mut self) -> Option<DirectoryLock> {
        self.bootstrap_lock.take()
    }
    pub(crate) fn take_bootstrap_admission(&mut self) -> Option<DirectoryLock> {
        self.bootstrap_admission.take()
    }
    pub(crate) async fn configured(
        config: &Value,
        environment: &Environment,
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ConfiguredChrome> {
        Self::configured_provider(config, environment, cwd, timeout, cancel).await
    }
    pub(crate) async fn configured_grok(
        config: &Value,
        environment: &Environment,
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ConfiguredChrome> {
        crate::browser_grok::require_config(config)?;
        Self::configured_provider(config, environment, cwd, timeout, cancel).await
    }
    async fn configured_provider(
        config: &Value,
        environment: &Environment,
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ConfiguredChrome> {
        let copied = if let Some(source) = crate::browser_profile_copy::configured_source(config)? {
            let deadline = std::time::Instant::now()
                .checked_add(timeout)
                .context("Invalid Chrome profile copy deadline")?;
            let source = cwd.join(source);
            let selected = config["chromeProfile"].as_str().map(str::to_owned);
            let base = environment
                .get(if cfg!(windows) { "TEMP" } else { "TMPDIR" })
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let base = cwd.join(base);
            let token = cancel.child_token();
            let _cancel_on_drop = token.clone().drop_guard();
            Some(
                tokio::task::spawn_blocking(move || {
                    let profile = TemporaryProfile::create(&base)?;
                    let directory = crate::browser_profile_copy::copy_profile(
                        &source,
                        &profile,
                        selected.as_deref(),
                        deadline,
                        &token,
                    )?;
                    Ok::<_, anyhow::Error>(CopiedLaunch { profile, directory })
                })
                .await
                .context("Chrome profile copy worker failed")??,
            )
        } else {
            None
        };
        let user_home = environment
            .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .context("Native browser worker has no user home")?;
        // Ordinary non-manual Chrome never uses the persistent manual-login profile.
        let mut fresh = if copied.is_none()
            && config["manualLogin"] != true
            && config["attachRunning"] != true
            && !config["remoteChrome"].is_object()
        {
            let base = environment
                .get(if cfg!(windows) { "TEMP" } else { "TMPDIR" })
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let base = cwd.join(base);
            let base = if prefer_system_tmp(cfg!(target_os = "linux"), &base, Path::new(user_home))
                && fs::create_dir_all("/tmp").is_ok()
            {
                PathBuf::from("/tmp")
            } else {
                base
            };
            let mut profile = tokio::task::spawn_blocking(move || TemporaryProfile::create(&base))
                .await
                .context("Fresh Chrome profile worker failed")??;
            // From here only a confirmed absent launch may delete it.
            profile.launched();
            Some(profile)
        } else {
            None
        };
        let profile_path = match copied
            .as_ref()
            .map(|copy| copy.profile.path().to_owned())
            .or_else(|| fresh.as_ref().map(|fresh| fresh.path().to_owned()))
        {
            Some(path) => path,
            None => crate::config::manual_login_profile_dir(
                [
                    config["manualLoginProfileDir"].as_str(),
                    environment
                        .get("ORACLE_BROWSER_PROFILE_DIR")
                        .map(String::as_str),
                ],
                environment,
                user_home,
            )?,
        };
        let profile = match ProfileRegistry::open(&cwd.join(profile_path)) {
            Ok(profile) => profile,
            Err(error) => {
                if let Some(mut fresh) = fresh.take() {
                    let _ = tokio::task::spawn_blocking(move || fresh.cleanup()).await;
                }
                return Err(error);
            }
        };
        let mut source_profile = None;
        let mut notes = Vec::new();
        let connection = if config["attachRunning"] == true {
            let attachment =
                crate::browser_attach_running::resolve(config, environment, timeout, cancel)
                    .await?;
            source_profile = attachment.profile_root;
            notes = attachment.notes;
            Self::attach(attachment.endpoint, timeout, cancel).await?
        } else if let Some(remote) = config["remoteChrome"].as_object() {
            let host = remote
                .get("host")
                .and_then(Value::as_str)
                .context("Remote CDP host is required")?;
            let port = remote
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .context("Remote CDP port is required")?;
            let endpoint = if let Some(ws) = config["remoteChromeBrowserWSEndpoint"].as_str() {
                AttachmentEndpoint {
                    host: host.into(),
                    port,
                    browser_ws_endpoint: ws.into(),
                }
            } else {
                AttachmentEndpoint::inspect(host, port, timeout, cancel).await?
            };
            Self::attach(endpoint, timeout, cancel).await?
        } else {
            if fresh.is_none()
                && config["keepBrowser"] != true
                && config["cookieBootstrapRequired"].is_null()
            {
                anyhow::ensure!(
                    manual_login_initialised(profile.path()),
                    "{}",
                    manual_login_not_initialised(profile.path())
                );
            }
            // Boxed so this wrapper never enlarges callers' futures or debug stack frames.
            let launched = Box::pin(async {
                let executable = config["chromePath"]
                    .as_str()
                    .map(PathBuf::from)
                    .or_else(|| crate::browser_detect::chrome(environment))
                    .context(
                        "No Chrome installation detected; supply trusted chromePath or CHROME_PATH",
                    )?;
                // Every owned headed persistent launch, ChatGPT and Grok alike, can exit once
                // idle. Fresh and copied profiles keep about:blank: their own disposal proves it.
                let startup_url = if fresh.is_none()
                    && copied.is_none()
                    && config["keepBrowser"] != true
                    && config["headless"] != true
                {
                    idle_retirement::startup_url()
                } else {
                    "about:blank".into()
                };
                Box::pin(Self::launch(
                    profile.clone(),
                    ChromeLaunch {
                        executable,
                        cwd: cwd.into(),
                        environment: environment.clone(),
                        debug_port: config["debugPort"]
                            .as_u64()
                            .and_then(|n| u16::try_from(n).ok())
                            .filter(|n| *n > 0),
                        timeout,
                        keep_browser: config["keepBrowser"] == true,
                        headless: config["headless"] == true,
                        hide_window: config["hideWindow"] == true,
                        keep_window_behind: config["keepWindowBehind"] == true,
                    },
                    &startup_url,
                    true,
                    config["cookieBootstrapRequired"].is_object()
                        || fresh.is_some() && crate::browser_auth::ordinary_sync(config),
                    copied,
                    fresh.is_some(),
                    cancel,
                ))
                .await
            })
            .await;
            match (launched, fresh.take()) {
                (Ok(mut connection), Some(profile)) => {
                    let startup_targets = Box::pin(startup_pages(
                        &connection.browser,
                        timeout.min(Duration::from_secs(2)),
                    ))
                    .await;
                    connection.fresh = Some(Box::new(FreshProfile {
                        profile,
                        startup_targets,
                    }));
                    connection
                }
                (Ok(connection), None) => connection,
                (Err(error), Some(fresh)) => {
                    drop(profile);
                    let _ = tokio::task::spawn_blocking(move || dispose_failed_fresh(fresh)).await;
                    return Err(error);
                }
                (Err(error), None) => return Err(error),
            }
        };
        Ok(ConfiguredChrome {
            profile,
            connection,
            source_profile,
            notes,
        })
    }
    pub async fn attach(
        endpoint: AttachmentEndpoint,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let browser = endpoint.connect(timeout, cancel).await?;
        Ok(Self {
            endpoint,
            browser,
            profile: None,
            child: None,
            identity: None,
            keep_browser: true,
            bootstrap_lock: None,
            bootstrap_admission: None,
            temporary_profile: None,
            copied_group: None,
            #[cfg(windows)]
            copied_job: None,
            fresh: None,
            idle_startup: None,
            attachment: None,
        })
    }
    pub async fn launch_or_reuse(
        profile: Arc<ProfileRegistry>,
        options: ChromeLaunch,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::launch(
            profile,
            options,
            "about:blank",
            true,
            false,
            None,
            false,
            cancel,
        )
        .await
    }
    pub(crate) async fn launch_health(
        profile: Arc<ProfileRegistry>,
        options: ChromeLaunch,
        explicit_executable: bool,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::launch(
            profile,
            options,
            "about:blank",
            explicit_executable,
            false,
            None,
            false,
            cancel,
        )
        .await
    }
    pub(crate) async fn launch_manual_login(
        profile: Arc<ProfileRegistry>,
        mut options: ChromeLaunch,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        options.keep_browser = true;
        options.headless = false;
        options.hide_window = false;
        Self::launch(
            profile,
            options,
            "https://chatgpt.com/",
            true,
            false,
            None,
            false,
            cancel,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)] // A copied profile carries its own forced cleanup lifetime.
    async fn launch(
        profile: Arc<ProfileRegistry>,
        options: ChromeLaunch,
        startup_url: &str,
        require_executable: bool,
        bootstrap: bool,
        copied: Option<CopiedLaunch>,
        fresh: bool,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        #[cfg(target_os = "linux")]
        refuse_wsl(
            std::env::var_os("WSL_DISTRO_NAME").as_deref(),
            &fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default(),
        )?;
        let directory = copied.as_ref().map(|copy| copy.directory.clone());
        let mut disposable = DisposableLaunch::new(copied.map(|copy| copy.profile));
        disposable.kill_on_failure = fresh;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Chrome launch cancelled before bring-up"
        );
        let admission = startup_admission(options.timeout, cancel).await?;
        profile.check()?;
        // Keep the native bring-up lock separate from each tab's renderer submission exclusion.
        let lock = DirectoryLock::acquire_async(
            &profile.path().join("oracle-browser-bringup.lock"),
            options.timeout.min(Duration::from_secs(30)),
            cancel,
        )
        .await?;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Chrome launch cancelled after bring-up lock"
        );
        let executable = fs::canonicalize(&options.executable)?;
        let profile_entry = NativeEntry::open(profile.path(), true)?;
        let record_path = profile.path().join("oracle-chrome-native.json");
        if let Some(raw) = read_record(&record_path)? {
            let record: LaunchRecord = serde_json::from_value(raw)
                .context("Native Chrome ownership record is malformed; refusing launch")?;
            anyhow::ensure!(
                record.version == 1
                    && record.profile_identity == profile_entry.identity
                    && (!require_executable
                        || NativeEntry::open(&executable, false)?.identity
                            == record.executable_identity)
                    && NativeEntry::open(&record.executable, false)?.identity
                        == record.executable_identity,
                ChromeIdentityRefusal(
                    "Chrome executable or native profile identity changed; refusing reuse"
                )
            );
            match record.process_identity.liveness() {
                Liveness::Alive => {
                    anyhow::ensure!(
                        !bootstrap,
                        "Explicit cookie bootstrap requires a newly owned Chrome process; close its existing browser or select a separate profile"
                    );
                    anyhow::ensure!(
                        record.headless == options.headless,
                        "Existing Chrome visibility mode differs; use a separate profile or close its owned browser before changing modes"
                    );
                    let endpoint = record.endpoint.context(
                        "Existing Chrome launch has no acknowledged endpoint; recovery is required",
                    )?;
                    let actual = read_devtools_endpoint(profile.path())?.ok_or(
                        ChromeIdentityRefusal("Existing Chrome debugging identity is unavailable"),
                    )?;
                    anyhow::ensure!(
                        actual == endpoint,
                        ChromeIdentityRefusal("Chrome debugging endpoint changed; refusing reuse")
                    );
                    let browser = endpoint.connect_admitted(options.timeout, cancel).await?;
                    profile.check()?;
                    anyhow::ensure!(
                        record.process_identity.liveness() == Liveness::Alive,
                        ChromeIdentityRefusal("Chrome process identity changed during attachment")
                    );
                    #[cfg(windows)]
                    if options.keep_window_behind
                        && !options.headless
                        && let Err(error) =
                            crate::window_keeper::start(&profile, &record.process_identity).await
                    {
                        eprintln!("Could not start the Chrome window keeper: {error}");
                    }
                    // Idle cleanup takes this bring-up lock; mark the attachment before releasing it.
                    let marker_profile = Arc::clone(&profile);
                    let attachment = Some(
                        tokio::task::spawn_blocking(move || {
                            marker_profile
                                .mark_attachment(std::time::Instant::now() + Duration::from_secs(5))
                        })
                        .await
                        .context("Borrowed attachment marker worker failed")??,
                    );
                    lock.release()?;
                    admission.release()?;
                    // Reusing a recorded process is a borrowed-process attachment, not new kill authority.
                    return Ok(Self {
                        endpoint,
                        browser,
                        profile: Some(profile),
                        child: None,
                        identity: Some(record.process_identity),
                        keep_browser: true,
                        bootstrap_lock: None,
                        bootstrap_admission: None,
                        temporary_profile: None,
                        copied_group: None,
                        #[cfg(windows)]
                        copied_job: None,
                        fresh: None,
                        idle_startup: None,
                        attachment,
                    });
                }
                Liveness::Unknown => {
                    bail!(ChromeIdentityRefusal(
                        "Chrome native process identity is unknown; preserving profile"
                    ))
                }
                Liveness::Dead => {
                    // The stale record alone cannot rule out a separately launched profile owner.
                    anyhow::ensure!(
                        profile_processes(profile.path())?.is_empty(),
                        ChromeIdentityRefusal(
                            "A foreign Chrome process occupies this profile; refusing launch"
                        )
                    );
                }
            }
        } else {
            anyhow::ensure!(
                profile_processes(profile.path())?.is_empty(),
                ChromeIdentityRefusal(
                    "A foreign Chrome process occupies this profile; use an explicit borrowed attachment"
                )
            );
        }
        profile.check()?;
        anyhow::ensure!(
            chrome_basename(
                executable
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
            ),
            "This profile lifecycle is qualified only for Chrome/Chromium executables; use the separate CDP attachment boundary for other browsers"
        );
        let executable_entry = NativeEntry::open(&executable, false)?;
        if cfg!(target_os = "macos") && options.hide_window && !options.headless {
            crate::browser_window::prepare_hidden_launch(&profile)?;
        }
        let mut command = Command::new(&executable);
        command.args(chrome_flags(
            options.debug_port,
            options.headless,
            options.hide_window,
            directory.is_some(),
            options
                .environment
                .get("ORACLE_CHROME_NO_SANDBOX")
                .is_some_and(|value| value == "1"),
        ));
        // Headless Chrome is not qualified to exit after its last page, so it never gains idle authority.
        let idle_launch = !fresh
            && disposable.profile.is_none()
            && !options.keep_browser
            && !options.headless
            && idle_retirement::is_startup_url(startup_url);
        if (fresh || idle_launch) && !options.keep_browser {
            // Let a disposable headed launch exit after its final window closes.
            // The launcher defaults already disable component background pages.
            command.arg("--disable-background-mode");
        }
        command
            .arg(user_data_dir_arg(profile.path())?)
            .arg(startup_url);
        if let Some(directory) = &directory {
            command.arg(format!("--profile-directory={directory}"));
        }
        command
            .current_dir(&options.cwd)
            .env_clear()
            .envs(chrome_environment(&options.environment));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(private_file(
                &profile.path().join("oracle-chrome-stderr.log"),
                false,
            )?));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe and only detaches the newly spawned child.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Chrome launch cancelled before process creation"
        );
        let identity = disposable.spawn(&mut command)?;
        let child = disposable.child.as_mut().unwrap();
        let mut record = LaunchRecord {
            version: 1,
            process_identity: identity.clone(),
            profile_identity: profile_entry.identity.clone(),
            executable,
            executable_identity: executable_entry.identity.clone(),
            headless: options.headless,
            endpoint: None,
            extra: serde_json::Map::new(),
        };
        if idle_launch {
            record.extra.insert(
                idle_retirement::STARTUP_KEY.into(),
                serde_json::json!(startup_url),
            );
        }
        atomic_json(&record_path, &record)?;
        // Observe startup without killing an acknowledged child when cancellation arrives.
        // A fresh profile has no recovery value, so its failed launch ends the exact child.
        let kept = if fresh {
            ""
        } else {
            "; launch record retained for recovery"
        };
        let deadline = Instant::now() + options.timeout;
        let endpoint = loop {
            anyhow::ensure!(!cancel.is_cancelled(), "Chrome startup cancelled{kept}");
            anyhow::ensure!(
                Instant::now() < deadline,
                "Chrome startup deadline expired{kept}"
            );
            profile.check()?;
            if child.try_wait()?.is_some() {
                bail!("Native Chrome exited before its debugging endpoint was acknowledged");
            }
            if let Some(endpoint) = read_devtools_endpoint(profile.path())?
                && options.debug_port.is_none_or(|port| port == endpoint.port)
            {
                // Check the actual browser/version endpoint as well as Chrome's profile control file.
                if AttachmentEndpoint::inspect(
                    &endpoint.host,
                    endpoint.port,
                    Duration::from_secs(1),
                    cancel,
                )
                .await
                .is_ok_and(|actual| actual == endpoint)
                {
                    break endpoint;
                }
            }
            tokio::select! { biased; _ = cancel.cancelled() => bail!("Chrome startup cancelled{kept}"), _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
        };
        anyhow::ensure!(
            identity.liveness() == Liveness::Alive,
            "Chrome process identity changed during startup"
        );
        record.endpoint = Some(endpoint.clone());
        profile.check()?;
        atomic_json(&record_path, &record)?;
        eprintln!(
            "Launched Chrome (pid {}) on port {}",
            identity.pid, endpoint.port
        );
        if cfg!(windows) && !fresh && directory.is_none() {
            eprintln!(
                "[browser] Browser control: Windows Chrome lifecycle detached=true; windowsHide=true."
            );
        }
        // A new process replaces any earlier owner's browser; an idle owner records itself
        // once its session saves persistentBrowserOwner.
        if let Err(error) = profile.clear_idle_owner() {
            eprintln!("Earlier persistent idle owner hint was not cleared: {error:#}");
        }
        let browser = endpoint.connect_admitted(options.timeout, cancel).await?;
        let idle_startup = if idle_launch {
            Some(
                match idle_retirement::IdleStartup::observe(
                    &browser,
                    startup_url,
                    options.timeout,
                    cancel,
                )
                .await
                {
                    Ok(startup) => startup,
                    Err(error) => {
                        eprintln!("Persistent startup observation remains pending: {error:#}");
                        idle_retirement::IdleStartup::pending(startup_url)
                    }
                },
            )
        } else {
            None
        };
        #[cfg(windows)]
        if options.keep_window_behind
            && !options.headless
            && let Err(error) = crate::window_keeper::start(&profile, &identity).await
        {
            eprintln!("Could not start the Chrome window keeper: {error}");
        }
        let bootstrap_lock = if bootstrap {
            Some(lock)
        } else {
            lock.release()?;
            None
        };
        let bootstrap_admission = if bootstrap {
            Some(admission)
        } else {
            admission.release()?;
            None
        };
        Ok(Self {
            endpoint,
            browser,
            profile: Some(profile),
            child: disposable.child.take(),
            identity: Some(identity),
            keep_browser: options.keep_browser,
            bootstrap_lock,
            bootstrap_admission,
            temporary_profile: disposable.profile.take(),
            copied_group: disposable.group.take(),
            #[cfg(windows)]
            copied_job: disposable.job.take(),
            fresh: None,
            idle_startup,
            attachment: None,
        })
    }
    pub fn borrowed(&self) -> bool {
        self.child.is_none()
    }
    pub(crate) async fn verify_health_identity(
        &self,
        executable: Option<&Path>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let identity = self
            .identity
            .as_ref()
            .context("Health Chrome has no native identity")?;
        let profile = self
            .profile
            .as_ref()
            .context("Health Chrome has no native profile")?;
        profile.check()?;
        let info = self
            .browser
            .call(
                "SystemInfo.getProcessInfo",
                serde_json::json!({}),
                timeout,
                cancel,
            )
            .await?;
        let rows = info["processInfo"]
            .as_array()
            .context("Health browser process list is missing")?;
        anyhow::ensure!(
            rows.iter().all(|row| row["type"].is_string()
                && row["id"]
                    .as_u64()
                    .is_some_and(|id| id > 0 && id <= u32::MAX as u64)),
            "Health browser process list is malformed"
        );
        let browsers = rows
            .iter()
            .filter(|row| row["type"] == "browser")
            .collect::<Vec<_>>();
        if browsers.len() != 1
            || browsers[0]["id"] != identity.pid
            || identity.liveness() != Liveness::Alive
        {
            return Ok(false);
        }
        if let Some(executable) = executable {
            let actual = native_executable(identity.pid)?;
            if NativeEntry::open(&actual, false)?.identity
                != NativeEntry::open(&fs::canonicalize(executable)?, false)?.identity
            {
                return Ok(false);
            }
        }
        let owners = profile_processes(profile.path())?;
        profile.check()?;
        Ok(owners == [identity.pid] && identity.liveness() == Liveness::Alive)
    }
    pub(crate) fn detach_kept_process(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.temporary_profile.is_none(),
            "Copied Chrome cannot be retained"
        );
        anyhow::ensure!(self.keep_browser, "Only persistent Chrome can be left open");
        if let Some(child) = self.child.take() {
            tokio::spawn(reap_persistent(child));
        }
        Ok(())
    }
    pub(crate) fn owns_process(&self) -> bool {
        self.child.is_some()
    }
    pub(crate) fn is_copied(&self) -> bool {
        self.temporary_profile.is_some()
    }
    /// A consultation may need this copied browser after its attempt returns or its
    /// worker stops. Give it the same durable, exact retirement path as a fresh launch.
    pub(crate) async fn defer_copy_retirement(&mut self, timeout: Duration) -> Result<()> {
        if self.temporary_profile.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            self.fresh.is_none() && self.child.is_some() && !self.keep_browser,
            "Deferred copied Chrome requires its disposable owned launch"
        );
        // Disarm kill-on-close before any await or Send. Cancellation must preserve
        // an uncertain capture, including when dropping the original worker future.
        #[cfg(windows)]
        if let Some(job) = &mut self.copied_job {
            job.release()?;
        }
        self.fresh = Some(Box::new(FreshProfile {
            profile: self.temporary_profile.take().unwrap(),
            startup_targets: Default::default(),
        }));
        self.copied_group = None;
        #[cfg(windows)]
        {
            self.copied_job = None;
        }
        let startup_targets = startup_pages(&self.browser, timeout).await;
        self.fresh.as_mut().unwrap().startup_targets = startup_targets;
        Ok(())
    }
    pub(crate) fn retain_process(&mut self) -> Result<()> {
        if self.temporary_profile.is_some() {
            return Ok(());
        }
        self.keep_browser = true;
        self.detach_kept_process()
    }
    pub(crate) async fn leave_open(mut self, timeout: Duration) -> Result<()> {
        self.detach_kept_process()?;
        self.browser.disconnect_and_wait(timeout).await
    }
    pub(crate) async fn dispose_copy(&mut self) -> Result<bool> {
        if self.temporary_profile.is_none() {
            return Ok(false);
        }
        let mut owned = DisposableLaunch {
            child: self.child.take(),
            profile: self.temporary_profile.take(),
            group: self.copied_group.take(),
            #[cfg(windows)]
            job: self.copied_job.take(),
            attempted: false,
            kill_on_failure: false,
        };
        tokio::task::spawn_blocking(move || owned.cleanup())
            .await
            .context("Copied Chrome cleanup worker failed")??;
        Ok(true)
    }
    pub async fn terminate_owned(&mut self, timeout: Duration) -> Result<()> {
        anyhow::ensure!(
            !self.is_copied(),
            "Copied Chrome requires process-tree disposal"
        );
        let child = self
            .child
            .as_mut()
            .context("Borrowed Chrome process cannot be terminated")?;
        anyhow::ensure!(
            !self.keep_browser,
            "Persistent Chrome was requested to remain open"
        );
        let targets = self
            .browser
            .call(
                "Target.getTargets",
                serde_json::json!({}),
                timeout,
                &CancellationToken::new(),
            )
            .await?;
        anyhow::ensure!(
            targets["targetInfos"].as_array().is_some_and(Vec::is_empty),
            "Chrome still contains foreign or unretired targets; refusing process termination"
        );
        let profile = self
            .profile
            .as_ref()
            .context("Chrome has no owned native profile")?;
        profile.check()?;
        let identity = self
            .identity
            .as_ref()
            .context("Chrome process identity is unavailable")?;
        anyhow::ensure!(
            child.id() == identity.pid
                && identity.liveness() == Liveness::Alive
                && profile.records()?["leases"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
            "Chrome still has leases/recovery or its exact process identity changed"
        );
        // Child retains the native Windows process handle and an unreaped Unix child PID.
        child.kill()?;
        child.wait()?;
        self.child = None;
        Ok(())
    }
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh.is_some()
    }
    /// Hand the exact launch to its saved disposal capability. Dropping directory pins
    /// lets recovery reopen the profile; the child reaper never terminates Chrome.
    pub(crate) fn release_fresh_for_saved_disposal(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.fresh.is_some() && !self.keep_browser && self.temporary_profile.is_none(),
            "Saved disposal requires a disposable fresh launch"
        );
        self.keep_browser = true;
        self.detach_kept_process()?;
        self.fresh = None;
        self.profile = None;
        Ok(())
    }
    /// Keep-browser leaves a fresh Chrome and its profile running. Otherwise Chrome is
    /// disposed only when it holds nothing but its own untouched startup pages.
    pub(crate) async fn settle_fresh(&mut self, timeout: Duration) -> Result<()> {
        if self.fresh.is_none() {
            return Ok(());
        }
        if self.keep_browser {
            return self.detach_kept_process();
        }
        let disposed = self.dispose_fresh(timeout).await;
        if disposed.is_err() && self.child.is_some() {
            // Retained Chrome keeps its profile, records and any recovery target.
            self.keep_browser = true;
            self.detach_kept_process()?;
            // The saved launch capability can retry later. Release these private
            // directory pins now so recovery can reopen them, including on Windows.
            self.fresh = None;
        }
        disposed
    }
    async fn dispose_fresh(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let cancel = CancellationToken::new();
        // Use launch's global -> profile -> admission order, retaining bootstrap
        // exclusion if setup failed before it could hand those locks back.
        let startup = match self.bootstrap_admission.take() {
            Some(lock) => lock,
            None => {
                startup_admission(
                    deadline.saturating_duration_since(std::time::Instant::now()),
                    &cancel,
                )
                .await?
            }
        };
        let fresh = self.fresh.as_ref().context("Chrome has no fresh profile")?;
        let profile = self
            .profile
            .as_ref()
            .context("Fresh Chrome has no owned profile")?;
        profile.check()?;
        let bringup = match self.bootstrap_lock.take() {
            Some(lock) => lock,
            None => {
                DirectoryLock::acquire_async(
                    &profile.path().join("oracle-browser-bringup.lock"),
                    deadline.saturating_duration_since(std::time::Instant::now()),
                    &cancel,
                )
                .await?
            }
        };
        let admission = profile.empty_disposal_lock(deadline)?;
        let identity = self
            .identity
            .as_ref()
            .context("Fresh Chrome process identity is unavailable")?;
        let child = self
            .child
            .as_mut()
            .context("Fresh Chrome process is not owned by this worker")?;
        anyhow::ensure!(
            profile.path() == fresh.profile.path()
                && child.id() == identity.pid
                && child.try_wait()?.is_none()
                && identity.liveness() == Liveness::Alive,
            "Fresh Chrome still has leases/recovery or its exact process identity changed"
        );
        let observed =
            fresh_retirement::qualify_processes(&self.browser, profile.path(), identity, deadline)
                .await?;
        fresh_retirement::close_proven_blanks(
            &self.browser,
            &fresh.startup_targets,
            None,
            identity,
            deadline,
        )
        .await?;
        fresh_retirement::wait_native_exit(identity, deadline).await?;
        // Browser exit alone does not attest renderer/helper exit.
        while !observed
            .iter()
            .all(|process| process.liveness() == Liveness::Dead)
            || !profile_processes(profile.path()).is_ok_and(|owners| owners.is_empty())
        {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Fresh Chrome descendants remain alive or occupancy is unknown; profile retained"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        profile.check()?;
        // Close admission before releasing its in-profile lock for directory removal.
        // This marker records in-memory disposal, never restart close authority.
        let marker = crate::browser_profile::fresh_disposal_marker(profile.path())
            .context("Fresh disposal profile path is invalid")?;
        anyhow::ensure!(
            !marker.try_exists()?,
            "Fresh profile already has disposal work"
        );
        crate::storage::atomic_json_until(
            &marker,
            &serde_json::json!({
                "version":1,"kind":"in-memory-fresh-disposal","processIdentity":identity
            }),
            deadline,
        )?;
        let marker_entry = NativeEntry::open(&marker, false)?;
        #[cfg(test)]
        anyhow::ensure!(
            fresh_retirement::TEST_DISPOSAL_STOP
                .try_with(|stage| *stage != "after-pre-prompt-marker")
                .unwrap_or(true),
            "PUBLIC injected interruption after pre-prompt native exit marker"
        );
        admission.release_until(deadline)?;
        bringup.release_until(deadline)?;
        let mut child = self
            .child
            .take()
            .context("Fresh Chrome child disappeared")?;
        let mut fresh = self
            .fresh
            .take()
            .context("Fresh Chrome profile disappeared")?;
        self.profile = None;
        let result = tokio::task::spawn_blocking(move || {
            child.wait()?;
            fresh.profile.cleanup_until(deadline)?;
            crate::native_fs::remove_owned_file(&marker, &marker_entry)
        })
        .await
        .context("Fresh Chrome cleanup worker failed")?;
        match (result, startup.release_until(deadline)) {
            (Err(cleanup), Err(release)) => Err(cleanup.context(format!(
                "Fresh Chrome startup exclusion release also failed: {release:#}"
            ))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}
/// The exact Chrome a termination signal may end and the disposable profile it then removes
/// (reference registerTerminationHooks `chrome.kill()` and `rm(userDataDir)`).
pub(crate) struct SignalTarget {
    identity: Option<ProcessIdentity>,
    // A duplicate of the child's own handle: termination can never reach a reused PID.
    #[cfg(windows)]
    process: Option<std::os::windows::io::OwnedHandle>,
    profile: Option<crate::browser_profile_copy::ProfilePins>,
}
impl SignalTarget {
    /// Without an owned child or a disposable profile, nothing is ended or removed.
    pub(crate) fn new(child: Option<&Child>, profile: Option<&TemporaryProfile>) -> Self {
        Self {
            // The owned child pins its PID (Windows handle, unreaped Unix child).
            identity: child.and_then(|child| ProcessIdentity::read(child.id()).ok().flatten()),
            #[cfg(windows)]
            process: child.and_then(|child| {
                std::os::windows::io::AsHandle::as_handle(child)
                    .try_clone_to_owned()
                    .ok()
            }),
            profile: profile.and_then(TemporaryProfile::pins),
        }
    }
    pub(crate) fn terminate(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        if let Some(identity) = &self.identity
            && identity.liveness() == Liveness::Alive
        {
            // SAFETY: Alcuin launched this exact process as a setsid group leader, and its
            // unreaped child still pins the PID.
            #[cfg(unix)]
            unsafe {
                libc::kill(-(identity.pid as i32), libc::SIGKILL);
            }
            // SAFETY: the handle is a live duplicate of Alcuin's own child process handle.
            #[cfg(windows)]
            if let Some(process) = &self.process {
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        std::os::windows::io::AsRawHandle::as_raw_handle(process),
                        1,
                    );
                }
            }
        }
        let Some(profile) = &self.profile else {
            return;
        };
        // Only a confirmed exit permits removal, as for every disposable profile.
        while !(self
            .identity
            .as_ref()
            .is_none_or(|identity| identity.liveness() == Liveness::Dead)
            && profile_processes(profile.path()).is_ok_and(|owners| owners.is_empty()))
        {
            if std::time::Instant::now() >= deadline {
                eprintln!("Chrome did not exit; profile retained");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if let Err(error) = profile.remove(deadline) {
            eprintln!("Chrome profile retained: {error:#}");
        }
    }
}
impl ChromeConnection {
    pub(crate) fn signal_target(&self) -> SignalTarget {
        SignalTarget::new(
            self.child.as_ref(),
            self.fresh
                .as_ref()
                .map(|fresh| &fresh.profile)
                .or(self.temporary_profile.as_ref()),
        )
    }
}
/// Fresh Chrome kept by a service until its remote answer tab is retired after acknowledgement.
static DEFERRED_FRESH: std::sync::Mutex<Vec<(String, ChromeConnection)>> =
    std::sync::Mutex::new(Vec::new());
pub(crate) fn defer_fresh(session: &str, connection: ChromeConnection) {
    DEFERRED_FRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((session.into(), connection));
}
/// Takes deferred connections for one session, or all of them at service shutdown.
pub(crate) fn take_deferred_fresh(session: Option<&str>) -> Vec<(String, ChromeConnection)> {
    let mut pending = DEFERRED_FRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (taken, kept) = std::mem::take(&mut *pending)
        .into_iter()
        .partition(|(id, _)| session.is_none_or(|session| session == id));
    *pending = kept;
    taken
}
/// Chrome's own UI and component/provider workers hold no user tab. Every page-like
/// target, and any other origin, still blocks fresh disposal.
fn browser_internal(row: &Value) -> bool {
    let url = row["url"].as_str().unwrap_or("");
    let internal = url.starts_with("chrome://") || url.starts_with("chrome-extension://");
    match row["type"].as_str() {
        Some("browser_ui") => internal,
        Some("background_page") => url.starts_with("chrome-extension://"),
        Some("service_worker" | "shared_worker" | "worker") => {
            internal || crate::browser::provider_url(url).is_some()
        }
        _ => false,
    }
}
/// Startup pages of a fresh launch are the only pages its disposal may treat as Alcuin's own.
async fn startup_pages(browser: &Cdp, timeout: Duration) -> std::collections::BTreeSet<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if let Ok(rows) = browser
            .call(
                "Target.getTargets",
                serde_json::json!({}),
                remaining,
                &CancellationToken::new(),
            )
            .await
        {
            let pages = rows["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| row["type"] == "page" && row["url"] == "about:blank")
                .filter_map(|row| row["targetId"].as_str().map(str::to_owned))
                .collect::<std::collections::BTreeSet<_>>();
            if !pages.is_empty() {
                return pages;
            }
        }
        if std::time::Instant::now() + Duration::from_millis(50) >= deadline {
            return Default::default();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn reap_persistent(mut child: Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => tokio::time::sleep(Duration::from_millis(250)).await,
            Err(error) => {
                eprintln!("Persistent Chrome child reap failed: {error}");
                return;
            }
        }
    }
}
#[cfg(target_os = "linux")]
fn native_executable(pid: u32) -> Result<PathBuf> {
    Ok(fs::read_link(format!("/proc/{pid}/exe"))?)
}
#[cfg(windows)]
fn native_executable(pid: u32) -> Result<PathBuf> {
    use std::os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    anyhow::ensure!(
        !handle.is_null(),
        "Health Chrome image handle is unavailable"
    );
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    anyhow::ensure!(
        unsafe {
            QueryFullProcessImageNameW(handle.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut length)
        } != 0,
        "Health Chrome executable is unavailable"
    );
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length as usize],
    )))
}
#[cfg(not(any(windows, target_os = "linux")))]
fn native_executable(_: u32) -> Result<PathBuf> {
    bail!("Native Chrome image verification is unqualified on this platform")
}
/// The reference WSL route (Windows Chrome reached through the WSL host address) is not
/// ported: refuse an owned launch under WSL, detected as the reference isWsl does.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn refuse_wsl(distro: Option<&std::ffi::OsStr>, release: &str) -> Result<()> {
    anyhow::ensure!(
        distro.is_none_or(|name| name.is_empty()) && !release.to_lowercase().contains("microsoft"),
        "Launching Chrome under WSL: this build does not support it. Run Alcuin on the Windows host instead."
    );
    Ok(())
}
/// The reference's `isManualLoginProfileInitialized`: Chrome has created a profile or Local State.
fn manual_login_initialised(profile: &Path) -> bool {
    fs::read_dir(profile).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "Default" || name == "Local State" || name.starts_with("Profile ")
        })
    })
}
/// The reference's `assertManualLoginProfileReadyForRun` refusal, with its setup command.
fn manual_login_not_initialised(profile: &Path) -> String {
    format!(
        "ChatGPT browser manual-login profile is not initialized. Browser mode is using Alcuin's private Chrome profile at {}, separate from your normal Chrome profile. Run first-time setup, sign in there, then retry: {}. If you want to reuse an already signed-in Chrome instead, use --browser-attach-running.",
        profile.display(),
        manual_login_setup_command(profile)
    )
}
/// The reference's `formatManualLoginSetupCommand`.
pub(crate) fn manual_login_setup_command(profile: &Path) -> String {
    format!(
        "alcuin --engine browser --browser-manual-login --browser-keep-browser --browser-manual-login-profile-dir {} -p \"HI\"",
        serde_json::Value::String(profile.display().to_string())
    )
}
/// The reference passes Chrome its whole environment. Chrome receives the variables it needs for
/// its display, locale, profile folders and desktop session, never Alcuin's credentials.
fn chrome_environment(environment: &Environment) -> impl Iterator<Item = (&String, &String)> {
    environment.iter().filter(|(key, _)| {
        matches!(
            key.as_str(),
            "HOME"
                | "USERPROFILE"
                | "PATH"
                | "SystemRoot"
                | "TEMP"
                | "TMP"
                | "LOCALAPPDATA"
                | "APPDATA"
                | "LANG"
                | "LC_ALL"
                | "DISPLAY"
                | "XAUTHORITY"
                | "WAYLAND_DISPLAY"
                | "XDG_RUNTIME_DIR"
                | "XDG_CONFIG_HOME"
                | "DBUS_SESSION_BUS_ADDRESS"
        )
    })
}
/// chrome-launcher 1.2.2 `DEFAULT_FLAGS`, which the reference launches with, in its order.
const LAUNCHER_DEFAULT_FLAGS: [&str; 23] = [
    "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,CalculateNativeWinOcclusion,InterestFeedContentSuggestions,CertificateTransparencyComponentUpdater,AutofillServerCommunication,PrivacySandboxSettings4,RenderDocument",
    "--disable-extensions",
    "--disable-component-extensions-with-background-pages",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-client-side-phishing-detection",
    "--disable-sync",
    "--metrics-recording-only",
    "--disable-default-apps",
    "--mute-audio",
    "--no-default-browser-check",
    "--no-first-run",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-background-timer-throttling",
    "--disable-ipc-flooding-protection",
    "--password-store=basic",
    "--use-mock-keychain",
    "--force-fieldtrials=*BackgroundTracing/default/",
    "--disable-hang-monitor",
    "--disable-prompt-on-repost",
    "--disable-domain-reliability",
    "--propagate-iph-for-testing",
];
/// The reference's effective launch arguments: the launcher defaults, then Alcuin's own flags,
/// whose later `--disable-features` wins. A copied profile keeps its source's real keychain, so
/// both keychain overrides are dropped and the launcher adds no Linux setuid flag.
pub fn chrome_flags(
    debug_port: Option<u16>,
    headless: bool,
    hide_window: bool,
    copied_profile: bool,
    no_sandbox: bool,
) -> Vec<String> {
    let keychain =
        |flag: &&str| *flag != "--password-store=basic" && *flag != "--use-mock-keychain";
    let mut flags: Vec<String> = LAUNCHER_DEFAULT_FLAGS
        .iter()
        .filter(|flag| !copied_profile || keychain(flag))
        .map(|flag| (*flag).to_owned())
        .collect();
    if cfg!(target_os = "linux") && !copied_profile {
        flags.push("--disable-setuid-sandbox".into());
    }
    flags.extend(
        [
            "--disable-background-networking",
            "--disable-background-timer-throttling",
            "--disable-breakpad",
            "--disable-client-side-phishing-detection",
            "--disable-default-apps",
            "--disable-hang-monitor",
            "--disable-popup-blocking",
            "--disable-prompt-on-repost",
            "--disable-sync",
            "--disable-translate",
            "--metrics-recording-only",
            "--no-first-run",
            "--safebrowsing-disable-auto-update",
            "--disable-features=TranslateUI,AutomationControlled",
            "--mute-audio",
            "--window-size=1280,720",
            "--lang=en-US",
            "--accept-lang=en-US,en",
            // Software compositing cut the GPU process from 197-307 MB to 65-136 MB on live
            // logged-out ChatGPT and Grok pages (ram-live-2026-10-24); WebGL stays on the GPU.
            "--disable-gpu-compositing",
        ]
        .map(str::to_owned),
    );
    if cfg!(target_os = "linux") && !copied_profile {
        flags.extend([
            "--password-store=basic".into(),
            "--use-mock-keychain".into(),
        ]);
    }
    flags.push(format!(
        "--remote-debugging-port={}",
        debug_port.unwrap_or(0)
    ));
    if headless {
        flags.push("--headless=new".into());
    } else if hide_window && cfg!(target_os = "macos") {
        flags.push("--window-position=-32000,-32000".into());
    }
    // Opt-in only, for container and CI Chromium that cannot use the sandbox.
    if no_sandbox {
        flags.extend(["--no-sandbox".into(), "--disable-dev-shm-usage".into()]);
    }
    flags
}
/// Chrome on Windows cannot read its Local State key through a verbatim `\\?\` profile path, so
/// copied cookies would not decrypt. It receives the ordinary spelling of the same directory;
/// occupancy scans compare filesystem identity, not spelling.
fn user_data_dir_arg(profile: &Path) -> Result<String> {
    Ok(format!(
        "--user-data-dir={}",
        crate::native_fs::canonical_wire_path(profile)?.display()
    ))
}
/// Ends an owned child and waits for it, bounded by `timeout`.
fn end_child(child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    while child.try_wait()?.is_none() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Owned Chrome did not exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
/// Settles the profile of a fresh launch that failed. Without a launch record no process was
/// recorded; otherwise the profile goes only when its exact recorded process is dead and a
/// complete occupancy scan finds no owner. Anything uncertain keeps the profile.
fn dispose_failed_fresh(mut fresh: TemporaryProfile) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let path = fresh.path().to_owned();
    let disposed = match read_record(&path.join("oracle-chrome-native.json")) {
        Ok(None) => fresh.cleanup_until(deadline).is_ok(),
        Ok(Some(raw)) => {
            serde_json::from_value::<LaunchRecord>(raw)
                .is_ok_and(|record| record.process_identity.liveness() == Liveness::Dead)
                && loop {
                    // Exiting processes can make one scan incomplete; only an empty one counts.
                    if profile_processes(&path).is_ok_and(|owners| owners.is_empty()) {
                        break true;
                    }
                    if std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                && fresh.cleanup_until(deadline).is_ok()
        }
        Err(_) => false,
    };
    if !disposed {
        eprintln!("Failed fresh Chrome launch left an unproven profile; retained");
    }
    disposed
}
fn read_record(path: &Path) -> Result<Option<Value>> {
    let mut entry = match NativeEntry::open(path, false) {
        Ok(entry) => entry,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    anyhow::ensure!(
        entry.handle.metadata()?.len() <= 64 * 1024,
        "Chrome control record exceeds its limit"
    );
    let mut bytes = vec![];
    entry
        .handle
        .by_ref()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 64 * 1024,
        "Chrome control record exceeds its limit"
    );
    entry.require_matches(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}
pub fn read_devtools_endpoint(profile: &Path) -> Result<Option<AttachmentEndpoint>> {
    let directory = NativeEntry::open(profile, true)?;
    for path in [
        profile.join("DevToolsActivePort"),
        profile.join("Default/DevToolsActivePort"),
    ] {
        let mut entry = match NativeEntry::open(&path, false) {
            Ok(e) => e,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut raw = String::new();
        entry.handle.by_ref().take(4097).read_to_string(&mut raw)?;
        anyhow::ensure!(
            raw.len() <= 4096,
            "Chrome debugging record exceeds its limit"
        );
        entry.require_matches(&path)?;
        directory.require_matches(profile)?;
        let mut lines = raw.lines();
        let port: u16 = lines
            .next()
            .context("Missing Chrome debugging port")?
            .trim()
            .parse()?;
        let browser_path = lines
            .next()
            .context("Missing exact Chrome browser socket identity")?
            .trim();
        let endpoint = AttachmentEndpoint {
            host: "127.0.0.1".into(),
            port,
            browser_ws_endpoint: format!("ws://127.0.0.1:{port}{browser_path}"),
        };
        endpoint.validate()?;
        return Ok(Some(endpoint));
    }
    Ok(None)
}
const CHROME_BASENAMES: &[&str] = &[
    "chrome",
    "chrome-wrapper",
    "chrome.exe",
    "chromium",
    "chromium.exe",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome-headless-shell",
    "chrome-headless-shell.exe",
];
fn chrome_basename(name: &str) -> bool {
    CHROME_BASENAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}
#[cfg(target_os = "linux")]
fn denied_profile_candidate(
    pid: u32,
    comm: std::io::Result<Vec<u8>>,
    disappeared: impl FnOnce() -> std::io::Result<bool>,
) -> Result<()> {
    let raw = match comm {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && disappeared()? => {
            return Ok(());
        }
        Err(error) => {
            return Err(error).context(format!(
                "Profile occupancy is unknown for native process {pid}"
            ));
        }
    };
    let name = raw
        .strip_suffix(b"\n")
        .context("Native process name is incomplete; profile occupancy is unknown")?;
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 127
            && name
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' '),
        "Native process name is malformed; profile occupancy is unknown"
    );
    anyhow::ensure!(
        !CHROME_BASENAMES
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate.as_bytes())
                || name.eq_ignore_ascii_case(&candidate.as_bytes()[..candidate.len().min(15)])),
        "Profile occupancy is unknown for native process {pid}"
    );
    // shortcut: inaccessible browsers with arbitrarily renamed comm are unsupported; use an explicit borrowed endpoint.
    Ok(())
}
// Native argv avoids flattened shell quoting and never reads profile contents.
#[cfg(target_os = "linux")]
fn profile_processes(profile: &Path) -> Result<Vec<u32>> {
    profile_processes_at(profile, Path::new("/proc"))
}
#[cfg(target_os = "linux")]
fn profile_processes_at(profile: &Path, proc_root: &Path) -> Result<Vec<u32>> {
    let mut owners = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let executable = match fs::read_link(entry.path().join("exe")) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let comm = (|| {
                    let mut raw = vec![];
                    fs::File::open(entry.path().join("comm"))?
                        // Procfs also exposes extended kernel-worker names.
                        .take(129)
                        .read_to_end(&mut raw)?;
                    Ok(raw)
                })();
                denied_profile_candidate(pid, comm, || {
                    entry.path().try_exists().map(|exists| !exists)
                })?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let name = executable.file_name().unwrap_or_default().to_string_lossy();
        if !chrome_basename(&name) {
            continue;
        }
        let raw = match fs::read(entry.path().join("cmdline")) {
            Ok(raw) => raw,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && !entry.path().try_exists()? =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let args: Vec<_> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
        if args.iter().any(|a| a.starts_with(b"--type=")) {
            continue;
        }
        for (index, arg) in args.iter().enumerate() {
            let value = arg.strip_prefix(b"--user-data-dir=").or_else(|| {
                (*arg == b"--user-data-dir")
                    .then(|| args.get(index + 1).copied())
                    .flatten()
            });
            if let Some(value) = value {
                use std::os::unix::ffi::OsStrExt;
                let path = Path::new(std::ffi::OsStr::from_bytes(value));
                if fs::canonicalize(path)? == profile {
                    owners.push(pid);
                }
            }
        }
    }
    Ok(owners)
}
#[cfg(windows)]
fn profile_processes(profile: &Path) -> Result<Vec<u32>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    // SAFETY: the snapshot handle and entry buffer remain valid and the handle is closed below.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let mut record: PROCESSENTRY32W = std::mem::zeroed();
            record.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut record);
            let mut owners = vec![];
            while more != 0 {
                let length = record
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(record.szExeFile.len());
                let name = String::from_utf16(&record.szExeFile[..length])?.to_lowercase();
                if chrome_basename(&name)
                    && let Some(command) = windows_process_command(record.th32ProcessID)?
                {
                    let args = windows_arguments(&command)?;
                    if !args.iter().any(|arg| arg.starts_with("--type=")) {
                        for (index, arg) in args.iter().enumerate() {
                            let directory = arg.strip_prefix("--user-data-dir=").or_else(|| {
                                (arg == "--user-data-dir")
                                    .then(|| args.get(index + 1).map(String::as_str))
                                    .flatten()
                            });
                            if let Some(directory) = directory {
                                let entry = NativeEntry::open(Path::new(directory), true)?;
                                if entry.matches(profile) {
                                    owners.push(record.th32ProcessID);
                                }
                            }
                        }
                    }
                }
                more = Process32NextW(snapshot, &mut record);
            }
            let error = std::io::Error::last_os_error();
            anyhow::ensure!(
                error.raw_os_error() == Some(18),
                "Native process enumeration is incomplete; profile occupancy is unknown"
            );
            Ok(owners)
        })();
        CloseHandle(snapshot);
        result
    }
}
#[cfg(all(windows, test))]
type CommandQuery = Box<dyn FnOnce() -> Result<Option<String>>>;
#[cfg(all(windows, test))]
thread_local! {
    static TEST_COMMAND_QUERY: std::cell::RefCell<Option<CommandQuery>> = const { std::cell::RefCell::new(None) };
}
#[cfg(windows)]
fn windows_process_command(pid: u32) -> Result<Option<String>> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            handle: *mut std::ffi::c_void,
            class: u32,
            buffer: *mut std::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    // ProcessCommandLineInformation returns a counted UNICODE_STRING in the caller buffer.
    // SAFETY: every returned pointer is range-checked before constructing a slice. The process
    // handle is closed on all paths; no foreign memory is dereferenced directly.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let result = (|| {
            #[cfg(test)]
            if let Some(query) = TEST_COMMAND_QUERY.with(|slot| slot.borrow_mut().take()) {
                return query();
            }
            let mut size = 0u32;
            let status = NtQueryInformationProcess(handle, 60, std::ptr::null_mut(), 0, &mut size);
            anyhow::ensure!(
                status == 0xc0000004u32 as i32
                    && size >= std::mem::size_of::<UnicodeString>() as u32
                    && size <= 1024 * 1024,
                "Native Chrome command identity is unavailable (status {status:#x}, bytes {size})"
            );
            let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let status =
                NtQueryInformationProcess(handle, 60, buffer.as_mut_ptr().cast(), size, &mut size);
            anyhow::ensure!(status >= 0, "Native Chrome command identity query failed");
            let value = &*buffer.as_ptr().cast::<UnicodeString>();
            let start = buffer.as_ptr() as usize;
            let end = start + buffer.len() * std::mem::size_of::<usize>();
            let pointer = value.buffer as usize;
            anyhow::ensure!(
                value.length % 2 == 0
                    && value.maximum_length >= value.length
                    && pointer >= start
                    && pointer.is_multiple_of(std::mem::align_of::<u16>())
                    && pointer
                        .checked_add(value.length as usize)
                        .is_some_and(|end_pointer| end_pointer <= end),
                "Native Chrome command identity buffer is malformed"
            );
            Ok(Some(String::from_utf16(std::slice::from_raw_parts(
                value.buffer,
                value.length as usize / 2,
            ))?))
        })();
        // Toolhelp may retain an entry after that exact process has exited. The open
        // handle cannot refer to a recycled PID; only proven exit permits skipping it.
        let mut exit_code = 259u32;
        let exited = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code != 259;
        CloseHandle(handle);
        match result {
            Err(_) if exited => Ok(None),
            result => result,
        }
    }
}
#[cfg(any(windows, test))]
fn windows_arguments(command: &str) -> Result<Vec<String>> {
    let chars: Vec<_> = command.chars().collect();
    let mut index = 0;
    let mut args = vec![];
    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index == chars.len() {
            break;
        }
        let mut value = String::new();
        let mut quoted = false;
        while index < chars.len() {
            if !quoted && chars[index].is_whitespace() {
                break;
            }
            let mut slashes = 0;
            while index < chars.len() && chars[index] == '\\' {
                slashes += 1;
                index += 1;
            }
            if index < chars.len() && chars[index] == '"' {
                value.extend(std::iter::repeat_n('\\', slashes / 2));
                if slashes % 2 == 1 {
                    value.push('"');
                } else if quoted && chars.get(index + 1) == Some(&'"') {
                    value.push('"');
                    index += 1;
                } else {
                    quoted = !quoted;
                }
                index += 1;
            } else {
                value.extend(std::iter::repeat_n('\\', slashes));
                if index < chars.len() {
                    if !quoted && chars[index].is_whitespace() {
                        break;
                    }
                    value.push(chars[index]);
                    index += 1;
                }
            }
        }
        anyhow::ensure!(
            !quoted,
            "Native Chrome command has unmatched quoting; profile occupancy is unknown"
        );
        args.push(value);
    }
    Ok(args)
}
#[cfg(not(any(target_os = "linux", windows)))]
fn profile_processes(_profile: &Path) -> Result<Vec<u32>> {
    bail!(
        "Native Chrome profile occupancy enumeration is unqualified on this platform; launch remains gated"
    )
}

/// Reference shouldPreferSystemTmpDir: on Linux a temp base inside a home dot-directory
/// (such as a snap's ~/.cache tmp) gives way to /tmp.
fn prefer_system_tmp(linux: bool, tmp: &Path, home: &Path) -> bool {
    linux
        && !home.as_os_str().is_empty()
        && tmp.strip_prefix(home).is_ok_and(|relative| {
            relative
                .components()
                .next()
                .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.'))
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_dot_directory_temp_bases_prefer_the_system_tmp() {
        let home = Path::new("/home/public");
        assert!(prefer_system_tmp(true, &home.join(".cache/tmp"), home));
        assert!(!prefer_system_tmp(false, &home.join(".cache/tmp"), home));
        assert!(!prefer_system_tmp(true, &home.join("tmp"), home));
        assert!(!prefer_system_tmp(true, home, home));
        assert!(!prefer_system_tmp(true, Path::new("/tmp"), home));
    }
    #[tokio::test]
    async fn copied_attempt_conversion_survives_cancellation_and_connection_drop() {
        use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
        for stage in ["unused", "converted", "cancelled-during-inventory"] {
            let home = tempfile::tempdir().unwrap();
            let profile = TemporaryProfile::create(home.path()).unwrap();
            let path = profile.path().to_owned();
            let mut owned = DisposableLaunch::new(Some(profile));
            #[cfg(unix)]
            let mut command = {
                let mut command = Command::new("/bin/sleep");
                command.arg("30");
                command
            };
            #[cfg(windows)]
            let mut command = {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "browser_chrome::tests::copied_job_descendant_fixture",
                        "--ignored",
                    ])
                    .env("ALCUIN_PUBLIC_COPY_FIXTURE", &path)
                    .env("ALCUIN_PUBLIC_COPY_LEAF", "1");
                command
            };
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let identity = owned.spawn(&mut command).unwrap();
            let (client, _server) = tokio::io::duplex(4096);
            let socket = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
            let mut connection = ChromeConnection {
                endpoint: AttachmentEndpoint {
                    host: "127.0.0.1".into(),
                    port: 1,
                    browser_ws_endpoint: "ws://127.0.0.1:1/devtools/browser/PUBLIC-copy".into(),
                },
                browser: Cdp::from_socket(socket, Duration::from_secs(1)),
                profile: None,
                child: owned.child.take(),
                identity: Some(identity.clone()),
                keep_browser: false,
                bootstrap_lock: None,
                bootstrap_admission: None,
                temporary_profile: owned.profile.take(),
                copied_group: owned.group.take(),
                #[cfg(windows)]
                copied_job: owned.job.take(),
                fresh: None,
                idle_startup: None,
                attachment: None,
            };
            if stage == "converted" {
                connection
                    .defer_copy_retirement(Duration::from_millis(20))
                    .await
                    .unwrap();
            } else if stage == "cancelled-during-inventory" {
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(5),
                        connection.defer_copy_retirement(Duration::from_secs(1))
                    )
                    .await
                    .is_err()
                );
            }
            if stage == "unused" {
                let _ = connection
                    .browser
                    .disconnect_and_wait(Duration::from_millis(50))
                    .await;
                drop(connection);
                assert_eq!(identity.liveness(), Liveness::Dead);
                assert!(!path.exists());
            } else {
                assert!(!connection.is_copied() && connection.is_fresh());
                assert!(connection.copied_group.is_none());
                #[cfg(windows)]
                assert!(connection.copied_job.is_none());
                assert!(!connection.dispose_copy().await.unwrap());
                // Retain the test's Child for bounded teardown after dropping the connection.
                // An armed Windows job would still kill it even with that handle retained.
                let mut child = connection.child.take().unwrap();
                let _ = connection
                    .browser
                    .disconnect_and_wait(Duration::from_millis(50))
                    .await;
                drop(connection);
                let retained = identity.liveness() == Liveness::Alive && path.exists();
                end_child(&mut child, Duration::from_secs(5)).unwrap();
                assert!(retained, "{stage} disposed a recoverable copied attempt");
            }
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn startup_contenders_leave_the_runtime_free_and_cancel_without_admission() {
        let cancel = CancellationToken::new();
        let held = startup_admission(Duration::from_secs(3), &cancel)
            .await
            .unwrap();
        let mut contenders = vec![];
        for _ in 0..4 {
            contenders.push(tokio::spawn(async {
                let admitted =
                    startup_admission(Duration::from_secs(3), &CancellationToken::new()).await?;
                admitted.release()
            }));
        }
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "Startup wait blocked the current-thread runtime"
        );
        assert!(contenders.iter().all(|task| !task.is_finished()));
        held.release().unwrap();
        for task in contenders {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        let held = startup_admission(Duration::from_secs(3), &cancel)
            .await
            .unwrap();
        let waiting_cancel = CancellationToken::new();
        let token = waiting_cancel.clone();
        let waiter =
            tokio::spawn(async move { startup_admission(Duration::from_secs(3), &token).await });
        tokio::time::sleep(Duration::from_millis(25)).await;
        waiting_cancel.cancel();
        let error = tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .unwrap()
            .unwrap()
            .err()
            .expect("Cancelled contender acquired startup authority");
        assert!(error.to_string().contains("cancelled"));
        held.release().unwrap();
    }
    #[test]
    fn owned_launch_refuses_wsl_like_the_reference_detection() {
        use std::ffi::OsStr;
        for (distro, release) in [
            (Some(OsStr::new("Ubuntu")), "6.6.0-generic"),
            (None, "5.15.167.4-microsoft-standard-WSL2"),
            (Some(OsStr::new("")), "4.4.0-19041-Microsoft"),
        ] {
            let error = refuse_wsl(distro, release).unwrap_err().to_string();
            assert_eq!(
                error,
                "Launching Chrome under WSL: this build does not support it. Run Alcuin on the Windows host instead."
            );
        }
        refuse_wsl(None, "6.6.0-generic").unwrap();
        refuse_wsl(Some(OsStr::new("")), "6.6.0-generic").unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn copied_profile_cleanup_waits_for_descendants_that_can_recreate_the_directory() {
        use std::os::unix::process::CommandExt;
        let home = tempfile::tempdir().unwrap();
        let mut profile = TemporaryProfile::create(home.path()).unwrap();
        let path = profile.path().to_owned();
        let mut command = Command::new("/bin/sh");
        command.env_clear().args(["-c", "(: > \"$1/PUBLIC-descendant-started\"; /bin/sleep 0.2; /bin/mkdir -p -- \"$1\"; : > \"$1/PUBLIC-late\") & exit 0", "PUBLIC-copy-child"]).arg(&path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: setsid only detaches this fresh synthetic child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.join("PUBLIC-descendant-started").exists()
            || child.try_wait().unwrap().is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "Synthetic descendant did not reach its late-write boundary"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        profile.launched();
        let mut owned = DisposableLaunch {
            child: Some(child),
            profile: Some(profile),
            group: Some(pid),
            attempted: false,
            kill_on_failure: false,
        };
        owned.cleanup().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !path.exists(),
            "A surviving copied-browser descendant recreated the deleted profile"
        );
        assert!(copied_group_absent(Some(pid)).unwrap());
        println!("PUBLIC copy descendant group={pid} absent");
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "Bounded synthetic child fixture invoked by copied_job_tracks_children_without_profile_arguments"]
    #[allow(clippy::zombie_processes)] // The owned Windows job, tested by its parent, reaps this intentional descendant.
    fn copied_job_descendant_fixture() {
        let Some(path) = std::env::var_os("ALCUIN_PUBLIC_COPY_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        if std::env::var_os("ALCUIN_PUBLIC_COPY_LEAF").is_some() {
            fs::write(path.join("PUBLIC-leaf-ready"), b"PUBLIC ready").unwrap();
            std::thread::sleep(Duration::from_secs(8));
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser_chrome::tests::copied_job_descendant_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("ALCUIN_PUBLIC_COPY_FIXTURE", &path)
            .env("ALCUIN_PUBLIC_COPY_LEAF", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.join("PUBLIC-leaf-ready").exists() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Synthetic copied-browser child failed to start");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fs::write(path.join("PUBLIC-leaf-pid"), child.id().to_string()).unwrap();
        // This fixture deliberately exits before its child, which has no profile argument.
    }
    #[cfg(windows)]
    #[test]
    fn copied_job_tracks_children_without_profile_arguments() {
        let home = tempfile::tempdir().unwrap();
        let profile = TemporaryProfile::create(home.path()).unwrap();
        let path = profile.path().to_owned();
        let mut owned = DisposableLaunch::new(Some(profile));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "browser_chrome::tests::copied_job_descendant_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("ALCUIN_PUBLIC_COPY_FIXTURE", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let parent = owned.spawn(&mut command).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while owned.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "Synthetic parent failed to exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let leaf = fs::read_to_string(path.join("PUBLIC-leaf-pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!(!owned.job.as_ref().unwrap().is_empty().unwrap());
        // Sources may have reaped the root already; job ownership must remain independent.
        owned.child = None;
        owned.cleanup().unwrap();
        assert!(owned.job.as_ref().unwrap().is_empty().unwrap());
        assert!(!path.exists());
        assert!(ProcessIdentity::read(leaf).unwrap().is_none());
        println!(
            "PUBLIC copied Windows parent={} leaf={leaf} absent",
            parent.pid
        );
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Uses only disposable synthetic profiles and installed Linux Chromium"]
    async fn copied_profile_reuses_synthetic_cookies_and_removes_only_its_owned_copy() {
        use crate::browser_target::TargetAcquisition;
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::Builder::new()
            .prefix("PUBLIC-copy-native-")
            .tempdir()
            .unwrap();
        let executable = home.path().join("chrome");
        fs::write(&executable,"#!/bin/sh\nexec /usr/lib/chromium/chromium '--host-resolver-rules=MAP * ~NOTFOUND' --password-store=basic --disable-component-update --renderer-process-limit=2 \"$@\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let copies = home.path().join("PUBLIC-copies");
        fs::create_dir(&copies).unwrap();
        let env = Environment::from([
            ("HOME".into(), home.path().to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TMPDIR".into(), copies.to_string_lossy().into_owned()),
        ]);
        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(10);
        let source_temp = TemporaryProfile::create(&home.path().join("PUBLIC-source")).unwrap();
        let source_path = source_temp.path().to_owned();
        let source_profile = ProfileRegistry::open(&source_path).unwrap();
        let mut source = ChromeConnection::launch(
            source_profile.clone(),
            ChromeLaunch {
                executable: executable.clone(),
                cwd: home.path().into(),
                environment: env.clone(),
                debug_port: None,
                timeout,
                keep_browser: false,
                headless: true,
                hide_window: false,
                keep_window_behind: false,
            },
            "about:blank",
            true,
            false,
            Some(CopiedLaunch {
                profile: source_temp,
                directory: "Default".into(),
            }),
            false,
            &cancel,
        )
        .await
        .unwrap();
        let source_identity = source.identity.clone().unwrap();
        println!("PUBLIC copy source pid={}", source_identity.pid);
        let source_lease = Arc::new(
            source_profile
                .acquire("PUBLIC-copy-source", timeout, &cancel)
                .await
                .unwrap(),
        );
        let source_target = TargetAcquisition::default()
            .acquire(source.browser.clone(), source_lease, &cancel, timeout)
            .await
            .unwrap();
        let stored = source_target.page().call("Network.setCookie",serde_json::json!({"name":"PUBLIC-copy-cookie","value":"PUBLIC-copy-cookie-value","url":"https://chatgpt.com/","httpOnly":true,"secure":true,"expires":2000000000}),timeout,&cancel).await.unwrap();
        assert_eq!(stored["success"], true);
        // SetCookie acknowledges the in-memory store; its SQLite commit is delayed.
        // Observe this unique public fixture row on disk before interrupting Chrome.
        let durable = Instant::now() + Duration::from_secs(40);
        loop {
            let persisted = [
                "Default/Cookies",
                "Default/Cookies-wal",
                "Default/Network/Cookies",
                "Default/Network/Cookies-wal",
            ]
            .into_iter()
            .any(|name| {
                fs::read(source_path.join(name)).is_ok_and(|bytes| {
                    bytes
                        .windows(b"PUBLIC-copy-cookie".len())
                        .any(|part| part == b"PUBLIC-copy-cookie")
                })
            });
            if persisted {
                break;
            }
            assert!(
                Instant::now() < durable,
                "Synthetic cookie was never committed to disk"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Stop and reap the owned source before copying its synthetic database.
        // SAFETY: the test retains this exact unreaped child; its PID cannot be reused.
        assert_eq!(
            unsafe { libc::kill(source_identity.pid as i32, libc::SIGTERM) },
            0
        );
        let stopped = Instant::now() + Duration::from_secs(5);
        while source.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < stopped,
                "Synthetic source Chrome did not stop gracefully"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        source.child.take().unwrap().wait().unwrap();
        while !copied_group_absent(Some(source_identity.pid)).unwrap() {
            assert!(
                Instant::now() < stopped,
                "Synthetic source Chrome descendants remain alive"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = source
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await;
        assert_eq!(source_identity.liveness(), Liveness::Dead);
        let database = source_path.join("Default/Cookies");
        let database = if database.exists() {
            database
        } else {
            source_path.join("Default/Network/Cookies")
        };
        let original = fs::read(&database).unwrap();
        let config = serde_json::json!({"manualLogin":false,"copyProfileSource":source_path,"chromeProfile":"Default","chromePath":executable,"headless":true,"cookieSync":false});
        for abandoned in [false, true] {
            let configured =
                ChromeConnection::configured(&config, &env, home.path(), timeout, &cancel)
                    .await
                    .unwrap();
            let copy_path = configured.profile.path().to_owned();
            assert!(copy_path.starts_with(&copies));
            let mut connection = configured.connection;
            let identity = connection.identity.clone().unwrap();
            println!(
                "PUBLIC copy owned pid={} abandoned={abandoned}",
                identity.pid
            );
            let lease = Arc::new(
                configured
                    .profile
                    .acquire("PUBLIC-copy-consumer", timeout, &cancel)
                    .await
                    .unwrap(),
            );
            let target = TargetAcquisition::default()
                .acquire(connection.browser.clone(), lease, &cancel, timeout)
                .await
                .unwrap();
            let cookies = target
                .page()
                .call(
                    "Network.getCookies",
                    serde_json::json!({"urls":["https://chatgpt.com/"]}),
                    timeout,
                    &cancel,
                )
                .await
                .unwrap();
            assert!(
                cookies["cookies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|cookie| cookie["name"] == "PUBLIC-copy-cookie"
                        && cookie["value"] == "PUBLIC-copy-cookie-value")
            );
            if abandoned {
                drop(connection);
            } else {
                connection
                    .browser
                    .disconnect_and_wait(Duration::from_secs(1))
                    .await
                    .unwrap();
                assert!(connection.dispose_copy().await.unwrap());
            }
            assert!(!copy_path.exists());
            assert_eq!(identity.liveness(), Liveness::Dead);
            assert_eq!(fs::read(&database).unwrap(), original);
        }
        let bad_directory = home.path().join("PUBLIC-bad-launch");
        fs::create_dir(&bad_directory).unwrap();
        let bad = bad_directory.join("chrome");
        fs::write(&bad, "#!/bin/sh\nexit 77\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o700)).unwrap();
        let mut failed = config.clone();
        failed["chromePath"] = serde_json::json!(bad);
        assert!(
            ChromeConnection::configured(&failed, &env, home.path(), timeout, &cancel)
                .await
                .is_err()
        );
        let remaining = fs::read_dir(&copies)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            remaining.is_empty(),
            "Disposable copy entries retained: {remaining:?}"
        );
        assert_eq!(fs::read(&database).unwrap(), original);
        source.dispose_copy().await.unwrap();
        assert!(!source_path.exists());
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Uses only a disposable synthetic profile and installed Linux Chromium"]
    async fn explicit_cookie_bootstrap_holds_the_original_launch_lock_until_seeding() {
        verify_cookie_bootstrap_profile(false).await;
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Uses only disposable home cookie files and installed Linux Chromium"]
    async fn home_cookie_bootstrap_holds_the_original_launch_lock_until_seeding() {
        verify_cookie_bootstrap_profile(true).await;
    }
    #[cfg(target_os = "linux")]
    async fn verify_cookie_bootstrap_profile(from_home: bool) {
        use crate::browser_target::TargetAcquisition;
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::Builder::new()
            .prefix("PUBLIC-cookie-native-")
            .tempdir()
            .unwrap();
        let executable = home.path().join("chrome");
        fs::write(&executable,"#!/bin/sh\nexec /usr/lib/chromium/chromium '--host-resolver-rules=MAP * ~NOTFOUND' --disable-component-update --renderer-process-limit=2 \"$@\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut env = Environment::from([
            ("HOME".into(), home.path().to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]);
        let mut config = serde_json::json!({"manualLogin":true,"manualLoginProfileDir":home.path().join("PUBLIC-profile"),"chromePath":executable,"headless":true});
        let raw = r#"[{"name":"PUBLIC-cookie","value":"PUBLIC-cookie-value","domain":".chatgpt.com","httpOnly":true}]"#;
        let input = if from_home {
            let oracle = home.path().join("PUBLIC-oracle");
            fs::create_dir(&oracle).unwrap();
            fs::write(oracle.join("cookies.json"), raw).unwrap();
            env.insert(
                "ORACLE_HOME_DIR".into(),
                oracle.to_string_lossy().into_owned(),
            );
            serde_json::json!({})
        } else {
            serde_json::json!({"browserInlineCookies":raw})
        };
        let batch = crate::browser_auth::prepare_config(&input, &mut config, &env, home.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            batch.requirement()["source"],
            if from_home {
                "home:cookies.json"
            } else {
                "inline-arg"
            }
        );
        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(10);
        let configured = ChromeConnection::configured(&config, &env, home.path(), timeout, &cancel)
            .await
            .unwrap();
        let mut connection = configured.connection;
        let identity = connection.identity.clone().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            anyhow::ensure!(
                connection.owns_process(),
                "Synthetic launch did not own its child"
            );
            let lock = connection
                .take_bootstrap_lock()
                .context("Bootstrap launch did not retain its original lock")?;
            let startup = connection
                .take_bootstrap_admission()
                .context("Bootstrap launch did not retain startup admission")?;
            anyhow::ensure!(
                DirectoryLock::acquire(
                    &configured
                        .profile
                        .path()
                        .join("oracle-browser-bringup.lock"),
                    Duration::from_millis(20)
                )
                .is_err(),
                "Competing controller entered during bootstrap"
            );
            anyhow::ensure!(
                connection
                    .endpoint
                    .connect(Duration::from_millis(20), &cancel)
                    .await
                    .is_err(),
                "Borrowed endpoint bypassed bootstrap using a different profile registry"
            );
            let lease = Arc::new(
                configured
                    .profile
                    .acquire_with_limit("PUBLIC-cookie-native", 2, timeout, &cancel)
                    .await?,
            );
            let acquisition = TargetAcquisition::default();
            let mut target = acquisition
                .acquire(connection.browser.clone(), lease, &cancel, timeout)
                .await?;
            anyhow::ensure!(
                target
                    .page()
                    .evaluate("location.href", timeout, &cancel)
                    .await?
                    == "about:blank",
                "Cookie target navigated before bootstrap"
            );
            anyhow::ensure!(
                batch
                    .apply(target.page(), "https://chatgpt.com/", timeout, &cancel)
                    .await?
                    == 1,
                "Native cookie was not applied"
            );
            let cookies = target
                .page()
                .call(
                    "Network.getCookies",
                    serde_json::json!({"urls":["https://chatgpt.com/"]}),
                    timeout,
                    &cancel,
                )
                .await?;
            anyhow::ensure!(
                cookies["cookies"]
                    .as_array()
                    .is_some_and(|cookies| cookies
                        .iter()
                        .any(|cookie| cookie["name"] == "PUBLIC-cookie"
                            && cookie["value"] == "PUBLIC-cookie-value"
                            && cookie["httpOnly"] == true)),
                "Native Chromium did not store the exact synthetic cookie"
            );
            anyhow::ensure!(
                target
                    .page()
                    .evaluate("location.href", timeout, &cancel)
                    .await?
                    == "about:blank",
                "Cookie bootstrap navigated the blank target"
            );
            target
                .page()
                .evaluate(
                    "document.body.textContent='PUBLIC retained blank'",
                    timeout,
                    &cancel,
                )
                .await?;
            anyhow::ensure!(
                target.retire_unclaimed_blank(timeout).await.is_err(),
                "Changed blank target was retired"
            );
            target.require(timeout, &cancel).await?;
            target.lease().require()?;
            target
                .page()
                .evaluate("document.body.textContent=''", timeout, &cancel)
                .await?;
            target.retire_unclaimed_blank(timeout).await?;
            lock.release()?;
            startup.release()?;
            let reused =
                ChromeConnection::configured(&config, &env, home.path(), timeout, &cancel).await;
            anyhow::ensure!(reused.is_err(), "Cookie bootstrap reused a live profile");
            Ok::<_, anyhow::Error>(())
        })
        .await;
        connection
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        if let Some(mut child) = connection.child.take() {
            // SAFETY: this unreaped fixture-owned child reserves the isolated process-group ID.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            child.wait().unwrap();
        }
        assert_eq!(identity.liveness(), Liveness::Dead);
        eprintln!("PUBLIC-cookie-owned-Chromium-{}=dead", identity.pid);
        result.unwrap().unwrap();
    }
    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    #[ignore = "Uses only disposable synthetic profiles and installed Chrome or Linux Chromium"]
    async fn fresh_ordinary_profile_is_seeded_privately_and_disposed_only_when_untouched() {
        use crate::browser_target::TargetAcquisition;
        use futures_util::FutureExt;
        let home = tempfile::Builder::new()
            .prefix("PUBLIC-fresh-native-")
            .tempdir()
            .unwrap();
        let (source, mut env) = crate::browser_cookie_loader::synthetic_source(home.path());
        #[cfg(windows)]
        let executable = {
            // The installed Chrome binary runs only against this disposable profile. Chrome
            // needs the real USERPROFILE to prove that profile is not its default directory.
            for key in [
                "SystemRoot",
                "PATH",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "USERPROFILE",
            ] {
                if let Ok(value) = std::env::var(key) {
                    env.insert(key.into(), value);
                }
            }
            crate::browser_detect::chrome(&env).expect("installed Chrome")
        };
        #[cfg(target_os = "linux")]
        let executable = home.path().join("chrome");
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&executable,"#!/bin/sh\nexec /usr/lib/chromium/chromium '--host-resolver-rules=MAP * ~NOTFOUND' --password-store=basic --disable-component-update --renderer-process-limit=2 \"$@\"\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            env.insert("HOME".into(), home.path().to_string_lossy().into_owned());
            env.insert("PATH".into(), "/usr/bin:/bin".into());
            env.insert(
                "DISPLAY".into(),
                std::env::var("DISPLAY").expect(
                    "Run the headed native disposal fixture under a disposable Xvfb display",
                ),
            );
        }
        let base = home.path().join("PUBLIC-snapshots");
        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(10);
        for case in [
            "keep",
            "foreign",
            "pre-prompt",
            "pre-prompt-marker",
            "clean",
            "late",
            "exit-retry",
            "partial-retry",
            "unreadable",
            "survivor",
            "headless",
            "missing-capability",
        ] {
            let config = serde_json::json!({"manualLogin":false,"cookieSync":true,"chromeCookiePath":source,"cookieNames":["PUBLIC-session"],"chromePath":executable,"headless":case=="headless","keepBrowser":case=="keep"});
            let batch = crate::browser_auth::read_ordinary(&config, &env, home.path(), &cancel)
                .await
                .unwrap();
            let mut attempts = 0;
            let configured = loop {
                match ChromeConnection::configured(&config, &env, home.path(), timeout, &cancel)
                    .await
                {
                    // The occupancy scan refuses while the previous case's renderers exit.
                    Err(error)
                        if attempts < 10
                            && error
                                .to_string()
                                .contains("command identity is unavailable") =>
                    {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    result => break result.unwrap(),
                }
            };
            let mut connection = configured.connection;
            let registry = configured.profile;
            let identity = connection.identity.clone().unwrap();
            let path = registry.path().to_owned();
            let disposal_store =
                crate::sessions::SessionStore::new(home.path().join("PUBLIC-disposal-state"));
            let result = tokio::time::timeout(Duration::from_secs(30), std::panic::AssertUnwindSafe(async {
                let disposal_id = if matches!(case, "late" | "clean" | "exit-retry" | "partial-retry" | "headless" | "missing-capability") {
                    Some(fresh_retirement::saved_disposal_fixture(&connection, &disposal_store))
                } else if case == "survivor" {
                    let meta = disposal_store.create(serde_json::json!({"prompt":"PUBLIC source","model":"gpt-6-pro","engine":"browser"}),home.path(),None)?;
                    Some(meta["id"].as_str().context("Missing live disposal fixture session")?.to_owned())
                } else { None };
                anyhow::ensure!(
                    connection.is_fresh() && connection.owns_process(),
                    "Ordinary launch did not own a fresh profile"
                );
                anyhow::ensure!(
                    path.starts_with(fs::canonicalize(&base)?)
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("alcuin-browser-")),
                    "Fresh profile is outside its private destination"
                );
                anyhow::ensure!(
                    !home.path().join(".oracle").exists(),
                    "Fresh run touched the persistent manual-login profile"
                );
                let lock = connection
                    .take_bootstrap_lock()
                    .context("Ordinary sync did not hold its bring-up lock until seeding")?;
                let startup = connection
                    .take_bootstrap_admission()
                    .context("Ordinary sync did not hold startup admission until seeding")?;
                let lease = Arc::new(
                    registry
                        .acquire_with_limit(if case=="survivor" {disposal_id.as_deref().unwrap()}else{"PUBLIC-fresh-native"}, 2, timeout, &cancel)
                        .await?,
                );
                let mut target = TargetAcquisition::default()
                    .acquire(connection.browser.clone(), lease, &cancel, timeout)
                    .await?;
                anyhow::ensure!(
                    batch
                        .apply(target.page(), "https://chatgpt.com/", timeout, &cancel)
                        .await?
                        == 1,
                    "Extracted cookie was not applied"
                );
                let cookies = target
                    .page()
                    .call(
                        "Network.getCookies",
                        serde_json::json!({"urls":["https://chatgpt.com/"]}),
                        timeout,
                        &cancel,
                    )
                    .await?;
                anyhow::ensure!(
                    cookies["cookies"].as_array().is_some_and(|cookies| cookies
                        .iter()
                        .any(|cookie| cookie["name"] == "PUBLIC-session"
                            && cookie["value"] == "PUBLIC-cookie-value")),
                    "Fresh Chromium did not store the extracted synthetic cookie"
                );
                lock.release()?;
                startup.release()?;
                if case == "survivor" {
                    fresh_retirement::retire_last_page_fixture(&connection,&disposal_store,disposal_id.as_deref().unwrap(),&target).await?;
                } else {
                    target.retire_unclaimed_blank(timeout).await?;
                }
                drop(target);
                let rows = connection
                    .browser
                    .call("Target.getTargets", serde_json::json!({}), timeout, &cancel)
                    .await?;
                eprintln!(
                    "PUBLIC-fresh-{case}-targets={} startup={:?}",
                    rows["targetInfos"],
                    connection
                        .fresh
                        .as_ref()
                        .map(|fresh| &fresh.startup_targets)
                );
                let blocker = if matches!(case, "foreign" | "late") {
                    Some(connection
                        .browser
                        .call(
                            "Target.createTarget",
                            serde_json::json!({"url":"about:blank"}),
                            timeout,
                            &cancel,
                        )
                        .await?["targetId"].as_str().context("Fixture target is missing")?.to_owned())
                } else { None };
                // Chrome may still be starting helpers, or the previous fixture's
                // processes may be exiting. Begin disposal from a complete scan.
                let scan_deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !profile_processes(&path).is_ok_and(|pids| pids == [identity.pid]) {
                    anyhow::ensure!(std::time::Instant::now() < scan_deadline,
                        "PUBLIC fixture process inventory never settled");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let settled = if case == "pre-prompt-marker" {
                    fresh_retirement::TEST_DISPOSAL_STOP.scope("after-pre-prompt-marker",
                        connection.settle_fresh(Duration::from_secs(5))).await
                } else if case == "unreadable" {
                    anyhow::ensure!(crate::browser_runner::settle_completed_fresh(
                        &disposal_store, "PUBLIC-missing-record", &mut connection).await
                        && identity.liveness() == Liveness::Alive
                        && path.join("oracle-chrome-native.json").exists(),
                        "Unreadable completion evidence entered the old deletion path");
                    Ok(())
                } else if matches!(case, "exit-retry" | "partial-retry") {
                    let id = disposal_id.as_deref().unwrap();
                    let stage = if case == "exit-retry" {"after-browser-exit"} else {"after-launch-record-removal"};
                    let routed = fresh_retirement::TEST_DISPOSAL_STOP.scope(stage,
                        crate::browser_runner::settle_completed_fresh(&disposal_store, id, &mut connection)).await;
                    anyhow::ensure!(routed && identity.liveness() == Liveness::Dead && path.exists(),
                        "Completed disposal did not reach its injected interruption");
                    let saved = disposal_store.stored_for_update(id)?;
                    anyhow::ensure!(saved["status"] == "completed"
                        && saved["browser"]["runtime"]["freshProfileDisposal"]["disposition"] == "retained"
                        && saved["browser"]["runtime"]["freshProfileDisposal"]["observedProcesses"].as_array().is_some_and(|rows| !rows.is_empty()),
                        "Interrupted disposal lost its completed answer or native exit evidence");
                    anyhow::ensure!(path.join("oracle-chrome-native.json").exists() == (case == "exit-retry")
                        && crate::browser_profile::fresh_disposal_marker(&path).unwrap().exists() == (case == "partial-retry"),
                        "Injected interruption did not exercise its durable marker boundary");
                    // No DEFERRED_FRESH entry remains, as after a worker restart.
                    crate::browser_runner::settle_deferred_fresh(&disposal_store, Some(id)).await;
                    let saved = disposal_store.stored_for_update(id)?;
                    anyhow::ensure!(saved["browser"]["runtime"]["freshProfileDisposal"]["disposition"] == "removed"
                        && !path.exists() && !crate::browser_profile::fresh_disposal_marker(&path).unwrap().exists(),
                        "Restarted deferred cleanup did not finish interrupted disposal");
                    Ok(())
                } else if let Some(id) = disposal_id.as_deref() {
                    if case == "missing-capability" {
                        disposal_store.update(id, |current| {
                            let mut browser=current["browser"].clone();
                            browser["runtime"].as_object_mut().unwrap().remove("freshProfileOwner");
                            Ok(Some(serde_json::json!({"browser":browser})))
                        })?;
                    }
                    anyhow::ensure!(crate::browser_runner::settle_completed_fresh(&disposal_store, id, &mut connection).await,
                        "Completed fresh launch bypassed durable disposal");
                    if case == "late" { Err(anyhow::anyhow!("PUBLIC retained target")) } else { Ok(()) }
                } else {
                    connection.settle_fresh(Duration::from_secs(5)).await
                };
                if case == "pre-prompt-marker" {
                    anyhow::ensure!(settled.is_err() && identity.liveness() == Liveness::Dead
                        && path.exists() && crate::browser_profile::fresh_disposal_marker(&path).unwrap().exists(),
                        "Interrupted pre-prompt disposal did not retain its disk barrier after native exit");
                    anyhow::ensure!(registry.acquire("PUBLIC-refused-after-disposal",Duration::from_millis(200),&cancel).await.is_err(),
                        "Interrupted disposal allowed a new profile admission");
                } else if case == "late" {
                    anyhow::ensure!(settled.is_err(), "First disposal must retain the unproven target");
                    let id = disposal_id.as_deref().unwrap();
                    anyhow::ensure!(retire_saved_fresh(&disposal_store, id, Duration::from_secs(3)).await.is_err()
                        && identity.liveness() == Liveness::Alive, "Recovery closed a browser with a retained target");
                    connection.browser.close_owned_target(blocker.as_deref().unwrap(), Duration::from_secs(1), &cancel).await?;
                    let absence_deadline = std::time::Instant::now() + timeout;
                    loop {
                        let inventory = connection.browser.call("Target.getTargets", serde_json::json!({}), timeout, &cancel).await?;
                        if inventory["targetInfos"].as_array().context("Missing fixture inventory")?.iter().all(|row|row["targetId"] != blocker.as_deref().unwrap()) { break; }
                        anyhow::ensure!(std::time::Instant::now() < absence_deadline, "Fixture blocker close was acknowledged but absence remains unconfirmed");
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    let lease = registry.acquire("PUBLIC-concurrent-admission", timeout, &cancel).await?;
                    anyhow::ensure!(retire_saved_fresh(&disposal_store, id, Duration::from_secs(3)).await.is_err()
                        && identity.liveness() == Liveness::Alive, "Recovery ignored concurrent admission");
                    lease.release()?;
                    let saved = disposal_store.stored_for_update(id)?;
                    for changed in ["keep", "profile", "process", "launch", "endpoint"] {
                        let mut value = saved.clone();
                        match changed {
                            "keep" => value["browser"]["config"]["keepBrowser"] = serde_json::json!(true),
                            "profile" => value["browser"]["runtime"]["freshProfileOwner"]["profile"] = serde_json::json!(home.path()),
                            "process" => value["browser"]["runtime"]["freshProfileOwner"]["processIdentity"] = serde_json::json!(ProcessIdentity::current()?),
                            "launch" => value["browser"]["runtime"]["freshProfileOwner"]["launchSha256"] = serde_json::json!("PUBLIC-changed"),
                            _ => value["browser"]["runtime"]["freshProfileOwner"]["endpoint"]["port"] = serde_json::json!(1),
                        }
                        disposal_store.update(id, |_| Ok(Some(value)))?;
                        anyhow::ensure!(retire_saved_fresh(&disposal_store, id, Duration::from_secs(3)).await.is_err()
                            && identity.liveness() == Liveness::Alive, "Recovery ignored changed {changed}");
                        disposal_store.update(id, |_| Ok(Some(saved.clone())))?;
                    }
                    let mut outcome = retire_saved_fresh(&disposal_store, id, Duration::from_secs(5)).await?;
                    if outcome.as_ref().is_some_and(|value| value["disposition"] != "removed") {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        outcome = retire_saved_fresh(&disposal_store, id, Duration::from_secs(5)).await?;
                    }
                    anyhow::ensure!(outcome.as_ref().is_some_and(|value| value["disposition"] == "removed")
                        && identity.liveness() == Liveness::Dead && !path.exists(),
                        "Later recovery did not reclaim fresh Chrome and its profile: {outcome:?}");
                } else if case == "headless" {
                    settled?;
                    let saved=disposal_store.stored_for_update(disposal_id.as_deref().unwrap())?;
                    let disposition=&saved["browser"]["runtime"]["freshProfileDisposal"]["disposition"];
                    anyhow::ensure!((identity.liveness()==Liveness::Dead && !path.exists() && disposition=="removed")
                        || (identity.liveness()==Liveness::Alive && path.exists() && disposition=="retained"),
                        "Headless natural exit was not observed or safely retained");
                    eprintln!("PUBLIC-headless-natural-exit-disposition={disposition}");
                } else if matches!(case, "clean" | "exit-retry" | "partial-retry" | "survivor" | "pre-prompt") {
                    settled?;
                    anyhow::ensure!(
                        identity.liveness() == Liveness::Dead && !path.exists(),
                        "Untouched fresh Chrome or profile survived disposal"
                    );
                } else {
                    anyhow::ensure!(
                        settled.is_err() == (case == "foreign")
                            && identity.liveness() == Liveness::Alive
                            && path.join("oracle-chrome-native.json").exists(),
                        "Kept or foreign-tab fresh Chrome was not retained with its profile"
                    );
                }
                Ok::<_, anyhow::Error>(())
            }).catch_unwind())
            .await.map(|caught| caught.unwrap_or_else(|_| Err(anyhow::anyhow!("PUBLIC disposal fixture panicked"))));
            eprintln!(
                "PUBLIC-fresh-{case}-before-teardown native={:?} passed={}",
                identity.liveness(),
                result.as_ref().is_ok_and(|result| result.is_ok())
            );
            let _ = connection
                .browser
                .disconnect_and_wait(Duration::from_secs(1))
                .await;
            if identity.liveness() == Liveness::Alive {
                // SAFETY: the fixture launched this isolated process group with setsid.
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(identity.pid as i32), libc::SIGKILL);
                }
                #[cfg(windows)]
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &identity.pid.to_string()])
                    .output();
            }
            if let Some(mut child) = connection.child.take() {
                let _ = child.wait();
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while identity.liveness() != Liveness::Dead && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(identity.liveness(), Liveness::Dead);
            eprintln!("PUBLIC-fresh-{case}-Chromium-{}=dead", identity.pid);
            result.unwrap().unwrap();
        }
    }
    /// A short-lived synthetic child, or a long-lived one when `linger` is set.
    pub(super) fn synthetic_child(home: &Path, linger: bool) -> Child {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sleep");
            command.arg(if linger { "30" } else { "0" });
            command
        };
        #[cfg(windows)]
        let mut command = {
            // The bounded copied-job leaf fixture sleeps for eight seconds after writing readiness.
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "browser_chrome::tests::copied_job_descendant_fixture",
                    "--ignored",
                ])
                .env("ALCUIN_PUBLIC_COPY_FIXTURE", home);
            if linger {
                command.env("ALCUIN_PUBLIC_COPY_LEAF", "1");
            }
            command
        };
        let _ = home;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    fn failed_fresh_profile(home: &Path, process: ProcessIdentity) -> TemporaryProfile {
        let mut fresh = TemporaryProfile::create(home).unwrap();
        fresh.launched();
        let executable = std::env::current_exe().unwrap();
        let record = LaunchRecord {
            version: 1,
            process_identity: process,
            profile_identity: NativeEntry::open(fresh.path(), true).unwrap().identity,
            executable_identity: NativeEntry::open(&executable, false).unwrap().identity,
            executable,
            headless: true,
            endpoint: None,
            extra: serde_json::Map::new(),
        };
        atomic_json(&fresh.path().join("oracle-chrome-native.json"), &record).unwrap();
        fresh
    }
    #[test]
    fn chrome_receives_an_ordinary_profile_path() {
        let home = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(home.path()).unwrap();
        let argument = user_data_dir_arg(&canonical).unwrap();
        let directory = argument.strip_prefix("--user-data-dir=").unwrap();
        assert!(!directory.starts_with(r"\\?\"), "{argument}");
        assert!(
            NativeEntry::open(Path::new(directory), true)
                .unwrap()
                .matches(&canonical)
        );
    }
    #[cfg(windows)]
    #[test]
    fn native_command_inventory_reads_live_child_and_skips_its_proven_exit() {
        let home = tempfile::tempdir().unwrap();
        let mut child = synthetic_child(home.path(), true);
        let live = windows_process_command(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(live.unwrap().is_some());
        assert_eq!(windows_process_command(child.id()).unwrap(), None);
    }
    #[cfg(windows)]
    #[test]
    fn failed_native_command_query_requires_exit_of_the_already_open_process() {
        let home = tempfile::tempdir().unwrap();
        let mut child = synthetic_child(home.path(), true);
        TEST_COMMAND_QUERY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|| bail!("PUBLIC injected native query failure")));
        });
        let live_error = windows_process_command(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            live_error.is_err(),
            "A live native handle was mistaken for exit"
        );

        let mut child = synthetic_child(home.path(), true);
        let pid = child.id();
        TEST_COMMAND_QUERY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                // The production function has opened its exact process handle.
                child.kill()?;
                child.wait()?;
                bail!("PUBLIC injected query failure after native exit")
            }));
        });
        assert_eq!(windows_process_command(pid).unwrap(), None);
    }
    #[test]
    fn failed_fresh_launch_disposes_only_a_profile_whose_recorded_process_is_dead() {
        let home = tempfile::tempdir().unwrap();
        let mut child = synthetic_child(home.path(), false);
        let identity = ProcessIdentity::read(child.id()).unwrap().unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while identity.liveness() != Liveness::Dead {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let dead = failed_fresh_profile(home.path(), identity);
        let path = dead.path().to_owned();
        assert!(dispose_failed_fresh(dead));
        assert!(!path.exists(), "Failed fresh profile survived disposal");
        // A live recorded process (this test) may still use the profile, so it is retained.
        let alive = failed_fresh_profile(home.path(), ProcessIdentity::current().unwrap());
        let path = alive.path().to_owned();
        assert!(!dispose_failed_fresh(alive));
        assert!(path.join("oracle-chrome-native.json").exists());
        // Without any launch record no process was recorded, so the empty profile goes.
        let mut unrecorded = TemporaryProfile::create(home.path()).unwrap();
        unrecorded.launched();
        let path = unrecorded.path().to_owned();
        assert!(dispose_failed_fresh(unrecorded));
        assert!(!path.exists());
    }
    #[test]
    fn failed_fresh_launch_ends_its_exact_child() {
        let home = tempfile::tempdir().unwrap();
        let mut child = synthetic_child(home.path(), true);
        #[cfg(windows)]
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !home.path().join("PUBLIC-leaf-ready").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "Synthetic child did not start"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let identity = ProcessIdentity::read(child.id()).unwrap().unwrap();
        assert_eq!(identity.liveness(), Liveness::Alive);
        let mut owned = DisposableLaunch::new(None);
        owned.kill_on_failure = true;
        owned.child = Some(child);
        drop(owned);
        assert_eq!(identity.liveness(), Liveness::Dead);
        // An ordinary persistent launch keeps its acknowledged child for recovery.
        child = synthetic_child(home.path(), true);
        let identity = ProcessIdentity::read(child.id()).unwrap().unwrap();
        let mut kept = DisposableLaunch::new(None);
        kept.child = Some(child);
        drop(kept);
        assert_eq!(identity.liveness(), Liveness::Alive);
        let mut child = Command::new(if cfg!(windows) { "taskkill" } else { "kill" });
        if cfg!(windows) {
            child.args(["/F", "/PID", &identity.pid.to_string()]);
        } else {
            child.args(["-9", &identity.pid.to_string()]);
        }
        child
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while identity.liveness() != Liveness::Dead {
            assert!(
                std::time::Instant::now() < deadline,
                "Synthetic kept child survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn fresh_disposal_tolerates_only_browser_internal_targets() {
        use serde_json::json;
        for (row, internal) in [
            (
                json!({"type":"browser_ui","url":"chrome://omnibox-popup.top-chrome/"}),
                true,
            ),
            (
                json!({"type":"background_page","url":"chrome-extension://PUBLIC/background.html"}),
                true,
            ),
            (
                json!({"type":"service_worker","url":"chrome-extension://PUBLIC/sw.js"}),
                true,
            ),
            (
                json!({"type":"service_worker","url":"https://chatgpt.com/PUBLIC-sw.js"}),
                true,
            ),
            (
                json!({"type":"service_worker","url":"https://example.com/PUBLIC-sw.js"}),
                false,
            ),
            (
                json!({"type":"browser_ui","url":"https://example.com/"}),
                false,
            ),
            (
                json!({"type":"background_page","url":"https://example.com/"}),
                false,
            ),
            (json!({"type":"page","url":"chrome://newtab/"}), false),
            (json!({"type":"page","url":"about:blank"}), false),
            (json!({"type":"webview","url":"about:blank"}), false),
            (json!({"type":"iframe","url":"https://chatgpt.com/"}), false),
            (
                json!({"type":"other","url":"devtools://devtools/PUBLIC"}),
                false,
            ),
            (json!({"url":"chrome://version/"}), false),
        ] {
            assert_eq!(browser_internal(&row), internal, "{row}");
        }
    }
    #[test]
    fn launch_and_occupancy_share_exact_case_insensitive_browser_names() {
        for name in CHROME_BASENAMES {
            assert!(chrome_basename(name));
            assert!(chrome_basename(&name.to_ascii_uppercase()));
            assert!(!chrome_basename(&format!("{name}-foreign")));
        }
        assert!(!chrome_basename("not-chrome"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn inaccessible_process_names_only_exclude_unrelated_named_candidates() {
        let unrelated = || Ok(b"systemd\n".to_vec());
        assert!(
            denied_profile_candidate(1, unrelated(), || panic!("No disappearance probe needed"))
                .is_ok()
        );
        assert!(
            denied_profile_candidate(1, Ok(b"pool_workqueue_release\n".to_vec()), || panic!(
                "No disappearance probe needed"
            ))
            .is_ok()
        );
        for name in CHROME_BASENAMES {
            let prefix = &name.as_bytes()[..name.len().min(15)];
            for value in [prefix.to_vec(), prefix.to_ascii_uppercase()] {
                let mut value = value;
                value.push(b'\n');
                assert!(
                    denied_profile_candidate(2, Ok(value), || panic!(
                        "Candidate must remain unknown"
                    ))
                    .is_err(),
                    "{name}"
                );
            }
            assert!(
                denied_profile_candidate(2, Ok(format!("{name}\n").into_bytes()), || panic!(
                    "Candidate must remain unknown"
                ))
                .is_err()
            );
        }
        // A renamed inaccessible browser has the same observations as an unrelated process.
        assert!(
            denied_profile_candidate(3, unrelated(), || panic!("No disappearance probe needed"))
                .is_ok()
        );
        for raw in [
            b"".as_slice(),
            b"\n",
            b"chrome",
            b"chrome\nextra\n",
            b"bad\0name\n",
            b"non-ascii-\xff\n",
        ] {
            assert!(denied_profile_candidate(4, Ok(raw.to_vec()), || Ok(false)).is_err());
        }
        let mut oversized = vec![b'x'; 128];
        oversized.push(b'\n');
        assert!(denied_profile_candidate(4, Ok(oversized), || Ok(false)).is_err());
        use std::io::{Error, ErrorKind};
        assert!(
            denied_profile_candidate(5, Err(Error::from(ErrorKind::PermissionDenied)), || panic!(
                "Denied metadata cannot establish disappearance"
            ))
            .is_err()
        );
        assert!(
            denied_profile_candidate(6, Err(Error::from(ErrorKind::NotFound)), || Ok(true)).is_ok()
        );
        assert!(
            denied_profile_candidate(6, Err(Error::from(ErrorKind::NotFound)), || Ok(false))
                .is_err()
        );
        assert!(
            denied_profile_candidate(6, Err(Error::from(ErrorKind::NotFound)), || Err(
                Error::from(ErrorKind::PermissionDenied)
            ))
            .is_err()
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn readable_browser_executables_use_exact_profile_arguments_without_comm() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let profile = home.path().join("PUBLIC-profile");
        fs::create_dir(&profile).unwrap();
        let profile = fs::canonicalize(profile).unwrap();
        let proc_root = home.path().join("PUBLIC-proc");
        fs::create_dir(&proc_root).unwrap();
        for (pid, name, args) in [
            (
                10,
                "CHROME.EXE",
                format!("chrome\0--user-data-dir={}\0", profile.display()),
            ),
            (
                11,
                "chromium-browser",
                format!(
                    "chromium\0--user-data-dir\0{}\0--type=renderer\0",
                    profile.display()
                ),
            ),
            (
                12,
                "systemd",
                "PUBLIC unrelated executable; no command read".into(),
            ),
        ] {
            let directory = proc_root.join(pid.to_string());
            fs::create_dir(&directory).unwrap();
            symlink(home.path().join(name), directory.join("exe")).unwrap();
            if pid != 12 {
                fs::write(directory.join("cmdline"), args).unwrap();
            }
            fs::write(directory.join("comm"), "systemd\n").unwrap();
        }
        assert_eq!(
            profile_processes_at(&profile, &proc_root).unwrap(),
            vec![10]
        );
        let directory = proc_root.join("13");
        fs::create_dir(&directory).unwrap();
        symlink(home.path().join("chrome"), directory.join("exe")).unwrap();
        assert!(
            profile_processes_at(&profile, &proc_root).is_err(),
            "Missing command metadata on a present candidate cannot establish absence"
        );
    }
    #[test]
    fn endpoint_identity_and_native_devtools_record_refuse_foreign_sockets() {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("DevToolsActivePort"),
            "9222\n/devtools/browser/PUBLIC\n",
        )
        .unwrap();
        let endpoint = read_devtools_endpoint(home.path()).unwrap().unwrap();
        assert_eq!(endpoint.port, 9222);
        for path in [
            "/devtools/page/PUBLIC",
            "/devtools/browser/",
            "/devtools/browser/PUBLIC?token=private",
            "/devtools/browser/PUBLIC/foreign",
        ] {
            fs::write(
                home.path().join("DevToolsActivePort"),
                format!("9222\n{path}"),
            )
            .unwrap();
            assert!(read_devtools_endpoint(home.path()).is_err());
        }
        let mut bad = endpoint.clone();
        bad.host = "foreign.invalid".into();
        assert!(bad.validate().is_err());
        bad = endpoint;
        bad.port = 9223;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn visible_launch_flags_do_not_disable_the_sandbox_or_request_headless() {
        let flags = chrome_flags(None, false, false, false, false);
        assert!(flags.contains(&"--remote-debugging-port=0".into()));
        assert!(
            !flags
                .iter()
                .any(|s| s.contains("headless") || s == "--no-sandbox")
        );
        assert!(flags.contains(&"--lang=en-US".into()));
        // Software compositing only: --disable-gpu would also move WebGL off the GPU.
        assert!(flags.contains(&"--disable-gpu-compositing".into()));
        assert!(!flags.contains(&"--disable-gpu".into()));
        let headless = chrome_flags(Some(9222), true, true, false, false);
        assert!(headless.contains(&"--disable-gpu-compositing".into()));
        assert!(headless.contains(&"--headless=new".into()));
        assert!(headless.contains(&"--remote-debugging-port=9222".into()));
        assert!(!headless.contains(&"--no-sandbox".into()));
        assert!(
            !headless
                .iter()
                .any(|flag| flag.starts_with("--window-position"))
        );
        let hidden = chrome_flags(None, false, true, false, false);
        assert_eq!(
            hidden.contains(&"--window-position=-32000,-32000".into()),
            cfg!(target_os = "macos")
        );
    }
    #[test]
    fn manual_login_profile_readiness_matches_the_reference() {
        let home = tempfile::tempdir().unwrap();
        assert!(!manual_login_initialised(home.path()));
        assert!(!manual_login_initialised(&home.path().join("missing")));
        fs::write(home.path().join("Profilex"), "").unwrap();
        assert!(!manual_login_initialised(home.path()));
        fs::create_dir(home.path().join("Profile 1")).unwrap();
        assert!(manual_login_initialised(home.path()));
        let profile = Path::new(if cfg!(windows) {
            r"C:\PUBLIC\profile dir"
        } else {
            "/PUBLIC/profile dir"
        });
        let quoted = if cfg!(windows) {
            r#""C:\\PUBLIC\\profile dir""#
        } else {
            r#""/PUBLIC/profile dir""#
        };
        assert_eq!(
            manual_login_not_initialised(profile),
            format!(
                "ChatGPT browser manual-login profile is not initialized. Browser mode is using Alcuin's private Chrome profile at {}, separate from your normal Chrome profile. Run first-time setup, sign in there, then retry: alcuin --engine browser --browser-manual-login --browser-keep-browser --browser-manual-login-profile-dir {quoted} -p \"HI\". If you want to reuse an already signed-in Chrome instead, use --browser-attach-running.",
                profile.display()
            )
        );
    }
    #[test]
    fn chrome_receives_its_session_variables_but_not_alcuin_credentials() {
        let environment: Environment = [
            "PATH",
            "XAUTHORITY",
            "LANG",
            "LC_ALL",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "OPENAI_API_KEY",
            "ORACLE_REMOTE_TOKEN",
            "ORACLE_CHROME_NO_SANDBOX",
        ]
        .map(|key| (key.to_owned(), "PUBLIC-value".to_owned()))
        .into();
        let forwarded: Vec<&str> = chrome_environment(&environment)
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(
            forwarded,
            [
                "APPDATA",
                "LANG",
                "LC_ALL",
                "LOCALAPPDATA",
                "PATH",
                "XAUTHORITY",
                "XDG_CONFIG_HOME"
            ]
        );
    }
    #[test]
    fn launch_flags_follow_the_reference_launcher_defaults_copy_and_sandbox_policy() {
        let flags = chrome_flags(None, false, false, false, false);
        // Launcher defaults come first, so Alcuin's own --disable-features is the effective one.
        assert_eq!(flags[..23], LAUNCHER_DEFAULT_FLAGS.map(str::to_owned));
        assert_eq!(
            flags
                .iter()
                .rfind(|flag| flag.starts_with("--disable-features="))
                .unwrap(),
            "--disable-features=TranslateUI,AutomationControlled"
        );
        for flag in [
            "--disable-renderer-backgrounding",
            "--disable-backgrounding-occluded-windows",
            "--disable-ipc-flooding-protection",
            "--no-default-browser-check",
            "--disable-component-update",
            "--disable-extensions",
            "--password-store=basic",
            "--use-mock-keychain",
        ] {
            assert!(flags.contains(&flag.into()), "{flag}");
        }
        assert_eq!(
            flags.contains(&"--disable-setuid-sandbox".into()),
            cfg!(target_os = "linux")
        );
        // A copied profile decrypts its source's cookies with the real keychain.
        let copied = chrome_flags(None, false, false, true, false);
        assert!(!copied.iter().any(|flag| flag == "--password-store=basic"
            || flag == "--use-mock-keychain"
            || flag == "--disable-setuid-sandbox"));
        assert!(copied.contains(&"--disable-extensions".into()));
        assert!(
            !flags
                .iter()
                .any(|flag| flag == "--no-sandbox" || flag == "--disable-dev-shm-usage")
        );
        let sandboxless = chrome_flags(None, true, false, false, true);
        assert!(sandboxless.ends_with(&["--no-sandbox".into(), "--disable-dev-shm-usage".into()]));
    }
    #[tokio::test]
    async fn cancellation_while_waiting_for_bringup_lock_prevents_a_new_launch() {
        let home = tempfile::tempdir().unwrap();
        let profile = ProfileRegistry::open(home.path()).unwrap();
        let lock = DirectoryLock::acquire(
            &profile.path().join("oracle-browser-bringup.lock"),
            Duration::from_secs(1),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let pending_cancel = cancel.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            pending_cancel.cancel();
            std::thread::sleep(Duration::from_millis(50));
            lock.release().unwrap();
        });
        let result = ChromeConnection::launch_manual_login(
            profile,
            ChromeLaunch {
                executable: home.path().join("PUBLIC-missing-chrome"),
                cwd: home.path().into(),
                environment: Environment::new(),
                debug_port: None,
                timeout: Duration::from_secs(2),
                keep_browser: true,
                headless: false,
                hide_window: false,
                keep_window_behind: false,
            },
            &cancel,
        )
        .await;
        release.join().unwrap();
        let error = result
            .err()
            .expect("Cancelled bring-up must not launch Chrome");
        assert!(cancel.is_cancelled());
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(!home.path().join("oracle-chrome-native.json").exists());
    }
    #[tokio::test]
    async fn persistent_child_is_reaped_after_it_exits() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--list")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let child = command.spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(5), reap_persistent(child))
            .await
            .unwrap();
    }
    #[test]
    fn windows_native_arguments_preserve_profile_spaces_and_quoted_switches() {
        for command in [
            r#""C:\Program Files\Chrome\chrome.exe" --user-data-dir="C:\PUBLIC profile" about:blank"#,
            r#""C:\Program Files\Chrome\chrome.exe" "--user-data-dir=C:\PUBLIC profile" about:blank"#,
        ] {
            let args = windows_arguments(command).unwrap();
            assert_eq!(args[0], r"C:\Program Files\Chrome\chrome.exe");
            assert_eq!(args[1], r"--user-data-dir=C:\PUBLIC profile");
        }
        assert!(windows_arguments(r#"chrome.exe --user-data-dir="unclosed"#).is_err());
    }
}
