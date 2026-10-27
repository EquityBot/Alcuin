use alcuin::{
    config::Environment,
    process::{Liveness, ProcessIdentity},
    remote_client::RemoteClient,
    sessions::SessionStore,
};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn environment(home: &Path) -> Environment {
    std::fs::create_dir_all(home.join("oracle")).unwrap();
    std::fs::write(
        home.join("oracle/config.json"),
        br#"{"browser":{"remoteChrome":{"host":"127.0.0.1","port":1}}}"#,
    )
    .unwrap();
    let mut environment = Environment::from_iter([
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
        (
            "ORACLE_HOME_DIR".into(),
            home.join("oracle").to_string_lossy().into_owned(),
        ),
    ]);
    if let Ok(root) = std::env::var("SystemRoot") {
        environment.insert("SystemRoot".into(), root);
    }
    environment
}
fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn ready(home: &Path, identity: &ProcessIdentity) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert_eq!(
            identity.liveness(),
            Liveness::Alive,
            "Owned bridge host exited before readiness"
        );
        if let Ok(bytes) = std::fs::read(home.join("oracle/bridge-connection.json"))
            && let Ok(connection) = serde_json::from_slice::<Value>(&bytes)
        {
            let client = RemoteClient::new(
                connection["remoteHost"].as_str().unwrap(),
                connection["remoteToken"].as_str().map(str::to_owned),
                SessionStore::new(home.join("receiver")),
            )
            .unwrap();
            if client.health(&CancellationToken::new()).await.unwrap()["ok"] == true {
                return connection;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Bridge host did not become healthy"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
#[tokio::test]
async fn compiled_foreground_host_rotates_credentials_redacts_output_and_exits() {
    let home = tempfile::tempdir().unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let address = format!("127.0.0.1:{}", unused_port());
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(environment(home.path()))
            .current_dir(home.path())
            .kill_on_drop(true)
            .args(["bridge", "host", "--foreground", "--bind", &address])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let identity = ProcessIdentity::read(child.id().unwrap()).unwrap().unwrap();
        let connection = ready(home.path(), &identity).await;
        let token = connection["remoteToken"].as_str().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if let Some(previous) = previous {
            assert_ne!(token, previous);
        }
        previous = Some(token.to_owned());
        #[cfg(unix)]
        {
            assert_eq!(identity.liveness(), Liveness::Alive);
            unsafe {
                assert_eq!(libc::kill(identity.pid as i32, libc::SIGTERM), 0);
            }
        }
        #[cfg(not(unix))]
        let child = {
            let mut child = child;
            child.start_kill().unwrap();
            child
        };
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        #[cfg(unix)]
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains(token));
        assert_ne!(identity.liveness(), Liveness::Alive);
        assert!(!home.path().join(".oracle/browser-profile").exists());
        assert!(
            SessionStore::new(home.path().join("oracle"))
                .list(false)
                .unwrap()
                .is_empty()
        );
    }
}
#[cfg(target_os = "linux")]
struct OwnedBackground {
    descriptor: std::os::fd::OwnedFd,
    identity: ProcessIdentity,
}
#[cfg(target_os = "linux")]
impl OwnedBackground {
    fn stop(&self) {
        self.signal(libc::SIGTERM);
    }
    fn open(identity: ProcessIdentity) -> Self {
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open returns a new descriptor owned by this fixture.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        assert!(raw >= 0);
        let descriptor = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
        assert_eq!(
            ProcessIdentity::read(identity.pid).unwrap().as_ref(),
            Some(&identity)
        );
        Self {
            descriptor,
            identity,
        }
    }
    fn signal(&self, signal: i32) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.descriptor.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}
