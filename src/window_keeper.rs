//! The Windows keeper follows an exact Chrome process and never activates its windows.
#[cfg(not(windows))]
pub(crate) fn run(_: &crate::process::ProcessIdentity, _: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("The Chrome window keeper is only supported on Windows")
}
#[cfg(windows)]
pub(crate) use native::{run, start};

#[cfg(windows)]
mod native {
    use crate::{
        browser_profile::ProfileRegistry,
        native_fs::NativeEntry,
        process::{Liveness, ProcessIdentity},
        storage::atomic_json,
    };
    use anyhow::{Context, Result, ensure};
    use serde_json::{Value, json};
    use std::{
        io::Read,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{FILETIME, HWND, LPARAM, RECT, WAIT_FAILED, WAIT_OBJECT_0},
        Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        System::Threading::{
            GetProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            TerminateProcess, WaitForSingleObject,
        },
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
                ShellExecuteExW,
            },
            WindowsAndMessaging::*,
        },
    };
    const RECORD: &str = "oracle-window-keeper.json";
    type Process = OwnedHandle;
    struct Startup {
        process: Process,
        ready: PathBuf,
        armed: bool,
    }
    impl Drop for Startup {
        fn drop(&mut self) {
            if self.armed {
                unsafe {
                    TerminateProcess(self.process.as_raw_handle(), 1);
                    WaitForSingleObject(self.process.as_raw_handle(), 2000);
                }
            }
            let _ = std::fs::remove_file(&self.ready);
        }
    }
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                DeregisterShellHookWindow(self.0);
                DestroyWindow(self.0);
            }
        }
    }
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn process_of(window: HWND) -> u32 {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut pid);
        }
        pid
    }
    fn flash_stop(window: HWND) {
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: window,
            dwFlags: FLASHW_STOP,
            uCount: 0,
            dwTimeout: 0,
        };
        unsafe {
            FlashWindowEx(&info);
        }
    }
    unsafe extern "system" fn collect(window: HWND, context: LPARAM) -> i32 {
        // SAFETY: EnumWindows holds the caller's Vec for this synchronous enumeration.
        unsafe { &mut *(context as *mut Vec<HWND>) }.push(window);
        1
    }
    fn windows() -> Result<Vec<HWND>> {
        let mut result = Vec::new();
        ensure!(
            unsafe { EnumWindows(Some(collect), &mut result as *mut Vec<HWND> as LPARAM) } != 0,
            "Window enumeration failed"
        );
        Ok(result)
    }
    fn shown(window: HWND) -> Option<RECT> {
        unsafe {
            if IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
                return None;
            }
            let mut cloaked = 0i32;
            if DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut i32 as *mut _,
                4,
            ) == 0
                && cloaked != 0
            {
                return None;
            }
            let mut name = [0u16; 64];
            let length = GetClassNameW(window, name.as_mut_ptr(), name.len() as i32);
            if matches!(
                String::from_utf16_lossy(&name[..length.max(0) as usize]).as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            ) {
                return None;
            }
            let mut rect = std::mem::zeroed();
            (GetWindowRect(window, &mut rect) != 0
                && rect.right - rect.left > 1
                && rect.bottom - rect.top > 1)
                .then_some(rect)
        }
    }
    fn overlaps(left: &RECT, right: &RECT) -> bool {
        right.left < left.right
            && left.left < right.right
            && right.top < left.bottom
            && left.top < right.bottom
    }
    fn tick(pid: u32) -> Result<bool> {
        let focused = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
        let inventory = windows()?;
        let mut moved = false;
        for (index, window) in inventory.iter().copied().enumerate() {
            if window == focused
                || process_of(window) != pid
                || unsafe { GetAncestor(window, GA_ROOTOWNER) } != window
            {
                continue;
            }
            let Some(chrome) = shown(window) else {
                continue;
            };
            for below in inventory[index + 1..].iter().rev().copied() {
                if process_of(below) == pid
                    || !shown(below).is_some_and(|rect| overlaps(&chrome, &rect))
                {
                    continue;
                }
                if unsafe {
                    SetWindowPos(
                        window,
                        below,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
                    )
                } != 0
                {
                    moved = true;
                    break;
                }
            }
        }
        Ok(moved)
    }
    fn open(identity: &ProcessIdentity) -> Result<Process> {
        ensure!(
            identity.liveness() == Liveness::Alive,
            "Keeper Chrome identity is not alive"
        );
        // SYNCHRONIZE keeps the wait bound to this process handle, including after PID reuse.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | 0x00100000,
                0,
                identity.pid,
            )
        };
        ensure!(!handle.is_null(), "Keeper could not open Chrome");
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut created: FILETIME = unsafe { std::mem::zeroed() };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        ensure!(
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0,
            "Keeper could not verify Chrome start time"
        );
        ensure!(
            ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                == identity.native_start,
            "Keeper Chrome identity changed"
        );
        Ok(process)
    }
    pub(crate) fn run(identity: &ProcessIdentity, ready: &Path) -> Result<()> {
        let process = open(identity)?;
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let message = unsafe { RegisterWindowMessageW(wide("SHELLHOOK").as_ptr()) };
        ensure!(message != 0, "Keeper shell message registration failed");
        let window = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        ensure!(!window.is_null(), "Keeper message window creation failed");
        let window = Window(window);
        ensure!(
            unsafe { RegisterShellHookWindow(window.0) } != 0,
            "Keeper shell hook registration failed"
        );
        for window in windows()? {
            if process_of(window) == identity.pid && unsafe { IsWindowVisible(window) } != 0 {
                flash_stop(window);
            }
        }
        atomic_json(
            ready,
            &json!({"keeper":ProcessIdentity::current()?,"chrome":identity}),
        )?;
        let mut streak = 0;
        let mut next_tick = Instant::now() + Duration::from_millis(500);
        loop {
            let wait = unsafe {
                MsgWaitForMultipleObjects(1, &process.as_raw_handle(), 0, 500, QS_ALLINPUT)
            };
            if wait == WAIT_OBJECT_0 {
                break;
            }
            ensure!(wait != WAIT_FAILED, "Keeper process/message wait failed");
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // Bound each batch so shell traffic cannot starve the exact process-exit wait.
            for _ in 0..256 {
                if unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if msg.message == message
                    && msg.wParam & 0xffff == 0x8006
                    && process_of(msg.lParam as HWND) == identity.pid
                {
                    flash_stop(msg.lParam as HWND);
                }
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if Instant::now() >= next_tick {
                if tick(identity.pid)? {
                    streak += 1;
                } else {
                    streak = 0;
                }
                next_tick = Instant::now()
                    + if streak >= 10 {
                        streak = 0;
                        Duration::from_secs(60)
                    } else {
                        Duration::from_millis(500)
                    };
            }
        }
        Ok(())
    }
    pub(crate) async fn start(profile: &ProfileRegistry, chrome: &ProcessIdentity) -> Result<()> {
        profile.check()?;
        match NativeEntry::open(&profile.path().join(RECORD), false) {
            Ok(mut entry) => {
                ensure!(
                    entry.handle.metadata()?.len() <= 64 * 1024,
                    "Keeper record exceeds its limit"
                );
                let mut bytes = Vec::new();
                Read::by_ref(&mut entry.handle)
                    .take(64 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 64 * 1024,
                    "Keeper record grew beyond its limit"
                );
                entry.require_matches(&profile.path().join(RECORD))?;
                let record: Value = serde_json::from_slice(&bytes)?;
                ensure!(record["version"] == 1, "Unknown keeper record version");
                let keeper: ProcessIdentity = serde_json::from_value(record["keeper"].clone())?;
                if record["chrome"] == json!(chrome) {
                    match keeper.liveness() {
                        Liveness::Alive => return Ok(()),
                        Liveness::Unknown => anyhow::bail!("Existing keeper identity is unknown"),
                        Liveness::Dead => {}
                    }
                }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let ready = profile
            .path()
            .join(format!(".keeper-{}.ready", uuid::Uuid::new_v4()));
        let process = {
            let executable = crate::process::cli_executable()?;
            let file: Vec<u16> = executable
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            let directory: Vec<u16> = profile
                .path()
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            let parameters = wide(&format!(
                "--exec-window-keeper \"{}\" --keeper-ready \"{}\"",
                serde_json::to_string(chrome)?.replace('"', "\\\""),
                ready.display()
            ));
            let mut launch = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                lpFile: file.as_ptr(),
                lpParameters: parameters.as_ptr(),
                lpDirectory: directory.as_ptr(),
                nShow: SW_HIDE,
                ..Default::default()
            };
            ensure!(
                unsafe { ShellExecuteExW(&mut launch) } != 0,
                "Unable to launch Rust window keeper: {}",
                std::io::Error::last_os_error()
            );
            ensure!(!launch.hProcess.is_null(), "Keeper process handle missing");
            unsafe { OwnedHandle::from_raw_handle(launch.hProcess) }
        };
        let mut startup = Startup {
            process,
            ready: ready.clone(),
            armed: true,
        };
        let setup = async {
            let identity =
                ProcessIdentity::read(unsafe { GetProcessId(startup.process.as_raw_handle()) })?
                    .context("Keeper native identity missing")?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match NativeEntry::open(&ready, false) {
                    Ok(mut entry) => {
                        ensure!(
                            entry.handle.metadata()?.len() <= 64 * 1024,
                            "Keeper readiness exceeds its limit"
                        );
                        let mut bytes = Vec::new();
                        Read::by_ref(&mut entry.handle)
                            .take(64 * 1024 + 1)
                            .read_to_end(&mut bytes)?;
                        ensure!(
                            bytes.len() <= 64 * 1024,
                            "Keeper readiness grew beyond its limit"
                        );
                        entry.require_matches(&ready)?;
                        let record: Value = serde_json::from_slice(&bytes)?;
                        ensure!(
                            record == json!({"keeper":identity,"chrome":chrome}),
                            "Keeper readiness identity mismatch"
                        );
                        std::fs::remove_file(&ready)?;
                        break;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
                ensure!(
                    Instant::now() < deadline
                        && unsafe { WaitForSingleObject(startup.process.as_raw_handle(), 0) }
                            != WAIT_OBJECT_0,
                    "Keeper did not acknowledge readiness"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            ensure!(
                identity.liveness() == Liveness::Alive && chrome.liveness() == Liveness::Alive,
                "Keeper/browser identity changed during startup"
            );
            profile.check()?;
            atomic_json(
                &profile.path().join(RECORD),
                &json!({"version":1,"keeper":identity,"chrome":chrome}),
            )?;
            profile.check()?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        startup.armed = setup.is_err();
        setup
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn overlap_requires_area_and_allows_negative_monitor_coordinates() {
            let left = RECT {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080,
            };
            assert!(!overlaps(
                &left,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080
                }
            ));
            assert!(overlaps(
                &left,
                &RECT {
                    left: -10,
                    top: 10,
                    right: 100,
                    bottom: 100
                }
            ));
        }
    }
}
