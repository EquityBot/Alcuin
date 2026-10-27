//! Grok's manual profile never inherits ChatGPT cookies or attachment capabilities.
use super::*;
use crate::{browser_runner::BrowserRequest, config::Environment, native_fs::NativeEntry};
use std::{
    fs,
    path::{Path, PathBuf},
};

const COPIED: &[&str] = &[
    "chromePath",
    "inputTimeoutMs",
    "profileLockTimeoutMs",
    "timeoutMs",
    "hideWindow",
    "keepWindowBehind",
    "keepBrowser",
    "closeOwnedTabOnCancel",
];
const FIXED: &[&str] = &[
    "manualLogin",
    "manualLoginProfileDir",
    "cookieSync",
    "manualLoginCookieSync",
    "headless",
    "fallbackProvider",
    "maxConcurrentTabs",
];

pub(crate) fn child_config(parent: &Value, environment: &Environment, cwd: &Path) -> Result<Value> {
    let profile = candidate(
        &cwd.join(crate::config::oracle_home(environment)?)
            .join("grok-browser-profile"),
    )?;
    let mut config = json!({"manualLogin":true,"manualLoginProfileDir":profile,"cookieSync":false,
        "manualLoginCookieSync":false,"headless":false,"fallbackProvider":"none","keepBrowser":false,
        "maxConcurrentTabs":crate::browser_health::tab_limit(&parent["maxConcurrentTabs"])});
    for key in COPIED {
        if let Some(value) = parent.get(*key) {
            config[*key] = value.clone();
        }
    }
    require_config(&config)?;
    require_distinct(&config, parent, environment, cwd)?;
    Ok(config)
}

pub(crate) fn require_config(config: &Value) -> Result<()> {
    ensure!(
        config.as_object().is_some_and(|object| object
            .keys()
            .all(|key| COPIED.contains(&key.as_str()) || FIXED.contains(&key.as_str()))),
        "Grok config contains an inherited or unsupported browser capability"
    );
    ensure!(
        config["manualLogin"] == true
            && config["cookieSync"] == false
            && config["manualLoginCookieSync"] == false
            && config["headless"] == false
            && config["fallbackProvider"] == "none"
            && config["manualLoginProfileDir"]
                .as_str()
                .is_some_and(|value| !value.is_empty() && Path::new(value).is_absolute())
            && config["maxConcurrentTabs"]
                .as_u64()
                .is_some_and(|value| value > 0),
        "Grok requires its separate manual-login profile without cookie sync"
    );
    for key in COPIED {
        if let Some(value) = config.get(*key) {
            let valid = match *key {
                "chromePath" => value.as_str().is_some_and(|value| !value.is_empty()),
                "inputTimeoutMs" | "profileLockTimeoutMs" | "timeoutMs" => value.as_u64().is_some(),
                _ => value.is_boolean(),
            };
            ensure!(valid, "Invalid Grok operational setting {key}");
        }
    }
    Ok(())
}

pub(crate) fn require_text_request(request: &BrowserRequest, options: &Value) -> Result<()> {
    ensure!(
        request.upload_paths.is_empty()
            && request
                .staged_uploads
                .as_ref()
                .is_none_or(|uploads| uploads.files.is_empty())
            && request.fallback.is_none()
            && !request.fallback_used,
        "Native Grok attachments require qualification before browser allocation"
    );
    ensure!(
        crate::browser_turns::prompts(options)?.is_empty()
            && options["generateImage"].is_null()
            && options["outputPath"].is_null(),
        "Native Grok sequential turns and generated assets require qualification before allocation"
    );
    Ok(())
}

pub(crate) fn require_distinct(
    config: &Value,
    parent: &Value,
    environment: &Environment,
    cwd: &Path,
) -> Result<()> {
    require_config(config)?;
    let grok = candidate(Path::new(config["manualLoginProfileDir"].as_str().unwrap()))?;
    let copied_source = parent["copyProfileSource"]
        .as_str()
        .map(|path| cwd.join(path));
    let parent = if let Some(path) = parent["manualLoginProfileDir"].as_str().or(environment
        .get("ORACLE_BROWSER_PROFILE_DIR")
        .map(String::as_str))
    {
        cwd.join(path)
    } else {
        PathBuf::from(
            environment
                .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .context("ChatGPT profile home is unavailable")?,
        )
        .join(".oracle/browser-profile")
    };
    for parent in std::iter::once(parent).chain(copied_source) {
        let parent = candidate(&parent)?;
        ensure!(
            !overlap(&grok, &parent),
            "Grok and ChatGPT profiles overlap; refusing shared login state"
        );
        if grok.try_exists()? && parent.try_exists()? {
            ensure!(
                NativeEntry::open(&grok, true)?.identity
                    != NativeEntry::open(&parent, true)?.identity,
                "Grok and ChatGPT profiles have the same native directory identity"
            );
        }
    }
    Ok(())
}

