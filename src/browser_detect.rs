//! Native browser discovery shared by execution and bridge diagnostics.
use crate::{config::Environment, policy};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", test))]
#[path = "browser_desktop.rs"]
mod desktop;
#[cfg(any(target_os = "macos", test))]
#[path = "browser_macos.rs"]
mod macos;

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
pub fn chrome(environment: &Environment) -> Option<PathBuf> {
    if let Some(path) = environment
        .get("CHROME_PATH")
        .map(|path| PathBuf::from(policy::trim(path)))
        .filter(|path| executable(path))
    {
        return Some(path);
    }
    if let Some(path) = environment
        .get("LIGHTHOUSE_CHROMIUM_PATH")
        .map(PathBuf::from)
        .filter(|path| executable(path))
    {
        return Some(path);
    }
    #[cfg(target_os = "linux")]
    if let Some(path) = desktop::chrome(environment) {
        return Some(path);
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = macos::chrome(environment) {
        return Some(path);
    }
    candidates(environment)
        .into_iter()
        .find(|path| executable(path))
}
fn candidates(environment: &Environment) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        for root in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(|key| environment.get(key))
        {
            for suffix in [
                "Google/Chrome SxS/Application/chrome.exe",
                "Google/Chrome/Application/chrome.exe",
            ] {
                paths.push(PathBuf::from(root).join(suffix));
            }
        }
        let program = environment
            .get("ProgramFiles")
            .map(String::as_str)
            .unwrap_or("C:\\Program Files");
        let program_x86 = environment
            .get("ProgramFiles(x86)")
            .map(String::as_str)
            .unwrap_or("C:\\Program Files (x86)");
        for root in [program, program_x86] {
            paths.push(PathBuf::from(root).join("Google/Chrome/Application/chrome.exe"));
        }
        if let Some(home) = environment
            .get("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                environment
                    .get("USERPROFILE")
                    .map(|home| PathBuf::from(home).join("AppData/Local"))
            })
        {
            paths.push(home.join("Google/Chrome/Application/chrome.exe"));
        }
        for root in [program, program_x86] {
            paths.push(PathBuf::from(root).join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.extend(
            [
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            ]
            .map(PathBuf::from),
        );
    }
    #[cfg(target_os = "linux")]
    {
        paths.extend(
            [
                "/usr/bin/google-chrome",
                "/usr/bin/google-chrome-stable",
                "/usr/bin/google-chrome-beta",
                "/usr/bin/google-chrome-unstable",
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/usr/bin/brave-browser",
                "/usr/bin/microsoft-edge",
                "/usr/bin/microsoft-edge-stable",
                "/snap/bin/chromium",
                "/snap/bin/brave",
                "/snap/bin/brave-browser",
                "/snap/bin/microsoft-edge",
                "/opt/google/chrome/chrome",
            ]
            .map(PathBuf::from),
        );
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
            "microsoft-edge-stable",
        ] {
            for dir in
                std::env::split_paths(environment.get("PATH").map(String::as_str).unwrap_or(""))
            {
                if !dir.as_os_str().is_empty() {
                    paths.push(dir.join(name));
                }
            }
        }
    }
    paths
}
pub fn cookie_db(environment: &Environment) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let _ = environment;
        None
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(environment.get("HOME")?);
        #[cfg(target_os = "linux")]
        let roots = [
            ".config/google-chrome",
            ".config/chromium",
            ".config/microsoft-edge",
            ".config/BraveSoftware/Brave-Browser",
            "snap/chromium/common/chromium",
            "snap/chromium/current/chromium",
        ];
        #[cfg(not(target_os = "linux"))]
        let roots = [
            "Library/Application Support/Google/Chrome",
            "Library/Application Support/Chromium",
            "Library/Application Support/Microsoft Edge",
            "Library/Application Support/BraveSoftware/Brave-Browser",
        ];
        roots
            .into_iter()
            .flat_map(|root| {
                [
                    home.join(root).join("Default/Cookies"),
                    home.join(root).join("Default/Network/Cookies"),
                ]
            })
            .find(|path| path.is_file())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_launcher_override_follows_valid_chrome_path_and_survives_missing_primary() {
        let home = tempfile::tempdir().unwrap();
        let primary = home.path().join("PUBLIC-primary");
        let legacy = home.path().join("PUBLIC-legacy");
        for path in [&primary, &legacy] {
            std::fs::write(path, b"PUBLIC executable fixture").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let mut environment = Environment::from_iter([
            ("CHROME_PATH".into(), primary.to_string_lossy().into_owned()),
            (
                "LIGHTHOUSE_CHROMIUM_PATH".into(),
                legacy.to_string_lossy().into_owned(),
            ),
        ]);
        assert_eq!(chrome(&environment), Some(primary));
        environment.insert(
            "CHROME_PATH".into(),
            home.path().to_string_lossy().into_owned(),
        );
        assert_eq!(chrome(&environment), Some(legacy));
    }
    #[cfg(windows)]
    #[test]
    fn windows_launcher_priority_includes_per_user_canary_before_system_chrome() {
        let home = tempfile::tempdir().unwrap();
        let local = home.path().join("local");
        let program = home.path().join("program");
        let x86 = home.path().join("x86");
        let env = Environment::from_iter([
            ("LOCALAPPDATA".into(), local.to_string_lossy().into_owned()),
            (
                "ProgramFiles".into(),
                program.to_string_lossy().into_owned(),
            ),
            (
                "ProgramFiles(x86)".into(),
                x86.to_string_lossy().into_owned(),
            ),
        ]);
        let paths = candidates(&env);
        assert_eq!(
            paths[0],
            local.join("Google/Chrome SxS/Application/chrome.exe")
        );
        assert_eq!(paths[1], local.join("Google/Chrome/Application/chrome.exe"));
        assert_eq!(
            paths[2],
            program.join("Google/Chrome SxS/Application/chrome.exe")
        );
        for path in [&paths[0], &paths[1], &paths[3]] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"PUBLIC synthetic executable").unwrap();
        }
        assert_eq!(chrome(&env), Some(paths[0].clone()));
        std::fs::remove_file(&paths[0]).unwrap();
        assert_eq!(chrome(&env), Some(paths[1].clone()));
    }
    #[test]
    fn explicit_executable_and_cookie_paths_use_only_the_given_environment() {
        let directory = tempfile::tempdir().unwrap();
        let executable_path = directory.path().join("PUBLIC-chrome");
        std::fs::write(&executable_path, b"PUBLIC synthetic binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let env = Environment::from_iter([
            (
                "CHROME_PATH".into(),
                executable_path.to_string_lossy().into_owned(),
            ),
            (
                "HOME".into(),
                directory.path().to_string_lossy().into_owned(),
            ),
        ]);
        assert_eq!(chrome(&env), Some(executable_path));
        assert_eq!(cookie_db(&env), None);
        #[cfg(target_os = "linux")]
        {
            let path = directory
                .path()
                .join(".config/chromium/Default/Network/Cookies");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"PUBLIC synthetic path fixture").unwrap();
            assert_eq!(cookie_db(&env), Some(path));
        }
    }
}
