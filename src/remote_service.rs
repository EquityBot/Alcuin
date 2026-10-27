//! Local service configuration keeps browser authority on the host.
use crate::{
    config::Environment,
    remote_driver::BrowserDriver,
    remote_host::{HostOptions, RemoteHost},
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn access_token() -> String {
    let mut entropy = Vec::with_capacity(32);
    for _ in 0..2 {
        entropy.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    crate::artifacts::sha256(&entropy)[..32].into()
}
pub(crate) fn endpoint(value: &Value) -> Result<Value> {
    let converted;
    let value = if value.is_object() {
        let host = value["host"]
            .as_str()
            .context("Configured Remote Chrome host is missing")?;
        let port = value["port"]
            .as_u64()
            .filter(|port| *port > 0 && *port <= 65535)
            .context("Configured Remote Chrome port must be between 1 and 65535")?;
        let host = crate::policy::trim(host);
        ensure!(!host.is_empty(), "Configured Remote Chrome host is missing");
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.into()
        };
        converted = json!(format!("{host}:{port}"));
        &converted
    } else {
        value
    };
    let Some(raw) = value
        .as_str()
        .map(crate::policy::trim)
        .filter(|value| !value.is_empty())
    else {
        ensure!(value.is_null(), "Remote Chrome target must use host:port");
        return Ok(Value::Null);
    };
    let (hostname, segment) = raw
        .rsplit_once(':')
        .context("Remote Chrome requires host:port")?;
    let segment = segment.trim();
    let digits = segment
        .strip_prefix('+')
        .unwrap_or(segment)
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect::<String>();
    let port = digits
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .context("Remote Chrome requires a port between 1 and 65535")?;
    ensure!(
        !hostname.starts_with('[') || segment.bytes().all(|byte| byte.is_ascii_digit()),
        "Bracketed Remote Chrome targets require an integer port"
    );
    let url = reqwest::Url::parse(&format!("http://{}:{port}", hostname.trim()))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Remote Chrome target must use host:port"
    );
    let host = url
        .host_str()
        .context("Remote Chrome host is missing")?
        .trim_matches(['[', ']']);
    Ok(json!({"host":host,"port":port}))
}
pub fn browser_config(
    config: &Value,
    flags: &Value,
    env: &Environment,
    cwd: &Path,
) -> Result<Value> {
    let config = &config["browser"];
    let attach = flags
        .get("attachRunning")
        .or(config.get("attachRunning"))
        .cloned()
        .unwrap_or(json!(false));
    ensure!(
        attach.is_boolean(),
        "Host browser attachRunning must be a boolean"
    );
    let remote = endpoint(
        flags
            .get("remoteChrome")
            .or(config.get("remoteChrome"))
            .unwrap_or(&Value::Null),
    )?;
    let wait = if let Some(raw) = flags["approvalWait"]
        .as_str()
        .or_else(|| env.get("ORACLE_BROWSER_APPROVAL_WAIT").map(String::as_str))
        .map(crate::policy::trim)
        .filter(|value| !value.is_empty())
    {
        crate::followup::attachment_timeout(raw)
            .context("Invalid browser approval wait duration")?
    } else {
        match &config["approvalWaitMs"] {
            Value::Null => 20000.0,
            Value::Number(value) => value
                .as_f64()
                .context("Invalid configured browser approval wait")?,
            Value::String(value) if crate::policy::trim(value).is_empty() => 20000.0,
            Value::String(value) => crate::followup::attachment_timeout(value)
                .context("Invalid configured browser approval wait")?,
            _ => anyhow::bail!("Invalid configured browser approval wait"),
        }
    };
    ensure!(
        wait.is_finite() && wait.fract() == 0.0 && wait > 0.0 && wait <= i32::MAX as f64,
        "Browser approval wait must be a positive integer no greater than 2147483647ms"
    );
    let attached = attach == true || remote.is_object();
    let user_home = env
        .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("Service has no user home")?;
    let profile = cwd.join(crate::config::manual_login_profile_dir(
        [flags["manualLoginProfileDir"].as_str()],
        env,
        user_home,
    )?);
    let manual =
        !attached && (flags["manualLogin"] == true || flags["cookieSync"] != true || cfg!(windows));
    let mut browser = json!({"attachRunning":attach,"remoteChrome":remote,"approvalWaitMs":wait as u64,
        "manualLogin":manual,"manualLoginProfileDir":profile,"keepBrowser":true,"cookieSync":!attached && !manual && flags["cookieSync"]==true});
    if browser["cookieSync"] == true {
        // Ordinary service runs read the host's literal Default profile into a fresh
        // owned profile per run. Clients choose neither the source nor its batch.
        browser["manualLoginProfileDir"] = Value::Null;
        browser["chromeProfile"] = json!("Default");
        if let Some(fields) = browser.as_object_mut() {
            fields.remove("keepBrowser");
        }
    }
    Ok(browser)
}
pub(crate) async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {signal=tokio::signal::ctrl_c()=>signal.context("Service interrupt signal failed"),signal=terminate.recv()=>signal.context("Service termination signal handler closed")}
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("Service interrupt signal failed")
    }
}
/// Returns false when the service must not start because ordinary sync found no session.
pub(crate) async fn bootstrap(
    browser: &Value,
    environment: &Environment,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<bool> {
    ensure!(
        !cancel.is_cancelled(),
        "Browser bootstrap cancelled before startup"
    );
    if browser["attachRunning"] == true || browser["remoteChrome"].is_object() {
        println!("Using the host browser attachment; skipping local Chrome login/bootstrap.");
        return Ok(true);
    }
    if crate::browser_auth::ordinary_sync(browser) {
        println!(
            "Warning: Chrome cookie copying can invalidate an active ChatGPT session when tokens rotate. Prefer the default dedicated manual-login profile when possible."
        );
        println!("Loading ChatGPT cookies from this host's Chrome profile...");
        // The warm-up batch is discarded; each owned run reads its own fresh batch.
        return match Box::pin(crate::browser_auth::warm_up(environment, cwd, cancel)).await {
            Ok(count) => {
                println!(
                    "Detected {count} ChatGPT cookies on this host; runs will reuse this session."
                );
                Ok(true)
            }
            Err(error) => {
                ensure!(
                    !cancel.is_cancelled(),
                    "Browser bootstrap cancelled during cookie warm-up"
                );
                println!("Unable to load local ChatGPT cookies on this host: {error:#}");
                println!("No ChatGPT cookies detected on this host.");
                println!(
                    "Please open https://chatgpt.com/ in this host's browser and sign in; then rerun."
                );
                Ok(false)
            }
        };
    }
    ensure!(
        browser["manualLogin"] == true && browser["cookieSync"] != true,
        "Service cookie bootstrap remains gated; use the dedicated manual-login profile or a host browser attachment"
    );
    let path = browser["manualLoginProfileDir"]
        .as_str()
        .context("Service manual-login profile directory is missing")?;
    // Reference order: the guidance precedes the manual-login launch.
    println!("No ChatGPT cookies detected on this host.");
    println!(
        "Cookie extraction is unavailable on this platform. Using manual-login Chrome profile at {}. Remote runs will reuse this profile; sign in once when the browser opens.",
        cwd.join(path).components().collect::<PathBuf>().display()
    );
    let executable = crate::browser_detect::chrome(environment).context(
        "No Chrome installation detected; set trusted CHROME_PATH for service bootstrap",
    )?;
    let profile = crate::browser_profile::ProfileRegistry::open(&cwd.join(path))?;
    let connection = crate::browser_chrome::ChromeConnection::launch_manual_login(
        profile,
        crate::browser_chrome::ChromeLaunch {
            executable,
            cwd: cwd.into(),
            environment: environment.clone(),
            debug_port: None,
            timeout: Duration::from_millis(browser["approvalWaitMs"].as_u64().unwrap_or(20_000)),
            keep_browser: true,
            headless: false,
            hide_window: false,
            keep_window_behind: false,
        },
        cancel,
    )
    .await?;
    connection.leave_open(Duration::from_secs(1)).await?;
    Ok(true)
}
pub async fn serve(
    store: SessionStore,
    options: HostOptions,
    browser_config: Value,
    environment: Environment,
    cwd: PathBuf,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Service cancelled before startup");
    if !bootstrap(&browser_config, &environment, &cwd, cancel).await? {
        return Ok(());
    }
    let token = options.token.clone();
    let sessions = store.clone();
    let driver = BrowserDriver {
        environment,
        cwd,
        browser_config,
    };
    if let Some(requested) = options.max_concurrent_runs
        && options.max_browser_tabs < requested
    {
        println!(
            "[serve] Admitting {} concurrent run(s): the host tab cap is lower than the requested {requested}.",
            options.max_browser_tabs
        );
    }
    let host = RemoteHost::start(store, options, driver.driver()).await?;
    println!("Listening at {}", host.address);
    println!("Access token: {token}");
    println!("Leave this terminal running; press Ctrl+C to stop alcuin serve.");
    let stopped = tokio::select! {_=cancel.cancelled()=>Ok(()),signal=shutdown_signal()=>signal};
    let shutdown = host.shutdown().await;
    // Unacknowledged runs keep their answer tab, so their fresh Chrome stays retained.
    Box::pin(crate::browser_runner::settle_deferred_fresh(
        &sessions, None,
    ))
    .await;
    match (stopped, shutdown) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Service shutdown also failed: {cleanup:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn borrowed_bootstrap_needs_no_local_browser_or_profile_and_cancellation_precedes_it() {
        let home = tempfile::tempdir().unwrap();
        for browser in [
            json!({"attachRunning":true}),
            json!({"remoteChrome":{"host":"127.0.0.1","port":1}}),
        ] {
            bootstrap(
                &browser,
                &Environment::new(),
                home.path(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(
                bootstrap(&browser, &Environment::new(), home.path(), &cancelled)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled")
            );
        }
    }
    #[tokio::test]
    async fn cookie_warm_up_without_a_host_session_declines_startup_before_profile_state() {
        let home = tempfile::tempdir().unwrap();
        // No home or Chrome root is supplied, so no real profile can be selected.
        let browser = json!({"manualLogin":false,"cookieSync":true,"manualLoginProfileDir":home.path().join("profile")});
        assert!(
            !bootstrap(
                &browser,
                &Environment::new(),
                home.path(),
                &CancellationToken::new()
            )
            .await
            .unwrap()
        );
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            bootstrap(&browser, &Environment::new(), home.path(), &cancelled)
                .await
                .is_err()
        );
    }
    #[test]
    fn ordinary_service_sync_uses_only_the_host_default_profile_and_a_fresh_destination() {
        let env = Environment::from_iter([
            ("HOME".into(), "PUBLIC-home".into()),
            ("USERPROFILE".into(), "PUBLIC-home".into()),
        ]);
        let config = json!({"browser":{"chromeProfile":"PUBLIC-other","chromeCookiePath":"PUBLIC-path","keepBrowser":true}});
        let browser = browser_config(
            &config,
            &json!({"cookieSync":true}),
            &env,
            Path::new("PUBLIC-cwd"),
        )
        .unwrap();
        if cfg!(windows) {
            assert_eq!(browser["manualLogin"], true);
            assert_eq!(browser["cookieSync"], false);
            assert_eq!(browser["keepBrowser"], true);
        } else {
            assert_eq!(browser["manualLogin"], false);
            assert!(crate::browser_auth::ordinary_sync(&browser));
            assert_eq!(browser["chromeProfile"], "Default");
            assert!(browser["manualLoginProfileDir"].is_null());
            assert!(
                browser.get("keepBrowser").is_none() && browser.get("chromeCookiePath").is_none()
            );
        }
        let manual = browser_config(
            &config,
            &json!({"cookieSync":true,"manualLogin":true}),
            &env,
            Path::new("PUBLIC-cwd"),
        )
        .unwrap();
        assert!(!crate::browser_auth::ordinary_sync(&manual) && manual["keepBrowser"] == true);
    }
    #[test]
    fn host_configuration_filters_client_settings_and_preserves_route_precedence() {
        let env = Environment::from_iter([
            ("HOME".into(), "PUBLIC-home".into()),
            ("USERPROFILE".into(), "PUBLIC-home".into()),
            ("ORACLE_BROWSER_APPROVAL_WAIT".into(), "35s".into()),
        ]);
        let config = json!({"browser":{"attachRunning":true,"remoteChrome":{"host":"localhost","port":9222},"approvalWaitMs":40000,"desiredModel":"PUBLIC ignored model","remoteToken":"PUBLIC ignored token","chromePath":"PUBLIC ignored executable","resumeConversationUrl":"PUBLIC ignored conversation"}});
        let browser = browser_config(&config, &json!({}), &env, Path::new("PUBLIC-cwd")).unwrap();
        assert_eq!(
            browser["remoteChrome"],
            json!({"host":"localhost","port":9222})
        );
        assert_eq!(browser["approvalWaitMs"], 35000);
        assert_eq!(browser["cookieSync"], false);
        assert_eq!(browser["manualLogin"], false);
        for key in [
            "desiredModel",
            "remoteToken",
            "chromePath",
            "resumeConversationUrl",
        ] {
            assert!(browser.get(key).is_none());
        }
        let browser=browser_config(&config,&json!({"attachRunning":false,"remoteChrome":"[::1]:1234","approvalWait":"5s","cookieSync":true}),&env,Path::new("PUBLIC-cwd")).unwrap();
        assert_eq!(browser["remoteChrome"], json!({"host":"::1","port":1234}));
        assert_eq!(browser["approvalWaitMs"], 5000);
        assert_eq!(browser["cookieSync"], false);
        for endpoint in [
            "localhost",
            "http://localhost:9222",
            "user:token@localhost:9222",
            "localhost:65536",
        ] {
            assert!(
                browser_config(
                    &json!({}),
                    &json!({"remoteChrome":endpoint}),
                    &env,
                    Path::new("PUBLIC-cwd")
                )
                .is_err(),
                "{endpoint}"
            );
        }
        for (target, port) in [("localhost:80", 80), ("localhost:9222/private", 9222)] {
            assert_eq!(
                browser_config(
                    &json!({}),
                    &json!({"remoteChrome":target}),
                    &env,
                    Path::new("PUBLIC-cwd")
                )
                .unwrap()["remoteChrome"]["port"],
                port
            );
        }
        for wait in ["0", "-1", "0.5ms", "2147483648"] {
            assert!(
                browser_config(
                    &json!({}),
                    &json!({"approvalWait":wait}),
                    &env,
                    Path::new("PUBLIC-cwd")
                )
                .is_err()
            );
        }
        let token = access_token();
        assert_eq!(token.len(), 32);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(token, access_token());
    }
}
