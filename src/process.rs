//! Native process identity. Unknown evidence never authorises cleanup.
#[cfg(not(windows))]
use anyhow::bail;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Keeps this process's own standard handles out of every child it starts. Windows children
/// inherit all inheritable handles, so a detached worker or retained Chrome would otherwise hold
/// the caller's pipes open after this process exits; std duplicates inherited stdio per child.
/// Unix descriptors are already close-on-exec.
pub fn private_std_handles() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation},
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle borrows this process's handle; only its inherit flag changes.
            unsafe {
                let handle = GetStdHandle(id);
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at_ms: f64,
    #[serde(with = "native_ticks")]
    pub native_start: u64,
}

mod native_ticks {
    use serde::{
        Deserializer, Serializer,
        de::{Error, Visitor},
    };
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct ExactStart;
        impl Visitor<'_> for ExactStart {
            type Value = u64;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a decimal native start string or a safe legacy integer")
            }
            fn visit_str<E: Error>(self, value: &str) -> Result<u64, E> {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(E::custom("Invalid native process start identity"));
                }
                value.parse().map_err(E::custom)
            }
            fn visit_u64<E: Error>(self, value: u64) -> Result<u64, E> {
                if value > 9_007_199_254_740_991 {
                    return Err(E::custom(
                        "Numeric native identity is ambiguous across legacy readers",
                    ));
                }
                Ok(value)
            }
        }
        deserializer.deserialize_any(ExactStart)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Liveness {
    Alive,
    Dead,
    Unknown,
}

impl ProcessIdentity {
    pub fn current() -> Result<Self> {
        Self::read(std::process::id())?.context("Unable to establish current process identity")
    }
    pub fn liveness(&self) -> Liveness {
        if self.pid == 0
            || self.native_start == 0
            || !self.started_at_ms.is_finite()
            || self.started_at_ms <= 0.0
        {
            return Liveness::Unknown;
        }
        match Self::read(self.pid) {
            Ok(Some(actual)) if actual.native_start == self.native_start => Liveness::Alive,
            Ok(_) => Liveness::Dead,
            Err(_) => Liveness::Unknown,
        }
    }
    #[cfg(target_os = "linux")]
    pub fn read(pid: u32) -> Result<Option<Self>> {
        use std::fs;
        let raw = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // comm can contain spaces and parentheses. Fields following its last ')' start at 3.
        let tail = raw
            .rsplit_once(')')
            .context("Malformed native process record")?
            .1;
        let fields: Vec<_> = tail.split_whitespace().collect();
        let native_start: u64 = fields
            .get(19)
            .context("Missing process start time")?
            .parse()?;
        if matches!(fields.first().copied(), Some("Z" | "X")) {
            return Ok(None);
        }
        let boot: u64 = fs::read_to_string("/proc/stat")?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .context("Missing boot time")?
            .parse()?;
        // SAFETY: sysconf only reads the system clock tick constant.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks <= 0 {
            bail!("Unable to determine native clock tick rate");
        }
        Ok(Some(Self {
            pid,
            native_start,
            // The maintained ps/Date.parse reader exposes whole seconds. Exact ticks
            // remain separate so new native owners still distinguish PID reuse.
            started_at_ms: (boot + native_start / ticks as u64) as f64 * 1000.0,
        }))
    }
    #[cfg(windows)]
    pub fn read(pid: u32) -> Result<Option<Self>> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FILETIME},
            System::Threading::{
                GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        // SAFETY: the process handle is closed on every path; all output pointers are valid.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(87) {
                    return Ok(None);
                }
                return Err(error.into());
            }
            let mut code = 0;
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            let ok = GetExitCodeProcess(handle, &mut code) != 0
                && GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
            let error = std::io::Error::last_os_error();
            CloseHandle(handle);
            if !ok {
                return Err(error.into());
            }
            if code != 259 {
                return Ok(None);
            }
            let native_start =
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
            Ok(Some(Self {
                pid,
                native_start,
                // Date.parse discards the sub-millisecond portion of the OS timestamp.
                started_at_ms: ((native_start - 116444736000000000) / 10000) as f64,
            }))
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    pub fn read(_pid: u32) -> Result<Option<Self>> {
        bail!("Native process identity is not qualified on this platform")
    }
}

/// The CLI beside the running executable, so a renamed pair of binaries stays paired: a stem
/// ending in `-mcp` (any case) names its CLI sibling (`industrious-mcp.exe` finds
/// `industrious.exe`, `industrious-mcp` finds `industrious`); any other executable is the CLI itself.
/// PATH is never searched, so another installed build is never picked up.
pub fn cli_executable() -> Result<std::path::PathBuf> {
    let current = std::env::current_exe().context("The running executable is unknown")?;
    let cli = cli_sibling(&current);
    anyhow::ensure!(
        cli.is_file(),
        "The CLI {} is missing beside {}; install both binaries in one folder.",
        cli.display(),
        current.display()
    );
    Ok(cli)
}
fn cli_sibling(executable: &std::path::Path) -> std::path::PathBuf {
    let stem = executable.file_stem().and_then(|stem| stem.to_str());
    let Some(cli) = stem.and_then(|stem| {
        let at = stem.len().checked_sub(4).filter(|&at| at > 0)?;
        (stem.is_char_boundary(at) && stem[at..].eq_ignore_ascii_case("-mcp")).then(|| &stem[..at])
    }) else {
        return executable.to_owned();
    };
    let mut name = std::ffi::OsString::from(cli);
    if let Some(extension) = executable.extension() {
        name.push(".");
        name.push(extension);
    }
    executable.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::cli_sibling;
    use std::path::{Path, PathBuf};

    #[test]
    fn cli_sibling_strips_a_trailing_mcp_and_keeps_the_folder_and_extension() {
        for (executable, cli) in [
            ("dir/industrious-mcp.exe", "dir/industrious.exe"),
            ("dir/industrious-mcp.exe", "dir/industrious.exe"),
            ("dir/Industrious-MCP.EXE", "dir/Industrious.EXE"),
            ("dir/industrious-mcp", "dir/industrious"),
            ("dir/industrious.exe", "dir/industrious.exe"),
            ("dir/industrious", "dir/industrious"),
            ("dir/-mcp.exe", "dir/-mcp.exe"),
            ("dir/mcp.exe", "dir/mcp.exe"),
            ("dir/industrious-mcp.v2.exe", "dir/industrious-mcp.v2.exe"),
        ] {
            assert_eq!(
                cli_sibling(Path::new(executable)),
                PathBuf::from(cli),
                "{executable}"
            );
        }
    }
}