#[cfg(windows)]
struct OwnedBackground {
    handle: windows_sys::Win32::Foundation::HANDLE,
    identity: ProcessIdentity,
}
#[cfg(windows)]
impl OwnedBackground {
    fn open(identity: ProcessIdentity) -> Self {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FILETIME},
            System::Threading::{
                GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
            },
        };
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | 0x00100000,
                0,
                identity.pid,
            );
            assert!(!handle.is_null());
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit = creation;
            let mut kernel = creation;
            let mut user = creation;
            let valid = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)
                != 0
                && (u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
                    == identity.native_start;
            if !valid {
                CloseHandle(handle);
                panic!("Background process handle does not match the owned native identity");
            }
            Self { handle, identity }
        }
    }
    fn stop(&self) {
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(self.handle, 0);
        }
    }
}
#[cfg(windows)]
impl Drop for OwnedBackground {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.handle, 5000);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for OwnedBackground {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}
#[cfg(any(target_os = "linux", windows))]
struct BackgroundWatchdog {
    sender: Option<std::sync::mpsc::Sender<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
#[cfg(any(target_os = "linux", windows))]
impl BackgroundWatchdog {
    fn start(home: &Path) -> Self {
        let owner = home.join("oracle/bridge-host-owner.json");
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            if receiver.recv_timeout(Duration::from_secs(20)) == Ok(true) {
                return;
            }
            if let Some(identity) = std::fs::read(owner)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| {
                    serde_json::from_value::<ProcessIdentity>(value["processIdentity"].clone()).ok()
                })
                && identity.liveness() == Liveness::Alive
            {
                let guard = OwnedBackground::open(identity);
                guard.stop();
                drop(guard);
            }
        });
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }
    fn disarm(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(true);
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
#[cfg(any(target_os = "linux", windows))]
impl Drop for BackgroundWatchdog {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(false);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn failed_background_pid_publication_cannot_start_ssh_before_commit() {
    let home = tempfile::tempdir().unwrap();
    let oracle = home.path().join("oracle");
    std::fs::create_dir_all(oracle.join("bridge-host.pid")).unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
    std::fs::copy(env!("CARGO_BIN_EXE_alcuin"), &fake).unwrap();
    let mut env = environment(home.path());
    env.insert("PATH".into(), bin.to_string_lossy().into_owned());
    std::fs::remove_file(oracle.join("config.json")).unwrap();
    let chrome = bin.join(if cfg!(windows) {
        "chrome.exe"
    } else {
        "chrome"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_alcuin"), &chrome).unwrap();
    env.insert("CHROME_PATH".into(), chrome.to_string_lossy().into_owned());
    let mut watchdog = BackgroundWatchdog::start(home.path());
    let address = format!("127.0.0.1:{}", unused_port());
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(env)
            .current_dir(home.path())
            .kill_on_drop(true)
            .args([
                "bridge",
                "host",
                "--background",
                "--bind",
                &address,
                "--ssh",
                "PUBLIC-user@PUBLIC-host",
            ])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let owner: Value =
        serde_json::from_slice(&std::fs::read(oracle.join("bridge-host-owner.json")).unwrap())
            .unwrap();
    let identity: ProcessIdentity =
        serde_json::from_value(owner["processIdentity"].clone()).unwrap();
    assert_ne!(identity.liveness(), Liveness::Alive);
    watchdog.disarm();
    let log = std::fs::read_to_string(oracle.join("bridge-host.log")).unwrap();
    assert!(!log.contains("ssh tunnel started"), "{log}");
    assert!(!home.path().join(".oracle/browser-profile").exists());
    assert!(SessionStore::new(oracle).list(false).unwrap().is_empty());
}
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn background_bootstrap_failure_is_reported_before_success() {
    let home = tempfile::tempdir().unwrap();
    let mut env = environment(home.path());
    std::fs::remove_file(home.path().join("oracle/config.json")).unwrap();
    let chrome = home.path().join(if cfg!(windows) {
        "chrome.exe"
    } else {
        "chrome"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_alcuin"), &chrome).unwrap();
    env.insert("CHROME_PATH".into(), chrome.to_string_lossy().into_owned());
    let mut watchdog = BackgroundWatchdog::start(home.path());
    let address = format!("127.0.0.1:{}", unused_port());
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(env)
            .current_dir(home.path())
            .kill_on_drop(true)
            .args(["bridge", "host", "--background", "--bind", &address])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let owner: Value = serde_json::from_slice(
        &std::fs::read(home.path().join("oracle/bridge-host-owner.json")).unwrap(),
    )
    .unwrap();
    let identity: ProcessIdentity =
        serde_json::from_value(owner["processIdentity"].clone()).unwrap();
    assert_ne!(identity.liveness(), Liveness::Alive);
    watchdog.disarm();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("running in background"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("during browser bootstrap"));
    assert!(!home.path().join("oracle/bridge-host-ready.json").exists());
}
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn compiled_background_host_survives_parent_exit_and_hands_off_credentials_only_in_artifact()
{
    let home = tempfile::tempdir().unwrap();
    let mut watchdog = BackgroundWatchdog::start(home.path());
    let address = format!("127.0.0.1:{}", unused_port());
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(environment(home.path()))
            .current_dir(home.path())
            .kill_on_drop(true)
            .args([
                "bridge",
                "host",
                "--background",
                "--bind",
                &address,
                "--token",
                "PUBLIC-background-token",
            ])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "Background bridge launch failed before its owner record: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let owner: Value = serde_json::from_slice(
        &std::fs::read(home.path().join("oracle/bridge-host-owner.json")).unwrap(),
    )
    .unwrap();
    let identity: ProcessIdentity =
        serde_json::from_value(owner["processIdentity"].clone()).unwrap();
    let guard = OwnedBackground::open(identity);
    watchdog.disarm();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready_record: Value = serde_json::from_slice(
        &std::fs::read(home.path().join("oracle/bridge-host-ready.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_value::<ProcessIdentity>(ready_record["processIdentity"].clone()).unwrap(),
        guard.identity
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("PUBLIC-background-token"));
    let connection = ready(home.path(), &guard.identity).await;
    assert_eq!(connection["remoteToken"], "PUBLIC-background-token");
    #[cfg(target_os = "linux")]
    {
        let command = std::fs::read(format!("/proc/{}/cmdline", guard.identity.pid)).unwrap();
        let environment = std::fs::read(format!("/proc/{}/environ", guard.identity.pid)).unwrap();
        for bytes in [command, environment] {
            assert!(
                !bytes
                    .windows(b"PUBLIC-background-token".len())
                    .any(|window| window == b"PUBLIC-background-token")
            );
        }
    }
    guard.stop();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while guard.identity.liveness() == Liveness::Alive {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = std::fs::read_to_string(home.path().join("oracle/bridge-host.log")).unwrap();
    assert!(!log.contains("PUBLIC-background-token"));
}