fn overlap(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    let (lower_left, lower_right) = (
        PathBuf::from(left.to_string_lossy().to_lowercase()),
        PathBuf::from(right.to_string_lossy().to_lowercase()),
    );
    #[cfg(windows)]
    let (left, right) = (lower_left.as_path(), lower_right.as_path());
    left.starts_with(right) || right.starts_with(left)
}

/// Canonicalise the existing ancestor without creating or reading profile files.
fn candidate(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Browser profile path must be absolute");
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "Browser profile path cannot contain parent traversal"
    );
    let mut ancestor = path.to_owned();
    let mut missing = vec![];
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir(),
                    "Browser profile ancestor is not a directory"
                );
                let entry = NativeEntry::open(&ancestor, true)?;
                let mut canonical = crate::native_fs::canonical_wire_path(&ancestor)?;
                entry.require_matches(&canonical)?;
                for name in missing.into_iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("Browser profile ancestor is unavailable")?
                        .to_owned(),
                );
                ensure!(ancestor.pop(), "Browser profile root is unavailable");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_config_strips_chatgpt_capabilities_and_refuses_aliases() {
        let root = tempfile::tempdir().unwrap();
        let env = Environment::from([
            (
                "ORACLE_HOME_DIR".into(),
                root.path().join("home").to_string_lossy().into(),
            ),
            (
                (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
                root.path().to_string_lossy().into(),
            ),
        ]);
        let parent = json!({"manualLogin":true,"cookieSync":true,"chromePath":"PUBLIC Chrome","keepBrowser":false,"inputTimeoutMs":60000,
            "copyProfileSource":"PUBLIC source","inlineCookies":"PUBLIC invalid cookie capability","remoteChrome":{"host":"PUBLIC"},
            "browserTabRef":"PUBLIC existing tab","chatgptUrl":"https://chatgpt.com/","desiredModel":"Latest"});
        let original = parent.clone();
        let config = child_config(&parent, &env, root.path()).unwrap();
        assert_eq!(parent, original);
        assert_eq!(config["chromePath"], "PUBLIC Chrome");
        assert_eq!(config["keepBrowser"], false);
        for key in [
            "copyProfileSource",
            "inlineCookies",
            "remoteChrome",
            "browserTabRef",
            "chatgptUrl",
            "desiredModel",
        ] {
            assert!(config.get(key).is_none(), "{key}");
        }
        for key in [
            "copyProfileSource",
            "inlineCookies",
            "chromeProfile",
            "chromeCookiePath",
            "cookieNames",
            "cookieBootstrapRequired",
            "attachRunning",
            "remoteChrome",
            "remoteChromeBrowserWSEndpoint",
            "browserTabRef",
            "resumeConversationUrl",
            "chatgptUrl",
            "url",
            "unknown",
        ] {
            let mut changed = config.clone();
            changed[key] = Value::Null;
            assert!(require_config(&changed).is_err(), "{key}");
        }
        for (key, value) in [
            ("cookieSync", json!(true)),
            ("manualLogin", json!(false)),
            ("headless", json!(true)),
            ("keepBrowser", Value::Null),
            ("timeoutMs", json!("100")),
            ("maxConcurrentTabs", json!(0)),
            ("manualLoginProfileDir", json!("relative/profile")),
        ] {
            let mut changed = config.clone();
            changed[key] = value;
            assert!(require_config(&changed).is_err(), "{key}");
        }
        let same = json!({"manualLoginProfileDir":config["manualLoginProfileDir"]});
        assert!(require_distinct(&config, &same, &env, root.path()).is_err());
        let nested = json!({"manualLoginProfileDir":Path::new(config["manualLoginProfileDir"].as_str().unwrap()).parent().unwrap()});
        assert!(require_distinct(&config, &nested, &env, root.path()).is_err());
        let profile = Path::new(config["manualLoginProfileDir"].as_str().unwrap());
        for source in [
            profile.to_owned(),
            profile.parent().unwrap().to_owned(),
            profile.join("PUBLIC nested"),
        ] {
            let copied = json!({"copyProfileSource":source});
            assert!(require_distinct(&config, &copied, &env, root.path()).is_err());
        }
        assert!(candidate(&root.path().join("home/../home/grok-browser-profile")).is_err());
    }
}
