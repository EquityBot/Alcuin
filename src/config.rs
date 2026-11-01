use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub type Environment = BTreeMap<String, String>;
pub fn oracle_home(env: &Environment) -> Result<PathBuf> {
    if let Some(path) = env.get("ORACLE_HOME_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = if cfg!(windows) {
        env.get("USERPROFILE")
    } else {
        env.get("HOME")
    };
    Ok(
        PathBuf::from(home.context("Unable to resolve home directory; set ORACLE_HOME_DIR")?)
            .join(".oracle"),
    )
}
/// Set by synthetic proofs so a missing profile option can never reach the signed-in profile.
pub const REFUSE_DEFAULT_PROFILE_ENV: &str = "ORACLE_REFUSE_DEFAULT_BROWSER_PROFILE";
/// The first non-blank candidate, else `<home>/.oracle/browser-profile`, which
/// `ORACLE_REFUSE_DEFAULT_BROWSER_PROFILE=1` refuses.
pub fn manual_login_profile_dir<'a>(
    candidates: impl IntoIterator<Item = Option<&'a str>>,
    env: &Environment,
    home: &str,
) -> Result<PathBuf> {
    if let Some(dir) = candidates
        .into_iter()
        .flatten()
        .map(crate::policy::trim)
        .find(|dir| !dir.is_empty())
    {
        return Ok(PathBuf::from(dir));
    }
    anyhow::ensure!(
        env.get(REFUSE_DEFAULT_PROFILE_ENV).map(String::as_str) != Some("1"),
        "{REFUSE_DEFAULT_PROFILE_ENV}=1 refuses the default signed-in browser profile; pass an explicit profile directory."
    );
    Ok(PathBuf::from(home).join(".oracle/browser-profile"))
}
pub fn runtime_environment() -> Environment {
    // Read application variables by name; never enumerate unrelated credentials.
    [
        "ORACLE_HOME_DIR",
        REFUSE_DEFAULT_PROFILE_ENV,
        "HOME",
        "USERPROFILE",
        "PATH",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "APPDATA",
        "LANG",
        "LC_ALL",
        "TEMP",
        "TMP",
        "TMPDIR",
        "ORACLE_ENGINE",
        "ORACLE_BROWSER_PROFILE_DIR",
        "ORACLE_BROWSER_PORT",
        "ORACLE_BROWSER_DEBUG_PORT",
        "ORACLE_BROWSER_ALLOW_COOKIE_ERRORS",
        "ORACLE_BROWSER_MAX_CONCURRENT_TABS",
        "ORACLE_MCP_ALLOW_EXTERNAL_INPUT",
        "ORACLE_MAX_FILE_SIZE_BYTES",
        "ORACLE_MIN_PROMPT_CHARS",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENROUTER_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "AZURE_OPENAI_ENDPOINT",
        "AZURE_OPENAI_DEPLOYMENT",
        "AZURE_OPENAI_API_VERSION",
        "ORACLE_REMOTE_HOST",
        "ORACLE_REMOTE_TOKEN",
        "ORACLE_BROWSER_APPROVAL_WAIT",
        "ORACLE_BROWSER_ATTACHMENT_TIMEOUT",
        "ORACLE_FORCE_TUI",
        "ORACLE_BROWSER_COOKIE_NAMES",
        "ORACLE_COOKIE_LOAD_TIMEOUT_MS",
        "SWEET_COOKIE_LINUX_KEYRING",
        "CHROME_PATH",
        "LIGHTHOUSE_CHROMIUM_PATH",
        "ORACLE_CHROME_NO_SANDBOX",
        "DISPLAY",
        "XAUTHORITY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSH_AUTH_SOCK",
        "ORACLE_DETACHED_START_GATE",
        "ORACLE_BRIDGE_START_GATE",
        "ORACLE_NO_DETACH",
        "ORACLE_NOTIFY",
        "ORACLE_PERF_TRACE",
        "ORACLE_BAR_CONFIRM_CYCLES",
        "ORACLE_TERMINAL_MIN_STABLE_MS",
        "ORACLE_NOTIFY_SOUND",
        "ORACLE_DISABLE_NOTIFICATIONS",
        "CI",
        "SSH_CONNECTION",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
    .collect()
}
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    let (Some(base), Some(overlay)) = (base.as_object(), overlay.as_object()) else {
        return overlay.clone();
    };
    let mut output = base.clone();
    for (key, value) in overlay {
        output.insert(
            key.clone(),
            deep_merge(output.get(key).unwrap_or(&Value::Null), value),
        );
    }
    Value::Object(output)
}
pub fn sanitise_project(config: &Value) -> Value {
    let mut output = json!({});
    for key in [
        "engine",
        "model",
        "search",
        "maxFileSizeBytes",
        "notify",
        "heartbeatSeconds",
        "filesReport",
        "background",
        "promptSuffix",
    ] {
        if let Some(value) = config.get(key) {
            output[key] = value.clone();
        }
    }
    if let Some(browser) = config.get("browser") {
        output["browser"] = json!({});
        for key in [
            "attachRunning",
            "timeoutMs",
            "inputTimeoutMs",
            "approvalWaitMs",
            "attachmentTimeoutMs",
            "assistantRecheckDelayMs",
            "assistantRecheckTimeoutMs",
            "reuseChromeWaitMs",
            "profileLockTimeoutMs",
            "maxConcurrentTabs",
            "autoReattachDelayMs",
            "autoReattachIntervalMs",
            "autoReattachTimeoutMs",
            "cookieSyncWaitMs",
            "hideWindow",
            "keepBrowser",
            "modelStrategy",
            "thinkingTime",
            "researchMode",
            "archiveConversations",
            "manualLogin",
        ] {
            if let Some(value) = browser.get(key) {
                output["browser"][key] = value.clone();
            }
        }
        let url = browser
            .get("chatgptUrl")
            .filter(|v| !v.is_null())
            .or_else(|| browser.get("url"));
        if let Some(url) = url
            && (url.is_null() || url.as_str().is_some_and(trusted_chatgpt_url))
        {
            output["browser"]["chatgptUrl"] = url.clone();
            output["browser"]["url"] = url.clone();
        }
    }
    output
}
pub fn trusted_chatgpt_url(raw: &str) -> bool {
    crate::browser::provider_url(raw).is_some()
}
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub config: Value,
    pub paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
