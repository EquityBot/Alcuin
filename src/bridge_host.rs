//! Local bridge hosting, credential handoff and owned reverse SSH tunnel lifecycle.
use crate::{
    bridge,
    config::Environment,
    policy,
    remote_host::{HostOptions, RemoteHost},
    sessions::SessionStore,
    storage,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn clean(value: Option<&str>) -> Option<&str> {
    value.map(policy::trim).filter(|value| !value.is_empty())
}
pub fn token(explicit: Option<&str>, respawn: bool, path: &Path) -> String {
    if let Some(value) = clean(explicit) {
        return if value == "auto" {
            crate::remote_service::access_token()
        } else {
            value.into()
        };
    }
    if respawn
        && let Ok(value) = std::fs::read(path).and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes).map_err(std::io::Error::other)
        })
        && let Some(token) = clean(value["remoteToken"].as_str())
    {
        return token.into();
    }
    crate::remote_service::access_token()
}
fn write_connection(path: &Path, mut value: Value) -> Result<Value> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let previous = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    value["createdAt"] = json!(
        previous
            .as_ref()
            .and_then(|value| value["createdAt"].as_str())
            .filter(|value| !policy::trim(value).is_empty())
            .unwrap_or(&now)
    );
    value["updatedAt"] = json!(now);
    storage::private_dir(path.parent().context("Connection artifact has no parent")?)?;
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    storage::atomic_bytes(path, &bytes)?;
    Ok(value)
}
fn split_args(raw: &str) -> Vec<String> {
    let mut args = vec![];
    let mut current = String::new();
    let mut quote = None;
    for character in raw.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !policy::trim(&current).is_empty() {
                args.push(policy::trim(&current).to_owned());
            }
            current.clear();
        } else {
            current.push(character);
        }
    }
    if !policy::trim(&current).is_empty() {
        args.push(policy::trim(&current).to_owned());
    }
    args
}
fn ssh_executable(env: &Environment) -> Result<PathBuf> {
    #[cfg(not(windows))]
    {
        let _ = env;
        Ok(PathBuf::from("ssh"))
    }
    #[cfg(windows)]
    {
        for entry in env.get("PATH").map(String::as_str).unwrap_or("").split(';') {
            let directory = PathBuf::from(
                entry
                    .strip_prefix('"')
                    .and_then(|entry| entry.strip_suffix('"'))
                    .unwrap_or(entry),
            );
            if !directory.is_absolute() {
                continue;
            }
            for name in ["ssh.com", "ssh.exe"] {
                let candidate = directory.join(name);
                if std::fs::symlink_metadata(&candidate).is_ok_and(|metadata| !metadata.is_dir()) {
                    return Ok(candidate);
                }
            }
        }
        anyhow::bail!("--ssh requires ssh on an absolute PATH entry")
    }
}
fn ssh_args(options: &Value, remote: u16, local: u16) -> Vec<String> {
    let mut args = vec![
        "-N".into(),
        "-R".into(),
        format!("{remote}:127.0.0.1:{local}"),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(path) = clean(options["ssh-identity"].as_str()) {
        args.extend(["-i".into(), path.into()]);
    }
    if let Some(raw) = clean(options["ssh-extra-args"].as_str()) {
        args.extend(split_args(raw));
    }
    args.push(clean(options["ssh"].as_str()).unwrap().into());
    args
}
async fn reverse_tunnel(
    executable: PathBuf,
    args: Vec<String>,
    env: Environment,
    cwd: PathBuf,
    cancel: CancellationToken,
) -> Result<()> {
    let mut delay = 1;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut command = std::process::Command::new(&executable);
        command
            .args(&args)
            .env_clear()
            .envs(&env)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Each attempt owns its process group, including OpenSSH proxy helpers.
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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000 | 0x00000004);
        }
        #[cfg(windows)]
        let job = startup_job::Job::new()?;
        #[cfg(windows)]
        let child = startup_job::spawn(&mut command)?;
        #[cfg(not(windows))]
        let child = command
            .spawn()
            .context("Failed to start reverse SSH tunnel")?;
        let mut owner = StartingHost {
            child: Some(child),
            armed: true,
            #[cfg(windows)]
            job,
        };
        let pid = owner.child.as_ref().unwrap().id();
        let identity = crate::process::ProcessIdentity::read(pid)?;
        #[cfg(windows)]
        let identity = Some(identity.context("Suspended SSH attempt has no native identity")?);
        #[cfg(windows)]
        {
            owner.job.assign(owner.child.as_ref().unwrap())?;
            startup_job::resume(owner.child.as_ref().unwrap(), identity.as_ref().unwrap())?;
        }
        println!("[bridge host] ssh tunnel started (pid {pid})");
        while let Some(identity) = identity.as_ref() {
            if cancel.is_cancelled() || identity.liveness() == crate::process::Liveness::Dead {
                break;
            }
            ensure!(
                identity.liveness() != crate::process::Liveness::Unknown,
                "Reverse SSH identity became unknown"
            );
            tokio::select! {biased; _=cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(25))=>{}}
        }
        // Keep the root unreaped until every process in its attempt group/job has been stopped.
        owner.kill_tree()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = owner.child.as_mut().unwrap().try_wait()? {
                break status;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Reverse SSH attempt did not exit"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        #[cfg(windows)]
        owner.job.wait_empty()?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        println!(
            "[bridge host] ssh tunnel exited ({status}); restarting in {}ms",
            delay * 1000
        );
        tokio::select! {biased; _=cancel.cancelled()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(delay))=>{}}
        delay = (delay * 2).min(30);
    }
}
fn child_environment(env: &Environment) -> Environment {
    env.iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "HOME"
                    | "USERPROFILE"
                    | "PATH"
                    | "SystemRoot"
                    | "TEMP"
                    | "TMP"
                    | "ProgramFiles"
                    | "ProgramFiles(x86)"
                    | "LOCALAPPDATA"
                    | "CHROME_PATH"
                    | "LIGHTHOUSE_CHROMIUM_PATH"
                    | "ORACLE_HOME_DIR"
                    | "ORACLE_BROWSER_PROFILE_DIR"
                    | "ORACLE_BROWSER_APPROVAL_WAIT"
                    | "DISPLAY"
                    | "WAYLAND_DISPLAY"
                    | "XDG_RUNTIME_DIR"
                    | "DBUS_SESSION_BUS_ADDRESS"
                    | "SSH_AUTH_SOCK"
            )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
