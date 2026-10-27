//! Linux discovery adapted from chrome-launcher 1.2.1, Copyright 2016 Google Inc.
//! Apache-2.0; see data/LICENSE.chrome-launcher. No shell or desktop command is executed.
use crate::config::Environment;
use std::path::{Path, PathBuf};

fn desktop_exec(line: &str) -> Option<PathBuf> {
    let value = line.strip_prefix("Exec=")?;
    if !value.starts_with('/')
        || !["/google-chrome-", "/chrome-", "/chromium-"]
            .iter()
            .any(|prefix| value.contains(prefix))
    {
        return None;
    }
    Some(PathBuf::from(value.split('=').next()?.split(' ').next()?))
}
fn priority(path: &Path, environment: &Environment) -> i32 {
    let raw = path.to_string_lossy();
    for (key, weight) in [("CHROME_PATH", 101), ("LIGHTHOUSE_CHROMIUM_PATH", 100)] {
        if environment
            .get(key)
            .is_some_and(|value| !value.is_empty() && raw.contains(value))
        {
            return weight;
        }
    }
    for (suffix, weight) in [
        ("chrome-wrapper", 51),
        ("google-chrome-stable", 50),
        ("google-chrome", 49),
        ("chromium-browser", 48),
        ("chromium", 47),
    ] {
        if raw.ends_with(suffix) {
            return weight;
        }
    }
    10
}
#[cfg(target_os = "linux")]
fn open_child(
    directory: &crate::native_fs::NativeEntry,
    name: &std::ffi::OsStr,
    is_directory: bool,
) -> Option<crate::native_fs::NativeEntry> {
    use std::os::fd::AsRawFd;
    directory
        .open_child(
            Path::new(&format!("/proc/self/fd/{}", directory.handle.as_raw_fd())),
            name,
            is_directory,
        )
        .ok()
}
#[cfg(target_os = "linux")]
fn read_desktop(
    directory: &crate::native_fs::NativeEntry,
    name: &std::ffi::OsStr,
) -> Option<String> {
    use std::io::Read;
    let mut entry = open_child(directory, name, false)?;
    if entry.handle.metadata().ok()?.len() > 1024 * 1024 {
        return None;
    }
    let mut text = String::new();
    entry
        .handle
        .by_ref()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= 1024 * 1024).then_some(text)
}
#[cfg(target_os = "linux")]
fn collect_open(
    directory: &crate::native_fs::NativeEntry,
    paths: &mut Vec<PathBuf>,
    remaining: &mut usize,
    deadline: std::time::Instant,
    depth: usize,
) {
    use std::os::fd::AsRawFd;
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", directory.handle.as_raw_fd()))
    else {
        return;
    };
    for entry in entries.flatten() {
        if *remaining == 0 || std::time::Instant::now() >= deadline {
            return;
        }
        *remaining -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        if kind.is_dir()
            && let Some(child) = open_child(directory, &name, true)
        {
            collect_open(&child, paths, remaining, deadline, depth + 1);
        }
        // shortcut: inspect regular desktop entries only; extend for a qualified nonstandard installation.
        if !kind.is_file()
            || Path::new(&name)
                .extension()
                .is_none_or(|extension| extension != "desktop")
        {
            continue;
        }
        let Some(text) = read_desktop(directory, &name) else {
            continue;
        };
        paths.extend(
            text.lines()
                .filter_map(desktop_exec)
                .filter(|path| super::executable(path)),
        );
    }
}
#[cfg(target_os = "linux")]
fn discover(environment: &Environment, roots: &[PathBuf]) -> Option<PathBuf> {
    let mut paths = Vec::new();
    let mut remaining = 4096;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    for root in roots {
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(handle) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root)
        else {
            continue;
        };
        if let Ok(directory) = crate::native_fs::NativeEntry::from_handle(handle, true) {
            collect_open(&directory, &mut paths, &mut remaining, deadline, 0);
        }
    }
    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
    ] {
        if let Some(path) =
            std::env::split_paths(environment.get("PATH").map(String::as_str).unwrap_or(""))
                .filter(|directory| !directory.as_os_str().is_empty())
                .map(|directory| directory.join(name))
                .find(|path| super::executable(path))
        {
            paths.push(path);
        }
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.as_os_str().to_owned()));
    paths.sort_by_key(|path| std::cmp::Reverse(priority(path, environment)));
    paths.into_iter().next()
}
#[cfg(target_os = "linux")]
pub(super) fn chrome(environment: &Environment) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = environment.get("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/applications"));
    }
    roots.push(PathBuf::from("/usr/share/applications"));
    discover(environment, &roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_parser_matches_the_pinned_narrow_exec_rule() {
        for (line, expected) in [
            (
                "Exec=/opt/google/google-chrome-stable %U",
                "/opt/google/google-chrome-stable",
            ),
            (
                "Exec=/opt/chrome/chrome-wrapper --flag",
                "/opt/chrome/chrome-wrapper",
            ),
            ("Exec=/opt/chromium-beta\t%U", "/opt/chromium-beta\t%U"),
            (
                "Exec=/opt/chrome-wrapper=value --flag",
                "/opt/chrome-wrapper",
            ),
            ("Exec=/opt/a b/chrome-wrapper %U", "/opt/a"),
        ] {
            assert_eq!(desktop_exec(line), Some(PathBuf::from(expected)), "{line}");
        }
        for line in [
            " Exec=/opt/chrome-wrapper",
            "Exec=google-chrome-stable",
            "Exec=\"/opt/chrome-wrapper\" %U",
            "Exec=/opt/google-chrome",
            "Exec=env /opt/chrome-wrapper",
        ] {
            assert_eq!(desktop_exec(line), None, "{line}");
        }
    }
    #[test]
    fn launcher_priority_keeps_environment_then_wrapper_then_browser_channels() {
        let env = Environment::from_iter([
            ("CHROME_PATH".into(), "/PUBLIC-first".into()),
            ("LIGHTHOUSE_CHROMIUM_PATH".into(), "/PUBLIC-second".into()),
        ]);
        for (path, rank) in [
            ("/PUBLIC-first/chromium", 101),
            ("/PUBLIC-second/chromium", 100),
            ("/opt/chrome-wrapper", 51),
            ("/opt/google-chrome-stable", 50),
            ("/opt/google-chrome", 49),
            ("/opt/chromium-browser", 48),
            ("/opt/chromium", 47),
            ("/opt/chrome-beta", 10),
        ] {
            assert_eq!(priority(Path::new(path), &env), rank);
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn recursive_desktop_wrapper_precedes_path_stable_and_equal_rank_preserves_user_order() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let user = home.path().join("user");
        let system = home.path().join("system");
        let bin = home.path().join("bin");
        for path in [&user.join("nested"), &system, &bin] {
            std::fs::create_dir_all(path).unwrap();
        }
        let wrapper = home.path().join("chrome-wrapper");
        let stable = bin.join("google-chrome-stable");
        let user_beta = home.path().join("chrome-beta");
        let system_beta = home.path().join("chromium-beta");
        for path in [&wrapper, &stable, &user_beta, &system_beta] {
            std::fs::write(path, b"PUBLIC executable fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(
            user.join("nested/browser.desktop"),
            format!("[Desktop Entry]\r\nExec={} %U\r\n", wrapper.display()),
        )
        .unwrap();
        std::fs::write(
            user.join("beta.desktop"),
            format!("Exec={} %U\n", user_beta.display()),
        )
        .unwrap();
        std::fs::write(
            system.join("beta.desktop"),
            format!("Exec={} %U\n", system_beta.display()),
        )
        .unwrap();
        let mut environment =
            Environment::from_iter([("PATH".into(), bin.to_string_lossy().into_owned())]);
        let roots = [user.clone(), system];
        assert_eq!(discover(&environment, &roots), Some(wrapper));
        std::fs::remove_file(user.join("nested/browser.desktop")).unwrap();
        assert_eq!(discover(&environment, &roots), Some(stable));
        environment.insert("PATH".into(), String::new());
        assert_eq!(discover(&environment, &roots), Some(user_beta));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_preserves_non_executable_symlinked_and_non_desktop_inputs() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = tempfile::tempdir().unwrap();
        let executable = home.path().join("chrome-wrapper");
        std::fs::write(&executable, b"PUBLIC executable fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let text = format!("Exec={} %U\n", executable.display());
        let outside = home.path().join("outside.desktop");
        std::fs::write(&outside, &text).unwrap();
        let applications = home.path().join("applications");
        std::fs::create_dir(&applications).unwrap();
        std::fs::write(applications.join("ignored.txt"), &text).unwrap();
        symlink(&outside, applications.join("linked.desktop")).unwrap();
        let before = std::fs::read(&outside).unwrap();
        assert_eq!(
            discover(&Environment::new(), std::slice::from_ref(&applications)),
            None
        );
        std::fs::write(applications.join("owned.desktop"), &text).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(discover(&Environment::new(), &[applications]), None);
        assert_eq!(std::fs::read(outside).unwrap(), before);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn entry_replaced_after_enumeration_cannot_follow_links_or_block_on_a_fifo() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let home = tempfile::tempdir().unwrap();
        let name = std::ffi::OsStr::new("browser.desktop");
        let path = home.path().join(name);
        std::fs::write(&path, "PUBLIC original desktop").unwrap();
        let kind = std::fs::read_dir(home.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_type()
            .unwrap();
        assert!(kind.is_file());
        let directory = crate::native_fs::NativeEntry::open(home.path(), true).unwrap();
        std::fs::remove_file(&path).unwrap();
        let native_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path belongs to this fixture and no existing entry is replaced.
        assert_eq!(unsafe { libc::mkfifo(native_path.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(read_desktop(&directory, name).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(&path).unwrap();
        let target = home.path().join("PUBLIC-target");
        std::fs::write(&target, "PUBLIC replacement must stay unread").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_desktop(&directory, name).is_none());
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "PUBLIC replacement must stay unread"
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn directory_replacement_does_not_redirect_a_pinned_desktop_read() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let applications = home.path().join("applications");
        let replacement = home.path().join("replacement");
        for path in [&applications, &replacement] {
            std::fs::create_dir(path).unwrap();
        }
        let name = std::ffi::OsStr::new("browser.desktop");
        std::fs::write(applications.join(name), "PUBLIC original desktop").unwrap();
        std::fs::write(replacement.join(name), "PUBLIC replacement desktop").unwrap();
        let directory = crate::native_fs::NativeEntry::open(&applications, true).unwrap();
        std::fs::rename(&applications, home.path().join("original-applications")).unwrap();
        symlink(&replacement, &applications).unwrap();
        assert_eq!(
            read_desktop(&directory, name).as_deref(),
            Some("PUBLIC original desktop")
        );
        assert_eq!(
            std::fs::read_to_string(replacement.join(name)).unwrap(),
            "PUBLIC replacement desktop"
        );
    }
}
