use super::*;
use crate::{browser_profile_copy::TemporaryProfile, process::ProcessIdentity};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[test]
fn termination_plan_matches_the_reference_hook() {
    let pending = "Received SIGTERM; leaving Chrome running (assistant response pending)";
    let reattach = "Session still in flight; reattach with \"alcuin session <slug>\" to continue.";
    let terminate = "Received SIGTERM; terminating Chrome process";
    let copied = "Received SIGTERM; terminating Chrome and removing the copied profile (copy-profile is not retained)";
    // (keep, in flight, shared manual login, copied) -> leave running, lines
    for (keep, in_flight, shared, force, leave, lines) in [
        (false, true, false, false, true, vec![pending, reattach]),
        (true, true, false, false, true, vec![pending, reattach]),
        (false, true, true, false, true, vec![pending, reattach]),
        (
            true,
            false,
            false,
            false,
            true,
            vec!["Received SIGTERM; leaving Chrome running"],
        ),
        (
            false,
            false,
            true,
            false,
            true,
            vec!["Received SIGTERM; leaving Chrome running"],
        ),
        (false, false, false, false, false, vec![terminate]),
        (false, true, false, true, false, vec![copied]),
        (true, false, false, true, false, vec![copied]),
        (false, false, false, true, false, vec![terminate]),
    ] {
        assert_eq!(
            plan("SIGTERM", keep, in_flight, shared, force),
            (leave, lines.into_iter().map(String::from).collect()),
            "keep={keep} in_flight={in_flight} shared={shared} force={force}"
        );
    }
    assert_eq!(
        plan("SIGQUIT", false, false, false, false).1,
        ["Received SIGQUIT; terminating Chrome process"]
    );
    // Ctrl-C takes the same hook (reference SIGINT listener).
    assert_eq!(
        plan("SIGINT", false, true, false, false),
        (
            true,
            vec![
                "Received SIGINT; leaving Chrome running (assistant response pending)".into(),
                reattach.into()
            ]
        )
    );
    assert_eq!(
        plan("SIGINT", true, false, false, false),
        (true, vec!["Received SIGINT; leaving Chrome running".into()])
    );
    assert_eq!(
        plan("SIGINT", false, false, false, false),
        (
            false,
            vec!["Received SIGINT; terminating Chrome process".into()]
        )
    );
    assert_eq!(
        plan("SIGINT", false, true, false, true),
        (
            false,
            vec!["Received SIGINT; terminating Chrome and removing the copied profile (copy-profile is not retained)".into()]
        )
    );
}

const ROOT: &str = "ALCUIN_SYNTHETIC_SIGNAL";
const CASE: &str = "ALCUIN_SYNTHETIC_SIGNAL_CASE";

/// A detached child stands in for Chrome, as Chrome is launched outside the console group.
fn synthetic_chrome() -> std::process::Child {
    #[cfg(unix)]
    let mut command = {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        // SAFETY: setsid only detaches this synthetic child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        command
    };
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("ping");
        command
            .args(["-n", "60", "127.0.0.1"])
            .creation_flags(0x00000008 | 0x00000200 | 0x08000000);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
fn raise_interrupt() {
    // SAFETY: interrupts only this helper process.
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGINT);
    }
    // SAFETY: the helper owns its own hidden console; group 0 is that console alone.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            CTRL_C_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
        };
        SetConsoleCtrlHandler(None, 0);
        assert_ne!(GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0), 0);
    }
}