#[cfg(windows)]
pub(crate) mod startup_job {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::*,
    };
    pub fn resume(
        child: &std::process::Child,
        identity: &crate::process::ProcessIdentity,
    ) -> Result<()> {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                    Thread32Next,
                },
                Threading::{
                    GetProcessIdOfThread, OpenThread, ResumeThread,
                    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
                },
            },
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        ensure!(
            snapshot != INVALID_HANDLE_VALUE,
            "Cannot inspect suspended SSH thread"
        );
        let result = (|| {
            let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
            entry.dwSize = std::mem::size_of_val(&entry) as u32;
            let mut found = unsafe { Thread32First(snapshot, &mut entry) };
            while found != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let thread = unsafe {
                        OpenThread(
                            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                            0,
                            entry.th32ThreadID,
                        )
                    };
                    ensure!(!thread.is_null(), "Cannot open suspended SSH thread");
                    let result = (|| {
                        ensure!(
                            unsafe { GetProcessIdOfThread(thread) } == child.id()
                                && identity.liveness() == crate::process::Liveness::Alive,
                            "Suspended SSH thread identity changed"
                        );
                        ensure!(
                            unsafe { ResumeThread(thread) } == 1,
                            "SSH primary thread was not suspended exactly once"
                        );
                        Ok(())
                    })();
                    unsafe {
                        CloseHandle(thread);
                    }
                    return result;
                }
                found = unsafe { Thread32Next(snapshot, &mut entry) };
            }
            anyhow::bail!("Suspended SSH primary thread was not found")
        })();
        unsafe {
            CloseHandle(snapshot);
        }
        result
    }
    pub fn spawn(command: &mut std::process::Command) -> Result<std::process::Child> {
        static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = SPAWN
            .lock()
            .map_err(|_| anyhow::anyhow!("Background stream inheritance lock was poisoned"))?;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        struct Restore(Vec<(HANDLE, u32)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (handle, flags) in &self.0 {
                    unsafe {
                        SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, *flags);
                    }
                }
            }
        }
        let mut restore = Restore(vec![]);
        // A detached child must not inherit the launcher's redirected input/output pipe handles.
        for handle in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            let handle = handle as HANDLE;
            if handle.is_null()
                || handle == INVALID_HANDLE_VALUE
                || restore.0.iter().any(|(existing, _)| *existing == handle)
            {
                continue;
            }
            let mut flags = 0;
            ensure!(
                unsafe { GetHandleInformation(handle, &mut flags) } != 0,
                "Cannot inspect launcher stream inheritance: {}",
                std::io::Error::last_os_error()
            );
            if flags & HANDLE_FLAG_INHERIT != 0 {
                restore.0.push((handle, flags));
                ensure!(
                    unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0,
                    "Cannot isolate background streams: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok(command.spawn()?)
    }
    pub struct Job {
        handle: HANDLE,
        armed: bool,
    }
    // The unnamed kernel handle has one owner and may move between runtime threads.
    unsafe impl Send for Job {}
    impl Job {
        pub fn new() -> Result<Self> {
            // SAFETY: the unnamed handle has one owner and all native buffers are valid.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            ensure!(
                !handle.is_null(),
                "Cannot create background startup job: {}",
                std::io::Error::last_os_error()
            );
            let job = Self {
                handle,
                armed: true,
            };
            job.limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
            Ok(job)
        }
        fn limits(&self, flags: u32) -> Result<()> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = flags;
            ensure!(
                unsafe {
                    SetInformationJobObject(
                        self.handle,
                        JobObjectExtendedLimitInformation,
                        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        std::mem::size_of_val(&limits) as u32,
                    )
                } != 0,
                "Cannot set background job limits: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        pub fn assign(&self, child: &std::process::Child) -> Result<()> {
            use std::os::windows::io::AsRawHandle;
            ensure!(
                unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle().cast()) } != 0,
                "Cannot assign background startup job: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        pub fn terminate(&self) -> Result<()> {
            ensure!(
                unsafe { TerminateJobObject(self.handle, 1) } != 0,
                "Cannot terminate background job: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        pub fn release(&mut self) -> Result<()> {
            self.limits(0)?;
            self.armed = false;
            Ok(())
        }
        /// The disposable owner has already waited against its own deadline.
        /// Keep kill-on-close armed in the kernel without starting another wait.
        pub fn close_without_wait(mut self) {
            self.armed = false;
        }
        pub fn wait_empty(&self) -> Result<()> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if self.is_empty()? {
                    return Ok(());
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "Background job descendants did not exit"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        pub fn is_empty(&self) -> Result<bool> {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            ensure!(
                unsafe {
                    QueryInformationJobObject(
                        self.handle,
                        JobObjectBasicAccountingInformation,
                        (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        std::mem::size_of_val(&accounting) as u32,
                        std::ptr::null_mut(),
                    )
                } != 0,
                "Cannot inspect owned process job: {}",
                std::io::Error::last_os_error()
            );
            Ok(accounting.ActiveProcesses == 0)
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            if self.armed {
                let _ = self.terminate();
                let _ = self.wait_empty();
            }
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}
struct StartingHost {
    child: Option<std::process::Child>,
    armed: bool,
    #[cfg(windows)]
    job: startup_job::Job,
}
impl StartingHost {
    fn kill_tree(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        let child = self
            .child
            .as_mut()
            .context("Background startup child is missing")?;
        #[cfg(unix)]
        {
            // The setsid root remains unreaped until its owned process group has been signalled.
            let status = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("Cannot stop background process group");
                }
            }
        }
        #[cfg(windows)]
        {
            let result = self.job.terminate();
            let _ = child.kill();
            result?;
        }
        Ok(())
    }
}
impl Drop for StartingHost {
    fn drop(&mut self) {
        let _ = self.kill_tree();
        if let Some(child) = self.child.as_mut() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while child.try_wait().is_ok_and(|status| status.is_none())
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn background(
    options: &Value,
    store: &SessionStore,
    cwd: &Path,
    env: &Environment,
    path: &Path,
    connection: &Value,
    browser: &Value,
    cancel: &CancellationToken,
) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let bind = clean(options["bind"].as_str()).unwrap_or("127.0.0.1:9473");
    let log_path = store.home().join("bridge-host.log");
    let pid_path = store.home().join("bridge-host.pid");
    storage::private_dir(store.home())?;
    let mut log = OpenOptions::new();
    log.create(true).append(true);
    crate::native_fs::no_follow(&mut log, false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log.mode(0o600);
    }
    let log = log.open(&log_path)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "bridge",
            "host",
            "--foreground",
            "--respawn",
            "--bind",
            bind,
            "--write-connection",
        ])
        .arg(path);
    for key in ["ssh", "ssh-remote-port", "ssh-identity", "ssh-extra-args"] {
        if let Some(value) = options[key].as_str() {
            command.arg(format!("--{key}")).arg(value);
        }
    }
    command
        .current_dir(cwd)
        .env_clear()
        .envs(child_environment(env))
        .env("ORACLE_BRIDGE_START_GATE", "1")
        .stdin(Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no parent memory.
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200 | 0x08000000);
    }
    #[cfg(windows)]
    let job = startup_job::Job::new()?;
    #[cfg(windows)]
    let spawned = startup_job::spawn(&mut command)?;
    #[cfg(not(windows))]
    let spawned = command.spawn()?;
    let mut child = StartingHost {
        child: Some(spawned),
        armed: true,
        #[cfg(windows)]
        job,
    };
    let pid = child.child.as_ref().unwrap().id();
    let mut gate = child
        .child
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .context("Background host has no start gate")?;
    let identity = crate::process::ProcessIdentity::read(pid)?
        .context("Background host has no native identity")?;
    let result=async {
        #[cfg(windows)] child.job.assign(child.child.as_ref().unwrap())?;
        let mut bytes=serde_json::to_vec(&identity)?;bytes.push(b'\n');gate.write_all(&bytes)?;
        let (local_host,local_port)=bridge::parse_host_port(bind)?;
        let local_host=bridge::normalise_host_port(if matches!(local_host.as_str(),"0.0.0.0"|"::"){"127.0.0.1"}else{&local_host},local_port);
        let client=crate::remote_client::RemoteClient::new(&local_host,connection["remoteToken"].as_str().map(str::to_owned),store.clone())?;
        let deadline=tokio::time::Instant::now()+Duration::from_secs(10);
        loop {
            ensure!(!cancel.is_cancelled(),"Background bridge startup cancelled");
            ensure!(identity.liveness()==crate::process::Liveness::Alive,"Background bridge host exited before readiness");
            let started=std::fs::read(store.home().join("bridge-host-owner.json")).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()).is_some_and(|value|serde_json::from_value::<crate::process::ProcessIdentity>(value["processIdentity"].clone()).ok().as_ref()==Some(&identity));
            if started {
                let health=tokio::time::timeout(Duration::from_millis(500),client.health(cancel)).await;
                if health.is_ok_and(|health|health.is_ok_and(|health|health["ok"]==true)){break;}
            }
            ensure!(tokio::time::Instant::now()<deadline,"Background bridge host did not become healthy");
            tokio::select! {biased; _=cancel.cancelled()=>anyhow::bail!("Background bridge startup cancelled"),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
        }
        storage::atomic_bytes(&pid_path,format!("{pid}\n").as_bytes())?;
        Ok::<_,anyhow::Error>(())
    }.await;
    if let Err(error) = result {
        let tree_cleanup = child.kill_tree();
        let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.child.as_mut().unwrap().try_wait()? {
                    return Ok::<_, std::io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        #[cfg(windows)]
        let tree_cleanup = tree_cleanup.and_then(|()| child.job.wait_empty());
        if let Err(cleanup) = tree_cleanup {
            return Err(error.context(format!("Background descendant cleanup failed: {cleanup:#}")));
        }
        return match cleanup {
            Ok(Ok(_)) => Err(error),
            other => Err(anyhow::anyhow!(
                "{error}; background cleanup failed: {other:?}"
            )),
        };
    }
    #[cfg(windows)]
    child.job.release()?;
    gate.write_all(b"commit\n")?;
    drop(gate);
    child.armed = false;
    let mut child = child.child.take().unwrap();
    tokio::spawn(async move {
        while child.try_wait().is_ok_and(|status| status.is_none()) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(browser["approvalWaitMs"].as_u64().unwrap_or(20_000) * 2 + 35_000);
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "Background bridge startup cancelled after handoff; preserve the published native owner"
        );
        ensure!(
            identity.liveness() == crate::process::Liveness::Alive,
            "Background bridge host exited during browser bootstrap; inspect {}",
            log_path.display()
        );
        let ready = std::fs::read(store.home().join("bridge-host-ready.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| {
                value["connectionArtifact"] == json!(path)
                    && serde_json::from_value::<crate::process::ProcessIdentity>(
                        value["processIdentity"].clone(),
                    )
                    .ok()
                    .as_ref()
                        == Some(&identity)
            });
        if ready {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Background browser bootstrap deadline expired; preserve the published native owner and inspect {}",
            log_path.display()
        );
        tokio::select! {biased; _=cancel.cancelled()=>anyhow::bail!("Background bridge startup cancelled after handoff; preserve the published native owner"),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
    }
    println!(
        "Bridge host running in background (pid {pid})\n- Log: {}\n- PID: {}",
        log_path.display(),
        pid_path.display()
    );
    Ok(())
}
pub async fn run(
    options: &Value,
    store: SessionStore,
    cwd: PathBuf,
    env: Environment,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "Bridge host cancelled before startup"
    );
    let mut startup_gate = None;
    if env
        .get("ORACLE_BRIDGE_START_GATE")
        .is_some_and(|value| value == "1")
    {
        use std::io::{BufRead, Read};
        ensure!(
            options["respawn"] == true,
            "Background bridge start gate requires internal respawn"
        );
        let mut reader = std::io::BufReader::new(std::io::stdin());
        let mut bytes = Vec::new();
        (&mut reader).take(8193).read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= 8192
                && serde_json::from_slice::<crate::process::ProcessIdentity>(&bytes)?
                    == crate::process::ProcessIdentity::current()?,
            "Background bridge start gate identity mismatch"
        );
        startup_gate = Some(reader);
    }
    let bind = clean(options["bind"].as_str()).unwrap_or("127.0.0.1:9473");
    let (hostname, port) = bridge::parse_host_port(bind)?;
    let path = cwd.join(
        clean(options["write-connection"].as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| store.home().join("bridge-connection.json")),
    );
    let token = token(options["token"].as_str(), options["respawn"] == true, &path);
    // Reference parseIntOption, then the 1-65535 range with the parsed value.
    let remote = match options["ssh-remote-port"].as_str() {
        Some(raw) => {
            let raw = raw.trim_start();
            let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
            let end = digits
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(digits.len());
            ensure!(end > 0, "Value must be an integer.");
            let value = digits[..end].parse::<i128>().unwrap_or(i128::MAX);
            let value = if raw.starts_with('-') { -value } else { value };
            u16::try_from(value)
                .ok()
                .filter(|port| *port > 0)
                .with_context(|| format!("Invalid --ssh-remote-port: {value}. Expected 1-65535."))?
        }
        None => port,
    };
    let ssh = clean(options["ssh"].as_str());
    let executable = ssh.map(|_| ssh_executable(&env)).transpose()?;
    let client_host = if ssh.is_some() {
        bridge::normalise_host_port("127.0.0.1", remote)
    } else {
        bridge::normalise_host_port(
            if matches!(hostname.as_str(), "0.0.0.0" | "::") {
                "127.0.0.1"
            } else {
                &hostname
            },
            port,
        )
    };
    let mut connection = json!({"remoteHost":client_host,"remoteToken":token});
    if let Some(ssh) = ssh {
        connection["tunnel"] = json!({"ssh":ssh,"remotePort":remote,"localPort":port});
        for (source, target) in [
            ("ssh-identity", "identity"),
            ("ssh-extra-args", "extraArgs"),
        ] {
            if let Some(value) = clean(options[source].as_str()) {
                connection["tunnel"][target] = json!(value);
            }
        }
    }
    let connection = write_connection(&path, connection)?;
    if options["print-token"] == true {
        println!("{token}");
    }
    if options["print"] == true {
        println!("{}", bridge::format_connection(&connection, true)?);
    }
    let loaded = crate::config::load(
        store.home(),
        &cwd,
        env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(Path::new),
        true,
    );
    let browser = crate::remote_service::browser_config(&loaded.config, &json!({}), &env, &cwd)?;
    if options["background"] == true {
        return background(
            options,
            &store,
            &cwd,
            &env,
            &path,
            &connection,
            &browser,
            cancel,
        )
        .await;
    }
    if startup_gate.is_none() {
        crate::remote_service::bootstrap(&browser, &env, &cwd, cancel).await?;
    }
    let address = tokio::net::lookup_host((hostname.as_str(), port))
        .await?
        .next()
        .context("Bridge bind host did not resolve")?;
    let driver = crate::remote_driver::BrowserDriver {
        environment: env.clone(),
        cwd: cwd.clone(),
        browser_config: browser.clone(),
    };
    let ready = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let actual_driver = driver.driver();
    let admission = ready.clone();
    let driver: crate::remote_host::RunDriver = std::sync::Arc::new(move |context| {
        let actual_driver = actual_driver.clone();
        let admission = admission.clone();
        Box::pin(async move {
            let permit = tokio::select! {biased; _=context.cancel.cancelled()=>anyhow::bail!("Bridge startup cancelled before run admission"),permit=admission.acquire_owned()=>permit?};
            drop(permit);
            actual_driver(context).await
        })
    });
    let host = RemoteHost::start(
        store.clone(),
        HostOptions {
            address,
            token,
            max_concurrent_runs: None,
            max_queued_runs: None,
            max_browser_tabs: crate::browser_profile::MAX_TABS,
        },
        driver,
    )
    .await?;
    let owner = (|| {
        storage::atomic_json(
            &store.home().join("bridge-host-owner.json"),
            &json!({"processIdentity":crate::process::ProcessIdentity::current()?,"connectionArtifact":path}),
        )
    })();
    if let Err(error) = owner {
        let cleanup = host.shutdown().await;
        return Err(error.context(format!(
            "Bridge owner publication failed; shutdown: {cleanup:?}"
        )));
    }
    println!(
        "Bridge host starting...\n- Local bind: {}\n- Connection artifact: {}\n- Client remoteHost: {}\nToken stored in connection artifact (not printed). Use --print or --print-token if needed.\nListening at {}",
        bridge::normalise_host_port(&hostname, port),
        path.display(),
        client_host,
        host.address
    );
    if let Some(mut reader) = startup_gate {
        let commit = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut bytes = Vec::new();
            (&mut reader).take(8).read_to_end(&mut bytes)?;
            ensure!(
                bytes == b"commit\n",
                "Background bridge handoff was not committed"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Background commit reader failed")
        .and_then(|result| result);
        if let Err(error) = commit {
            let cleanup = host.shutdown().await;
            return Err(error.context(format!("Background handoff failed; shutdown: {cleanup:?}")));
        }
        if let Err(error) = crate::remote_service::bootstrap(&browser, &env, &cwd, cancel).await {
            let cleanup = host.shutdown().await;
            return Err(error.context(format!(
                "Bridge browser bootstrap failed; shutdown: {cleanup:?}"
            )));
        }
    }
    let published = (|| {
        storage::atomic_json(
            &store.home().join("bridge-host-ready.json"),
            &json!({"processIdentity":crate::process::ProcessIdentity::current()?,"connectionArtifact":path}),
        )
    })();
    if let Err(error) = published {
        let cleanup = host.shutdown().await;
        return Err(error.context(format!(
            "Bridge readiness publication failed; shutdown: {cleanup:?}"
        )));
    }
    ready.add_permits(1);
    let stopped = cancel.child_token();
    let _stop_on_drop = stopped.clone().drop_guard();
    let mut tunnel = executable.map(|executable| {
        let task = tokio::spawn(reverse_tunnel(
            executable,
            ssh_args(options, remote, port),
            child_environment(&env),
            cwd,
            stopped.clone(),
        ));
        println!("Reverse SSH tunnel active (remote 127.0.0.1:{remote} -> local 127.0.0.1:{port})");
        task
    });
    let (result, tunnel_joined) = tokio::select! {
        biased; _=cancel.cancelled()=>(Ok(()),false),result=crate::remote_service::shutdown_signal()=>(result,false),
        result=async {match tunnel.as_mut(){Some(task)=>task.await.context("Reverse SSH task failed")?,None=>std::future::pending().await}}=>(result,true)
    };
    stopped.cancel();
    let tunnel_result = if let Some(task) = tunnel {
        if tunnel_joined {
            Ok(())
        } else {
            task.await
                .context("Reverse SSH task failed")
                .and_then(|result| result)
        }
    } else {
        Ok(())
    };
    let host_result = host.shutdown().await;
    let errors = [result, tunnel_result, host_result]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_rotate_except_for_internal_respawn_and_artifacts_remain_private() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("nested/bridge.json");
        let first = token(None, false, &path);
        let connection = write_connection(
            &path,
            json!({"remoteHost":"127.0.0.1:9473","remoteToken":first}),
        )
        .unwrap();
        assert_eq!(token(None, true, &path), first);
        assert_ne!(token(None, false, &path), first);
        assert_ne!(token(Some("auto"), true, &path), first);
        assert_eq!(
            token(Some(" PUBLIC explicit "), false, &path),
            "PUBLIC explicit"
        );
        let second = write_connection(
            &path,
            json!({"remoteHost":"127.0.0.1:9474","remoteToken":"PUBLIC next"}),
        )
        .unwrap();
        assert_eq!(second["createdAt"], connection["createdAt"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            split_args(" -o 'PUBLIC value'  -p \"22\" "),
            ["-o", "PUBLIC value", "-p", "22"]
        );
        let env = Environment::from_iter([
            ("ORACLE_REMOTE_TOKEN".into(), "PUBLIC secret".into()),
            ("ORACLE_REMOTE_HOST".into(), "PUBLIC-host:123".into()),
            ("HOME".into(), "PUBLIC-home".into()),
            ("SSH_AUTH_SOCK".into(), "PUBLIC-agent-socket".into()),
            ("LIGHTHOUSE_CHROMIUM_PATH".into(), "PUBLIC-browser".into()),
        ]);
        assert_eq!(child_environment(&env).len(), 3);
        assert_eq!(
            child_environment(&env)["LIGHTHOUSE_CHROMIUM_PATH"],
            "PUBLIC-browser"
        );
        assert_eq!(
            child_environment(&env)["SSH_AUTH_SOCK"],
            "PUBLIC-agent-socket"
        );
    }
    #[test]
    #[ignore = "subprocess-only reverse tunnel lifecycle fixture"]
    fn tunnel_child_fixture() {
        if let Some(socket) = std::env::var_os("SSH_AUTH_SOCK") {
            std::fs::write(
                PathBuf::from(std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER").unwrap())
                    .with_extension("agent"),
                socket.to_string_lossy().as_bytes(),
            )
            .unwrap();
        }
        // Publish readiness only after every fixture marker is complete.
        storage::atomic_bytes(
            &PathBuf::from(std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER").unwrap()),
            std::process::id().to_string().as_bytes(),
        )
        .unwrap();
        if std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_EXIT").is_some() {
            use std::io::Write;
            let path = PathBuf::from(std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER").unwrap())
                .with_extension("attempts");
            let mut attempts = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(attempts, "{}", std::process::id()).unwrap();
            std::process::exit(7);
        }
        std::thread::sleep(Duration::from_secs(30));
    }
    #[test]
    #[ignore = "subprocess-only startup tree fixture"]
    fn startup_tree_fixture() {
        use std::io::Read;
        let mut gate = String::new();
        std::io::stdin().read_to_string(&mut gate).unwrap();
        assert_eq!(gate, "ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "bridge_host::tests::tunnel_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env(
                "INDUSTRIOUS_PUBLIC_TUNNEL_MARKER",
                std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER").unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(root) = std::env::var_os("SystemRoot") {
            child.env("SystemRoot", root);
        }
        let mut child = child.spawn().unwrap();
        std::thread::sleep(Duration::from_secs(30));
        let _ = child.wait();
    }
    #[test]
    #[ignore = "subprocess-only SSH proxy fixture"]
    fn tunnel_proxy_fixture() {
        let marker = PathBuf::from(std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER").unwrap());
        std::fs::write(
            marker.with_extension("root"),
            std::process::id().to_string(),
        )
        .unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bridge_host::tests::tunnel_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn().unwrap();
        if std::env::var_os("INDUSTRIOUS_PUBLIC_TUNNEL_NATURAL").is_some() {
            std::thread::sleep(Duration::from_millis(500));
            std::process::exit(7);
        }
        let _ = child.wait();
    }
    #[tokio::test]
    async fn tunnel_proxy_descendants_exit_on_cancellation_and_before_restart() {
        for natural in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let marker = home.path().join("proxy.pid");
            let mut env = Environment::from_iter([(
                "INDUSTRIOUS_PUBLIC_TUNNEL_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            )]);
            if natural {
                env.insert("INDUSTRIOUS_PUBLIC_TUNNEL_NATURAL".into(), "1".into());
            }
            if let Ok(root) = std::env::var("SystemRoot") {
                env.insert("SystemRoot".into(), root);
            }
            let cancel = CancellationToken::new();
            let _cancel_on_failure = cancel.clone().drop_guard();
            let task = tokio::spawn(reverse_tunnel(
                std::env::current_exe().unwrap(),
                vec![
                    "--exact".into(),
                    "bridge_host::tests::tunnel_proxy_fixture".into(),
                    "--ignored".into(),
                    "--nocapture".into(),
                ],
                env,
                home.path().into(),
                cancel.clone(),
            ));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let descendant = loop {
                if let Some(pid) = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|text| text.parse().ok())
                    && let Some(identity) = crate::process::ProcessIdentity::read(pid).unwrap()
                {
                    break identity;
                }
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            if !natural {
                cancel.cancel();
            }
            while descendant.liveness() == crate::process::Liveness::Alive {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "SSH proxy survived its attempt"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
            tokio::time::timeout(Duration::from_secs(6), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(descendant.liveness(), crate::process::Liveness::Dead);
        }
    }
    #[tokio::test]
    async fn immediately_exiting_tunnel_attempts_back_off_and_restart() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("fast.pid");
        let mut env = Environment::from_iter([
            (
                "INDUSTRIOUS_PUBLIC_TUNNEL_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            ),
            ("INDUSTRIOUS_PUBLIC_TUNNEL_EXIT".into(), "1".into()),
        ]);
        if let Ok(root) = std::env::var("SystemRoot") {
            env.insert("SystemRoot".into(), root);
        }
        let cancel = CancellationToken::new();
        let _cancel_on_failure = cancel.clone().drop_guard();
        let task = tokio::spawn(reverse_tunnel(
            std::env::current_exe().unwrap(),
            vec![
                "--exact".into(),
                "bridge_host::tests::tunnel_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            env,
            home.path().into(),
            cancel.clone(),
        ));
        // Each attempt starts this whole test binary as the fake SSH child, so two attempts and
        // the 1 s backoff can take several seconds under a parallel suite. The bound only stops
        // a hang; the backoff itself is not timed here.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if std::fs::read_to_string(marker.with_extension("attempts"))
                .is_ok_and(|text| text.lines().count() >= 2)
            {
                break;
            }
            assert!(
                !task.is_finished(),
                "Fast SSH failure incorrectly stopped the host tunnel task"
            );
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn failed_or_dropped_startup_reaps_the_owned_process_tree() {
        use std::io::Write;
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("descendant.pid");
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bridge_host::tests::startup_tree_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("INDUSTRIOUS_PUBLIC_TUNNEL_MARKER", &marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
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
        #[cfg(windows)]
        let job = startup_job::Job::new().unwrap();
        let mut owner = StartingHost {
            child: Some(command.spawn().unwrap()),
            armed: true,
            #[cfg(windows)]
            job,
        };
        let root = crate::process::ProcessIdentity::read(owner.child.as_ref().unwrap().id())
            .unwrap()
            .unwrap();
        #[cfg(windows)]
        owner.job.assign(owner.child.as_ref().unwrap()).unwrap();
        let mut gate = owner.child.as_mut().unwrap().stdin.take().unwrap();
        gate.write_all(b"ready").unwrap();
        drop(gate);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.parse().ok())
                && let Some(identity) = crate::process::ProcessIdentity::read(pid).unwrap()
            {
                break identity;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop(owner);
        assert_eq!(root.liveness(), crate::process::Liveness::Dead);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while descendant.liveness() == crate::process::Liveness::Alive {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(descendant.liveness(), crate::process::Liveness::Dead);
    }
    #[tokio::test]
    async fn tunnel_cancellation_reaps_its_exact_running_child() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join("child.pid");
        let mut env = Environment::from_iter([(
            "INDUSTRIOUS_PUBLIC_TUNNEL_MARKER".into(),
            marker.to_string_lossy().into_owned(),
        )]);
        env.insert("SSH_AUTH_SOCK".into(), "PUBLIC-agent-socket".into());
        if let Ok(root) = std::env::var("SystemRoot") {
            env.insert("SystemRoot".into(), root);
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(reverse_tunnel(
            std::env::current_exe().unwrap(),
            vec![
                "--exact".into(),
                "bridge_host::tests::tunnel_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            env,
            home.path().into(),
            cancel.clone(),
        ));
        let _cancel_on_drop = cancel.clone().drop_guard();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Ok(pid) = std::fs::read_to_string(&marker) {
                let pid = pid.parse().unwrap();
                if let Some(identity) = crate::process::ProcessIdentity::read(pid).unwrap() {
                    break identity;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Tunnel child did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            std::fs::read_to_string(marker.with_extension("agent")).unwrap(),
            "PUBLIC-agent-socket"
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(identity.liveness(), crate::process::Liveness::Dead);
    }
}
