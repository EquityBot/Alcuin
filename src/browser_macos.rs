//! LaunchServices discovery adapted from chrome-launcher 1.2.1, Copyright 2016 Google Inc.
//! Apache-2.0; see data/LICENSE.chrome-launcher. Registry output never becomes a shell command.
use crate::config::Environment;
use std::path::{Path, PathBuf};

fn installations(dump: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in dump.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("google chrome.app") && !lower.contains("google chrome canary.app") {
            continue;
        }
        let value = line
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ");
        let Some(end) = value.find(".app") else {
            continue;
        };
        let bundle = Path::new(value[..end + 4].trim());
        for suffix in [
            "Contents/MacOS/Google Chrome Canary",
            "Contents/MacOS/Google Chrome",
        ] {
            let path = bundle.join(suffix);
            if seen.insert(path.as_os_str().to_owned()) {
                paths.push(path);
            }
        }
    }
    paths
}
fn app_pattern(raw: &str, canary: bool, wildcard_dot: bool) -> bool {
    let label = if canary { "Chrome Canary" } else { "Chrome" }
        .encode_utf16()
        .collect::<Vec<_>>();
    let text = raw.encode_utf16().collect::<Vec<_>>();
    text.windows(label.len() + 4).any(|part| {
        part[..label.len()] == label
            && part[label.len() + 1..] == [97, 112, 112]
            && if wildcard_dot {
                ![10, 13, 0x2028, 0x2029].contains(&part[label.len()])
            } else {
                part[label.len()] == 46
            }
    })
}
fn priority(path: &Path, environment: &Environment) -> i32 {
    let raw = path.to_string_lossy();
    for (key, weight) in [("CHROME_PATH", 151), ("LIGHTHOUSE_CHROMIUM_PATH", 150)] {
        if environment
            .get(key)
            .is_some_and(|value| !value.is_empty() && raw.contains(value))
        {
            return weight;
        }
    }
    for (prefix, stable, canary, wildcard) in [
        (
            environment
                .get("HOME")
                .map(|home| format!("{home}/Applications/")),
            50,
            51,
            false,
        ),
        (Some("/Applications/".into()), 100, 101, true),
        (Some("/Volumes/".into()), -2, -1, true),
    ] {
        if prefix
            .as_ref()
            .is_some_and(|prefix| raw.starts_with(prefix))
        {
            if app_pattern(&raw, false, wildcard) {
                return stable;
            }
            if app_pattern(&raw, true, wildcard) {
                return canary;
            }
        }
    }
    10
}
fn with_dump(
    environment: &Environment,
    fast: &[PathBuf],
    dump: impl FnOnce() -> Option<String>,
) -> Option<PathBuf> {
    if let Some(path) = fast.iter().find(|path| super::executable(path)) {
        return Some(path.clone());
    }
    let mut paths = installations(&dump()?);
    paths.sort_by_key(|path| std::cmp::Reverse(priority(path, environment)));
    paths.into_iter().find(|path| super::executable(path))
}
#[cfg(target_os = "macos")]
pub(super) fn chrome(environment: &Environment) -> Option<PathBuf> {
    let fast = [
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    .map(PathBuf::from);
    with_dump(environment, &fast, || {
        let mut command = std::process::Command::new(
            "/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister",
        );
        command.arg("-dump").env_clear();
        if let Some(home) = environment.get("HOME") {
            command.env("HOME", home);
        }
        bounded_output(command, std::time::Duration::from_secs(5), 16 * 1024 * 1024)
    })
}
#[cfg(unix)]
fn bounded_output(
    mut command: std::process::Command,
    timeout: std::time::Duration,
    limit: usize,
) -> Option<String> {
    use std::{io::Read, os::fd::AsRawFd, process::Stdio, time::Instant};
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let result = (|| {
        let mut stdout = child.stdout.take()?;
        // SAFETY: stdout owns this live pipe descriptor; only its nonblocking flag changes.
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return None;
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut eof = false;
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            while !eof {
                if Instant::now() >= deadline {
                    return None;
                }
                match stdout.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        if output.len().checked_add(count)? > limit {
                            return None;
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            }
            if status.is_none() {
                status = child.try_wait().ok()?;
            }
            if let Some(status) = status {
                if !status.success() {
                    return None;
                }
                if eof {
                    return String::from_utf8(output).ok();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    })();
    if result.is_none() {
        let _ = child.kill();
    }
    child.wait().ok()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_services_paths_normalise_spaces_deduplicate_and_preserve_suffix_order() {
        let dump = "path: /PUBLIC/Google  Chrome.app\npath: /PUBLIC/Google Chrome.app\npath: /Applications/Google Chrome Canary.app/ignored\npath: /PUBLIC/Other.app\npath: /PUBLIC/GOOGLE CHROME.APP\n";
        assert_eq!(
            installations(dump),
            [
                "/PUBLIC/Google Chrome.app/Contents/MacOS/Google Chrome Canary",
                "/PUBLIC/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome",
            ]
            .map(PathBuf::from)
        );
    }
    #[test]
    fn launch_services_priorities_match_environment_system_home_unknown_and_volume_tiers() {
        let environment = Environment::from_iter([
            ("HOME".into(), "/Users/PUBLIC".into()),
            ("CHROME_PATH".into(), "/PUBLIC-first".into()),
            ("LIGHTHOUSE_CHROMIUM_PATH".into(), "/PUBLIC-second".into()),
        ]);
        for (path, expected) in [
            ("/PUBLIC-first", 151),
            ("/PUBLIC-second", 150),
            ("/Applications/Google Chrome Canary.app/Contents", 101),
            ("/Applications/Google Chrome.app/Contents", 100),
            (
                "/Users/PUBLIC/Applications/Google Chrome Canary.app/Contents",
                51,
            ),
            ("/Users/PUBLIC/Applications/Google Chrome.app/Contents", 50),
            ("/PUBLIC/Google Chrome.app", 10),
            ("/Volumes/Google Chrome Canary.app", -1),
            ("/Volumes/Google Chrome.app", -2),
            ("/Applications/Google ChromeXapp.app", 100),
        ] {
            assert_eq!(priority(Path::new(path), &environment), expected, "{path}");
        }
    }
    #[test]
    fn fast_candidates_skip_registry_and_slow_candidates_require_executable_files() {
        let home = tempfile::tempdir().unwrap();
        let executable = home
            .path()
            .join("Google Chrome.app/Contents/MacOS/Google Chrome");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"PUBLIC executable fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            with_dump(
                &Environment::new(),
                std::slice::from_ref(&executable),
                || panic!("Fast discovery invoked LaunchServices")
            ),
            Some(executable.clone())
        );
        let dump = format!(
            "path: {}\n",
            home.path().join("Google Chrome.app").display()
        );
        assert_eq!(
            with_dump(&Environment::new(), &[], || Some(dump)),
            Some(executable)
        );
        assert_eq!(with_dump(&Environment::new(), &[], || None), None);
        assert_eq!(
            with_dump(&Environment::new(), &[], || Some(String::new())),
            None
        );
    }
    #[cfg(unix)]
    #[test]
    fn registry_child_output_is_bounded_and_owned_child_is_reaped_on_every_exit() {
        let command = || {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.arg("--list").env_clear();
            command
        };
        let output =
            bounded_output(command(), std::time::Duration::from_secs(5), 1024 * 1024).unwrap();
        assert!(output.contains("browser_detect::macos::tests"));
        assert!(bounded_output(command(), std::time::Duration::from_secs(5), 1).is_none());
        assert!(bounded_output(command(), std::time::Duration::ZERO, 1024 * 1024).is_none());
    }
}