pub fn load(
    home: &Path,
    cwd: &Path,
    user_home: Option<&Path>,
    include_project: bool,
) -> LoadedConfig {
    let user = home.join("config.json");
    let mut paths = vec![(user.clone(), false)];
    if include_project {
        let mut projects = vec![];
        for directory in cwd.ancestors() {
            if Some(directory) == user_home {
                break;
            }
            let path = directory.join(".oracle/config.json");
            if path != user {
                projects.push((path, true));
            }
        }
        projects.reverse();
        paths.extend(projects);
    }
    let mut result = LoadedConfig {
        config: json!({}),
        paths: vec![],
        warnings: vec![],
    };
    for (path, project) in paths {
        // Reference discoverProjectConfigPaths: only regular files are project candidates.
        if project {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    result.warnings.push(format!(
                        "Failed to inspect {}: configuration is unreadable",
                        path.display()
                    ));
                    continue;
                }
            }
        }
        match fs::read_to_string(&path) {
            Ok(raw) => match json5::from_str::<Value>(&raw) {
                Ok(value) => {
                    if value.is_object() {
                        result.config = deep_merge(
                            &result.config,
                            &if project {
                                sanitise_project(&value)
                            } else {
                                value
                            },
                        );
                        result.paths.push(path);
                    } else if !value.is_null() {
                        result.warnings.push(format!(
                            "Failed to read {}: config must be an object",
                            path.display()
                        ));
                    }
                }
                Err(_) => result.warnings.push(format!(
                    "Failed to read {}: invalid JSON5 configuration",
                    path.display()
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => result.warnings.push(format!(
                "Failed to read {}: configuration is unreadable",
                path.display()
            )),
        }
    }
    result
}
pub fn resolve_engine(request: Option<&str>, config: &Value, env: &Environment) -> Result<String> {
    let environment = env
        .get("ORACLE_ENGINE")
        .map(|value| crate::policy::trim(value).to_lowercase());
    let environment = environment
        .as_deref()
        .filter(|value| matches!(*value, "api" | "browser"));
    let value = request
        .or(environment)
        .or(config["engine"].as_str())
        .unwrap_or_else(|| {
            if ["OPENAI_API_KEY", "OPENROUTER_API_KEY"]
                .iter()
                .any(|key| env.get(*key).is_some_and(|value| !value.is_empty()))
            {
                "api"
            } else {
                "browser"
            }
        });
    anyhow::ensure!(
        matches!(value, "api" | "browser"),
        "Engine must be api or browser."
    );
    Ok(value.to_owned())
}

/// Canonicalise the existing prefix without accepting dangling symlinks.
pub fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut prefix = absolute.as_path();
    let mut tail = vec![];
    loop {
        match fs::canonicalize(prefix) {
            Ok(mut resolved) => {
                for segment in tail.iter().rev() {
                    resolved.push(segment);
                }
                let mut normalised = PathBuf::new();
                for component in resolved.components() {
                    match component {
                        std::path::Component::CurDir => {}
                        std::path::Component::ParentDir => {
                            normalised.pop();
                        }
                        _ => normalised.push(component),
                    }
                }
                return Ok(normalised);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(prefix).is_ok_and(|m| m.is_symlink()) {
                    anyhow::bail!(
                        "MCP output path contains an unresolved symlink: {}",
                        prefix.display()
                    );
                }
                tail.push(
                    prefix
                        .file_name()
                        .context("Unable to resolve output path")?
                        .to_owned(),
                );
                prefix = prefix.parent().context("Unable to resolve output parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