/// Cases: `term-*`/`quit-*` wait for a Unix signal from the test; `int-*` (CLI) and
/// `mcp-*` interrupt themselves. A `*-none` case registers no hook.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "helper process for the browser signal tests"]
async fn signal_helper_process() {
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        return;
    };
    let case = std::env::var(CASE).unwrap();
    install();
    let mut profile = TemporaryProfile::create(&root.join("profiles")).unwrap();
    profile.launched();
    let child = synthetic_chrome();
    let registration = (!case.ends_with("-none")).then(|| {
        register(Hook::new(
            false,
            false,
            case.ends_with("-copied"),
            SignalTarget::new(Some(&child), Some(&profile)),
            Box::new(|line| eprintln!("{line}")),
        ))
    });
    if case.ends_with("-complete") {
        registration.as_ref().unwrap().complete();
    }
    let cancel = CancellationToken::new();
    let ctrl_c = if case.starts_with("int-") {
        Some(cancel_on_ctrl_c(cancel.clone()))
    } else {
        if case.starts_with("mcp-") {
            exit_on_ctrl_c();
        }
        None
    };
    let identity = ProcessIdentity::read(child.id()).unwrap().unwrap();
    let state = serde_json::json!({"chrome":identity,"profile":profile.path()});
    std::fs::write(root.join("state.tmp"), state.to_string()).unwrap();
    std::fs::rename(root.join("state.tmp"), root.join("state.json")).unwrap();
    if case.starts_with("int-") || case.starts_with("mcp-") {
        // Let the listener task register before the interrupt.
        tokio::time::sleep(Duration::from_millis(300)).await;
        raise_interrupt();
    }
    tokio::select! {
        _ = cancel.cancelled() => std::fs::write(root.join("settled"), "cancelled").unwrap(),
        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
    }
    drop((ctrl_c, child, registration, profile));
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    lines: Vec<String>,
    settled: bool,
    chrome: ProcessIdentity,
    profile: PathBuf,
    _root: tempfile::TempDir,
}
impl Drop for Run {
    fn drop(&mut self) {
        if !alive(&self.chrome) {
            return;
        }
        // SAFETY: the helper launched this exact synthetic process group with setsid.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.chrome.pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &self.chrome.pid.to_string()])
            .output();
    }
}
fn helper(case: &str, #[cfg(unix)] signal: Option<i32>) -> Run {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("helper");
    std::fs::create_dir_all(base.join("profiles")).unwrap();
    let stdout = std::fs::File::create(root.path().join("helper.stdout")).unwrap();
    let stderr = std::fs::File::create(root.path().join("helper.stderr")).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "browser_signal::tests::signal_helper_process",
            "--nocapture",
        ])
        .env(ROOT, &base)
        .env(CASE, case)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    // A hidden console of its own: the interrupt never reaches this test process.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x08000000);
    let mut helper = command.spawn().unwrap();
    let state = base.join("state.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !state.exists() {
        assert!(Instant::now() < deadline, "helper did not start");
        assert!(helper.try_wait().unwrap().is_none(), "helper exited early");
        std::thread::sleep(Duration::from_millis(20));
    }
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    #[cfg(unix)]
    if let Some(signal) = signal {
        // SAFETY: signals only the helper this test spawned and still owns unreaped.
        assert_eq!(unsafe { libc::kill(helper.id() as i32, signal) }, 0);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = helper.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = helper.kill();
            panic!("helper did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = std::fs::read_to_string(root.path().join("helper.stderr")).unwrap();
    let lines = stderr
        .lines()
        .filter(|line| line.starts_with("Received ") || line.starts_with("Session "))
        .map(str::to_owned)
        .collect();
    Run {
        status,
        stdout: std::fs::read_to_string(root.path().join("helper.stdout")).unwrap(),
        lines,
        stderr,
        settled: base.join("settled").exists(),
        chrome: serde_json::from_value(state["chrome"].clone()).unwrap(),
        profile: serde_json::from_value(state["profile"].clone()).unwrap(),
        _root: root,
    }
}
fn interrupt(case: &str) -> Run {
    helper(
        case,
        #[cfg(unix)]
        None,
    )
}
fn alive(identity: &ProcessIdentity) -> bool {
    ProcessIdentity::read(identity.pid)
        .unwrap()
        .is_some_and(|actual| actual.native_start == identity.native_start)
}
fn exists(path: &Path) -> bool {
    path.try_exists().unwrap()
}
fn cancelled_line(stdout: &str) -> bool {
    stdout.lines().any(|line| line == "Cancelled.")
}

#[test]
fn ctrl_c_with_a_browser_hook_exits_130_without_settling_the_run() {
    let run = interrupt("int-in-flight");
    assert_eq!(run.status.code(), Some(INTERRUPT_EXIT_CODE));
    assert!(cancelled_line(&run.stdout), "{}", run.stdout);
    assert_eq!(
        run.lines,
        [
            "Received SIGINT; leaving Chrome running (assistant response pending)",
            "Session still in flight; reattach with \"alcuin session <slug>\" to continue."
        ]
    );
    assert!(!run.settled);
    assert!(alive(&run.chrome) && exists(&run.profile));

    let run = interrupt("int-complete");
    assert_eq!(run.status.code(), Some(INTERRUPT_EXIT_CODE));
    assert!(cancelled_line(&run.stdout), "{}", run.stdout);
    assert_eq!(run.lines, ["Received SIGINT; terminating Chrome process"]);
    assert!(!run.settled);
    assert!(
        !alive(&run.chrome) && !exists(&run.profile),
        "{}",
        run.stderr
    );
}

#[test]
fn ctrl_c_without_a_browser_hook_cancels_the_run() {
    let run = interrupt("int-none");
    assert_eq!(run.status.code(), Some(0));
    assert!(run.settled);
    assert!(!cancelled_line(&run.stdout), "{}", run.stdout);
    assert!(run.lines.is_empty());
    assert!(alive(&run.chrome) && exists(&run.profile));
}

#[test]
fn mcp_ctrl_c_without_a_browser_hook_keeps_the_default_action() {
    let run = interrupt("mcp-none");
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&run.status),
        Some(libc::SIGINT)
    );
    #[cfg(windows)]
    assert_eq!(run.status.code(), Some(0xC000013A_u32 as i32));
    assert!(!run.settled && run.lines.is_empty());
}

#[cfg(unix)]
#[test]
fn termination_signals_leave_or_end_the_owned_chrome_and_exit_one() {
    let run = helper("term-in-flight", Some(libc::SIGTERM));
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(
        run.lines,
        [
            "Received SIGTERM; leaving Chrome running (assistant response pending)",
            "Session still in flight; reattach with \"alcuin session <slug>\" to continue."
        ]
    );
    assert!(alive(&run.chrome) && exists(&run.profile));

    let run = helper("term-complete", Some(libc::SIGTERM));
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(run.lines, ["Received SIGTERM; terminating Chrome process"]);
    assert!(
        !alive(&run.chrome) && !exists(&run.profile),
        "{}",
        run.stderr
    );

    let run = helper("quit-copied", Some(libc::SIGQUIT));
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(
        run.lines,
        [
            "Received SIGQUIT; terminating Chrome and removing the copied profile (copy-profile is not retained)"
        ]
    );
    assert!(
        !alive(&run.chrome) && !exists(&run.profile),
        "{}",
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn termination_without_an_owned_chrome_keeps_the_default_disposition() {
    let run = helper("term-none", Some(libc::SIGTERM));
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&run.status),
        Some(libc::SIGTERM)
    );
    assert!(run.lines.is_empty());
    assert!(alive(&run.chrome) && exists(&run.profile));
}
