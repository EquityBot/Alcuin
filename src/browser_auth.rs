//! Explicit cookie bootstrap stays in memory and never becomes saved browser intent.
use crate::{browser, cdp::Cdp, config::Environment};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt, fs::File, io::Read, path::Path, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_COOKIE_INPUT: usize = 4 * 1024 * 1024;
pub(crate) const MAX_START_GATE: usize = MAX_COOKIE_INPUT + 1024 * 1024;
const MAX_COOKIES: usize = 4096;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Cookie {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    url: Option<String>,
    domain: Option<String>,
    path: Option<String>,
    expires: Option<f64>,
    secure: Option<bool>,
    http_only: Option<bool>,
    same_site: Option<String>,
}

impl Drop for Cookie {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.value);
    }
}

#[derive(Clone)]
pub(crate) struct CookieBootstrap {
    source: String,
    cookies: Vec<Cookie>,
}
impl fmt::Debug for CookieBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CookieBootstrap([redacted])")
    }
}
impl CookieBootstrap {
    fn parse(raw: &str, source: &str) -> Result<Option<Self>> {
        if raw.len() > MAX_COOKIE_INPUT {
            return Ok(None);
        }
        let decoded = STANDARD
            .decode(raw.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let raw = decoded
            .as_deref()
            .filter(|text| text.trim_start().starts_with('['))
            .unwrap_or(raw);
        let Ok(Value::Array(cookies)) = serde_json::from_str::<Value>(raw) else {
            return Ok(None);
        };
        ensure!(
            cookies.len() <= MAX_COOKIES,
            "Selected browser cookie array exceeds its limit"
        );
        let cookies = serde_json::from_value(Value::Array(cookies)).map_err(|_| {
            anyhow::anyhow!("Selected browser cookie array contains unsupported fields")
        })?;
        Ok(Some(Self {
            source: source.into(),
            cookies,
        }))
    }
    fn candidate(raw: &str, source: &str, allow_file: bool, cwd: &Path) -> Result<Option<Self>> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > MAX_COOKIE_INPUT {
            return Ok(None);
        }
        if allow_file {
            let path = cwd.join(raw);
            if let Ok(file) = File::open(path)
                && let Some(batch) = Self::from_file(file, source)?
            {
                return Ok(Some(batch));
            }
        }
        Self::parse(raw, source)
    }
    fn from_file(file: File, source: &str) -> Result<Option<Self>> {
        if !file
            .metadata()
            .ok()
            .is_some_and(|metadata| metadata.is_file() && metadata.len() <= MAX_COOKIE_INPUT as u64)
        {
            return Ok(None);
        }
        let mut text = String::new();
        if file
            .take(MAX_COOKIE_INPUT as u64 + 1)
            .read_to_string(&mut text)
            .is_err()
        {
            return Ok(None);
        }
        Self::parse(&text, source)
    }
    pub(crate) fn resolve_explicit(
        input: &Value,
        config: &Value,
        env: &Environment,
        cwd: &Path,
    ) -> Result<Option<Self>> {
        let sources = [
            (
                input["browserInlineCookiesFile"].as_str(),
                "inline-file",
                true,
            ),
            (input["browserInlineCookies"].as_str(), "inline-arg", true),
            (
                env.get("ORACLE_BROWSER_COOKIES_FILE").map(String::as_str),
                "env-file",
                true,
            ),
            (
                env.get("ORACLE_BROWSER_COOKIES_JSON").map(String::as_str),
                "env-payload",
                false,
            ),
        ];
        let mut requested = false;
        for (raw, source, allow_file) in sources {
            if let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) {
                requested = true;
                if let Some(batch) = Self::candidate(raw, source, allow_file, cwd)? {
                    return Ok(Some(batch));
                }
            }
        }
        let requirement = &config["cookieBootstrapRequired"];
        let saved_bootstrap = requirement["kind"] == "explicit-cookies";
        let chrome_sync = input["browserNoCookieSync"] != true
            && (input["browserCookieSync"]
                .as_bool()
                .unwrap_or(!saved_bootstrap && config["cookieSync"] == true)
                || input["browserManualLoginCookieSync"]
                    .as_bool()
                    .unwrap_or(!saved_bootstrap && config["manualLoginCookieSync"] == true));
        if !chrome_sync && let Ok(home) = crate::config::oracle_home(env) {
            for name in ["cookies.json", "cookies.base64"] {
                if !requirement.is_null()
                    && !(requirement["version"] == 1
                        && saved_bootstrap
                        && requirement["source"] == format!("home:{name}"))
                {
                    continue;
                }
                // Automatic fallback accepts regular native files, never a link or special file.
                if let Ok(file) =
                    crate::native_fs::NativeEntry::open(&cwd.join(&home).join(name), false)
                    && let Some(batch) = Self::from_file(file.handle, &format!("home:{name}"))?
                {
                    return Ok(Some(batch));
                }
            }
        }
        if let Some(raw) = config.get("inlineCookies").filter(|value| !value.is_null()) {
            requested = true;
            let payload = if let Some(raw) = raw.as_str() {
                raw.to_owned()
            } else {
                serde_json::to_string(raw)?
            };
            if let Some(batch) = Self::parse(&payload, "configured-inline")? {
                return Ok(Some(batch));
            }
        }
        ensure!(
            !requested,
            "Explicit browser cookie input is invalid or exceeds the supported limits"
        );
        Ok(None)
    }
    pub(crate) fn requirement(&self) -> Value {
        json!({"version":1,"kind":"explicit-cookies","source":self.source})
    }
    pub(crate) fn validate(&self, destination: &str) -> Result<()> {
        ensure!(
            !self.parameters(destination)?.is_empty(),
            "Explicit browser cookie bootstrap contains no usable cookies"
        );
        Ok(())
    }
    fn parameters(&self, destination: &str) -> Result<Vec<Value>> {
        let destination = browser::provider_url(destination)
            .context("Cookie bootstrap requires a trusted provider destination")?;
        let fallback = destination
            .host_str()
            .context("Cookie destination host is missing")?;
        let mut seen = BTreeSet::new();
        let mut parameters = vec![];
        for cookie in &self.cookies {
            if cookie.name.is_empty() {
                continue;
            }
            ensure!(
                cookie.name.len() <= 4096
                    && !cookie.name.chars().any(char::is_control)
                    && !cookie
                        .value
                        .chars()
                        .any(|character| matches!(character, '\0' | '\r' | '\n')),
                "Explicit browser cookie contains an invalid name or value"
            );
            let domain = cookie.domain.as_deref().unwrap_or(fallback);
            ensure!(
                allowed_cookie_host(domain.trim_start_matches('.')) || domain == ".openai.com",
                "Explicit browser cookie domain is unsupported"
            );
            if !seen.insert((domain.to_owned(), cookie.name.clone())) {
                continue;
            }
            let mut value = json!({"name":cookie.name,"value":cookie.value,"path":cookie.path.as_deref().unwrap_or("/"),
                "secure":cookie.secure.unwrap_or(true),"httpOnly":cookie.http_only.unwrap_or(false)});
            if let Some(url) = cookie.url.as_deref().filter(|url| !url.is_empty()) {
                let url = reqwest::Url::parse(url)
                    .ok()
                    .filter(|url| {
                        url.scheme() == "https"
                            && url.username().is_empty()
                            && url.password().is_none()
                            && url.port_or_known_default() == Some(443)
                            && url.host_str().is_some_and(allowed_cookie_host)
                    })
                    .context("Explicit browser cookie URL is unsupported")?;
                value["url"] = json!(url.to_string());
            } else if domain.starts_with('.') {
                value["domain"] = json!(domain);
            } else {
                value["url"] = json!(format!("https://{domain}"));
            }
            if let Some(expires) = cookie
                .expires
                .filter(|value| value.is_finite() && *value > 0.0)
            {
                let seconds = if expires >= 1e15 {
                    expires / 1e6 - 11_644_473_600.0
                } else if expires >= 1e12 {
                    expires / 1000.0
                } else {
                    expires
                };
                value["expires"] = json!((seconds + 0.5).floor());
            }
            if let Some(same_site) = &cookie.same_site {
                ensure!(
                    matches!(same_site.as_str(), "Lax" | "Strict" | "None"),
                    "Explicit browser cookie SameSite is unsupported"
                );
                value["sameSite"] = json!(same_site);
            }
            parameters.push(value);
        }
        Ok(parameters)
    }
    pub(crate) async fn apply(
        &self,
        page: &Cdp,
        destination: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<usize> {
        let parameters = self.parameters(destination)?;
        let deadline = Instant::now() + timeout;
        let mut applied = 0;
        for parameters in parameters {
            ensure!(!cancel.is_cancelled(), "Cookie bootstrap was cancelled");
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "Cookie bootstrap deadline expired");
            let result = page
                .call("Network.setCookie", parameters, remaining, cancel)
                .await;
            ensure!(!cancel.is_cancelled(), "Cookie bootstrap was cancelled");
            if result.as_ref().is_ok_and(|value| value["success"] == true) {
                applied += 1;
            }
        }
        ensure!(
            applied > 0,
            "{}",
            if self.is_chrome_profile() {
                NO_PROFILE_COOKIES
            } else {
                "No inline cookies were applied; aborting before navigation."
            }
        );
        Ok(applied)
    }
    /// True for a batch read from the local Chrome profile rather than explicit cookies.
    pub(crate) fn is_chrome_profile(&self) -> bool {
        self.source == "chrome-profile"
    }
    /// Reference progress line after `applied` cookies reached the browser.
    pub(crate) fn applied_line(&self, applied: usize, config: &Value) -> String {
        if self.is_chrome_profile() {
            format!(
                "Copied {applied} cookies from Chrome profile {}",
                config["chromeProfile"].as_str().unwrap_or("Default")
            )
        } else {
            format!("Applied {applied} inline cookies")
        }
    }
}
pub(crate) fn prepare_config(
    input: &Value,
    config: &mut Value,
    env: &Environment,
    cwd: &Path,
) -> Result<Option<CookieBootstrap>> {
    if crate::browser_profile_copy::configured_source(config)?.is_some() {
        ensure!(
            config["manualLogin"] != true
                && config["keepBrowser"] != true
                && config["attachRunning"] != true
                && !config["remoteChrome"].is_object()
                && crate::remote_session::Connection::resolve(config, env).is_none(),
            "Copied profiles require a disposable local browser without manual login or a borrowed attachment"
        );
        ensure!(
            ["browserInlineCookies", "browserInlineCookiesFile"]
                .iter()
                .all(|key| input[*key].is_null())
                && ["ORACLE_BROWSER_COOKIES_FILE", "ORACLE_BROWSER_COOKIES_JSON"]
                    .iter()
                    .all(|key| !env.contains_key(*key))
                && config["inlineCookies"].is_null()
                && config["cookieBootstrapRequired"].is_null(),
            "Copy-profile cannot be combined with explicit cookie input"
        );
        // The selected copied profile supplies its session. Do not select another
        // account from automatic home-cookie fallback or ordinary Chrome sync.
        config["cookieSync"] = json!(false);
        config["manualLoginCookieSync"] = json!(false);
        return Ok(None);
    }
    let selected = CookieBootstrap::resolve_explicit(input, config, env, cwd)?;
    let batch = match selected {
        Some(batch) if batch.cookies.is_empty() => {
            // An explicitly selected empty array is still an account-source choice.
            // It must not turn into ordinary Chrome sync or another home fallback.
            config["cookieSync"] = json!(false);
            config["manualLoginCookieSync"] = json!(false);
            None
        }
        batch => batch,
    };
    if let Some(fields) = config.as_object_mut() {
        fields.remove("inlineCookies");
        fields.remove("inlineCookiesSource");
    }
    if let Some(batch) = &batch {
        ensure!(
            config["attachRunning"] != true
                && !config["remoteChrome"].is_object()
                && crate::remote_session::Connection::resolve(config, env).is_none(),
            "Explicit cookie bootstrap requires a newly owned local Chrome process"
        );
        batch.validate(
            config["chatgptUrl"]
                .as_str()
                .unwrap_or("https://chatgpt.com/"),
        )?;
        config["cookieBootstrapRequired"] = batch.requirement();
        config["cookieSync"] = json!(true);
        config["manualLoginCookieSync"] = json!(true);
    }
    Ok(batch)
}
/// True when this run reads ordinary Chrome cookies into its fresh private profile.
pub(crate) fn ordinary_sync(config: &Value) -> bool {
    config["cookieSync"] == true
        && config["manualLogin"] != true
        && config["cookieBootstrapRequired"].is_null()
        && config["copyProfileSource"].is_null()
        && config["attachRunning"] != true
        && !config["remoteChrome"].is_object()
}
async fn read_chrome(
    config: &Value,
    env: &Environment,
    cwd: &Path,
    attempt: Duration,
    cancel: &CancellationToken,
) -> Result<CookieBootstrap> {
    let deadline = std::time::Instant::now() + attempt;
    let rows =
        crate::browser_cookie_loader::read_native(config, env, cwd, deadline, cancel).await?;
    let cookies = serde_json::from_value(Value::Array(rows))
        .map_err(|_| anyhow::anyhow!("Chrome cookie extraction produced unsupported fields"))?;
    let batch = CookieBootstrap {
        source: "chrome-profile".into(),
        cookies,
    };
    let destination = config["chatgptUrl"]
        .as_str()
        .or(config["url"].as_str())
        .unwrap_or("https://chatgpt.com/");
    ensure!(
        !batch.parameters(destination)?.is_empty(),
        NO_PROFILE_COOKIES
    );
    Ok(batch)
}
/// Reference refusal when cookie sync found nothing to apply for a fresh profile.
/// Reference extractChatGptConversationId: the first `/c/<id>` of a ChatGPT tab URL.
fn tab_conversation_id(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com")) {
        return None;
    }
    let path = url.path();
    path.match_indices("/c/").find_map(|(index, _)| {
        let id: String = path[index + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        (!id.is_empty()).then_some(id)
    })
}
/// Reference clearStaleChatGptConversationCookies. Before navigation, delete the ChatGPT
/// `conv_key_*` cookies that no open conversation tab or preserved conversation uses. Only names,
/// domains and paths leave the CDP reply; values are zeroised and never kept or logged. Failures
/// never stop a run. Returns the reference log lines.
pub(crate) async fn clear_stale_conversation_cookies(
    browser: &Cdp,
    page: &Cdp,
    preserve: &[Option<String>],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Vec<String> {
    let mut lines = vec![];
    let mut preserved: BTreeSet<String> = preserve
        .iter()
        .flatten()
        .map(|id| format!("conv_key_{id}"))
        .collect();
    match browser
        .call("Target.getTargets", json!({}), timeout, cancel)
        .await
    {
        Ok(targets) => preserved.extend(
            targets["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|target| tab_conversation_id(target["url"].as_str()?))
                .map(|id| format!("conv_key_{id}")),
        ),
        Err(error) => {
            lines.push(format!("[cookies] Failed to inspect active ChatGPT conversations; skipping stale cookie cleanup: {error}"));
            return lines;
        }
    }
    let stale: Vec<(String, String, String)> = match page
        .call("Network.getAllCookies", json!({}), timeout, cancel)
        .await
    {
        Ok(mut reply) => {
            let stale = reply["cookies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|cookie| {
                    let name = cookie["name"].as_str()?;
                    let domain = cookie["domain"].as_str().unwrap_or("");
                    let host = domain.strip_prefix('.').unwrap_or(domain).to_lowercase();
                    (name.starts_with("conv_key_")
                        && matches!(host.as_str(), "chatgpt.com" | "chat.openai.com")
                        && !preserved.contains(name))
                    .then(|| {
                        let path = cookie["path"].as_str().unwrap_or("/");
                        (name.to_owned(), domain.to_owned(), path.to_owned())
                    })
                })
                .collect();
            for cookie in reply["cookies"].as_array_mut().into_iter().flatten() {
                if let Some(Value::String(value)) = cookie.get_mut("value") {
                    zeroize::Zeroize::zeroize(value);
                }
            }
            stale
        }
        Err(error) => {
            lines.push(format!(
                "[cookies] Failed to inspect ChatGPT conversation cookies: {error}"
            ));
            return lines;
        }
    };
    let mut deleted = 0;
    for (name, domain, path) in stale {
        let parameters = json!({"name":name,"domain":domain,"path":path});
        match page
            .call("Network.deleteCookies", parameters, timeout, cancel)
            .await
        {
            Ok(_) => deleted += 1,
            Err(error) => lines.push(format!(
                "[cookies] Failed to clear a stale ChatGPT conversation cookie: {error}"
            )),
        }
    }
    if deleted > 0 {
        lines.push(format!(
            "[cookies] Cleared {deleted} stale ChatGPT conversation cookie{}.",
            if deleted == 1 { "" } else { "s" }
        ));
    }
    lines
}
pub(crate) const NO_PROFILE_COOKIES: &str = "No ChatGPT cookies were applied from your Chrome profile; cannot proceed in browser mode. Make sure ChatGPT is signed in in the selected profile, use --browser-manual-login / inline cookies, or retry with --browser-cookie-wait 5s if Keychain prompts are slow.";
/// Reference CHROME_COOKIE_SYNC_WARNING, logged before an ordinary Chrome cookie read.
pub(crate) const CHROME_COOKIE_SYNC_WARNING: &str = "Warning: copying cookies from a live Chrome profile can invalidate that browser's ChatGPT session when tokens rotate. Prefer --browser-manual-login or inline cookies when possible.";
/// Each read attempt follows the reference's integer-prefix `ORACLE_COOKIE_LOAD_TIMEOUT_MS`.
fn load_timeout(env: &Environment) -> Duration {
    let milliseconds = env
        .get("ORACLE_COOKIE_LOAD_TIMEOUT_MS")
        .map(|raw| raw.trim())
        .and_then(|raw| {
            let digits = raw.len() - raw.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            raw[..digits].parse::<u64>().ok()
        })
        .filter(|ms| *ms > 0)
        .unwrap_or(5_000);
    Duration::from_millis(milliseconds)
}
/// `read_ordinary_logged` without progress lines.
#[cfg(test)]
pub(crate) async fn read_ordinary(
    config: &Value,
    env: &Environment,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<CookieBootstrap> {
    read_ordinary_logged(config, env, cwd, cancel, &|_| {}).await
}
/// One fresh per-run read, logging the reference retry line. The batch stays in this worker
/// and is never saved or forwarded.
pub(crate) async fn read_ordinary_logged(
    config: &Value,
    env: &Environment,
    cwd: &Path,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Result<CookieBootstrap> {
    ensure!(
        ordinary_sync(config),
        "Ordinary Chrome cookie extraction requires a fresh owned local profile"
    );
    let attempt = load_timeout(env);
    let wait = config["cookieSyncWaitMs"].as_u64().unwrap_or(0);
    match read_chrome(config, env, cwd, attempt, cancel).await {
        Err(error) if wait > 0 && !cancel.is_cancelled() => {
            let label = if wait >= 1000 {
                format!("{}s", (wait as f64 / 1000.0).round())
            } else {
                format!("{wait}ms")
            };
            log(&if error.to_string() == NO_PROFILE_COOKIES {
                format!("[cookies] No cookies found; waiting {label} then retrying once.")
            } else {
                format!(
                    "[cookies] Cookie read failed ({error}); waiting {label} then retrying once."
                )
            });
            tokio::select! {
                biased;
                _ = cancel.cancelled() => anyhow::bail!("Chrome cookie extraction cancelled"),
                _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            }
            read_chrome(config, env, cwd, attempt, cancel).await
        }
        result => result,
    }
}
/// Service warm-up proves the host Default profile is usable, then discards that batch.
pub(crate) async fn warm_up(
    env: &Environment,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<usize> {
    let batch = read_chrome(
        &json!({"cookieSync":true,"chromeProfile":"Default"}),
        env,
        cwd,
        Duration::from_secs(5),
        cancel,
    )
    .await?;
    Ok(batch.cookies.len())
}
fn allowed_cookie_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "chatgpt.com" | "chat.openai.com" | "atlas.openai.com"
    )
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CookieGate {
    version: u8,
    session_id: String,
    process_identity: crate::process::ProcessIdentity,
    source: String,
    cookies: Vec<Cookie>,
}
pub(crate) fn encode_start_gate(
    batch: Option<&CookieBootstrap>,
    session: &str,
    identity: &crate::process::ProcessIdentity,
) -> Result<Vec<u8>> {
    let Some(batch) = batch else {
        return Ok(b"ready\n".to_vec());
    };
    let mut bytes = b"cookies-v1\n".to_vec();
    serde_json::to_writer(
        &mut bytes,
        &CookieGate {
            version: 1,
            session_id: session.into(),
            process_identity: identity.clone(),
            source: batch.source.clone(),
            cookies: batch.cookies.clone(),
        },
    )?;
    ensure!(
        bytes.len() <= MAX_START_GATE,
        "Cookie bootstrap start payload exceeds its limit"
    );
    Ok(bytes)
}
pub(crate) fn decode_start_gate(
    bytes: &[u8],
    session: &str,
    identity: &crate::process::ProcessIdentity,
) -> Result<Option<CookieBootstrap>> {
    if bytes == b"ready\n" {
        return Ok(None);
    }
    ensure!(
        bytes.len() <= MAX_START_GATE,
        "Detached worker start payload exceeds its limit"
    );
    let bytes = bytes
        .strip_prefix(b"cookies-v1\n")
        .context("Detached worker start was not acknowledged; refusing dispatch")?;
    let gate: CookieGate = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Detached cookie bootstrap payload is malformed"))?;
    ensure!(
        gate.version == 1
            && gate.session_id == session
            && gate.process_identity == *identity
            && !gate.cookies.is_empty()
            && gate.cookies.len() <= MAX_COOKIES
            && matches!(
                gate.source.as_str(),
                "inline-file"
                    | "inline-arg"
                    | "env-file"
                    | "env-payload"
                    | "configured-inline"
                    | "home:cookies.json"
                    | "home:cookies.base64"
            ),
        "Detached cookie bootstrap does not match its exact worker and session"
    );
    Ok(Some(CookieBootstrap {
        source: gate.source,
        cookies: gate.cookies,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_copy_selection_never_falls_back_to_another_cookie_account() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("cookies.json"),
            r#"[{"name":"PUBLIC-other-account","value":"PUBLIC-other-value"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        for source in [
            json!(0),
            json!(true),
            json!({}),
            json!([]),
            json!(""),
            json!(" "),
        ] {
            let mut config = json!({"copyProfileSource":source,"manualLogin":false});
            assert!(
                prepare_config(&json!({}), &mut config, &env, home.path()).is_err(),
                "source={source}"
            );
        }
        for profile in [
            json!(0),
            json!(true),
            json!({}),
            json!([]),
            json!("../Default"),
            json!("Profile\\Other"),
        ] {
            let mut config = json!({"copyProfileSource":"PUBLIC-source","manualLogin":false,"chromeProfile":profile});
            assert!(
                prepare_config(&json!({}), &mut config, &env, home.path()).is_err(),
                "profile={profile}"
            );
        }
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn ordinary_sync_reads_one_runtime_batch_that_cannot_cross_the_worker_gate() {
        let home = tempfile::tempdir().unwrap();
        let (source, env) = crate::browser_cookie_loader::synthetic_source(home.path());
        let original = std::fs::read(&source).unwrap();
        let config = json!({"cookieSync":true,"manualLogin":false,"chromeCookiePath":source,"cookieNames":["PUBLIC-session"]});
        let batch = read_ordinary(&config, &env, home.path(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(batch.source, "chrome-profile");
        assert_eq!(batch.cookies.len(), 1);
        assert_eq!(batch.cookies[0].value, "PUBLIC-cookie-value");
        assert_eq!(format!("{batch:?}"), "CookieBootstrap([redacted])");
        let parameters = batch.parameters("https://chatgpt.com/").unwrap();
        assert_eq!(parameters[0]["domain"], ".chatgpt.com");
        assert!(parameters[0].get("url").is_none());
        // Extracted batches stay in the worker that read them.
        let identity = crate::process::ProcessIdentity::current().unwrap();
        let gate = encode_start_gate(Some(&batch), "PUBLIC-session-id", &identity).unwrap();
        assert!(decode_start_gate(&gate, "PUBLIC-session-id", &identity).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(home.path().join("PUBLIC-snapshots"))
                .unwrap()
                .count(),
            0
        );
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn empty_profile_reads_use_the_reference_refusal_and_retry_line() {
        let home = tempfile::tempdir().unwrap();
        let fixture = home.path().join("PUBLIC-fixture");
        std::fs::create_dir(&fixture).unwrap();
        let (source, env) = crate::browser_cookie_loader::synthetic_source(&fixture);
        let config = json!({"cookieSync":true,"chromeCookiePath":source,"cookieNames":["PUBLIC-absent"],"cookieSyncWaitMs":5});
        let lines = std::sync::Mutex::new(Vec::new());
        let error = read_ordinary_logged(
            &config,
            &env,
            home.path(),
            &CancellationToken::new(),
            &|line| lines.lock().unwrap().push(line.to_owned()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), NO_PROFILE_COOKIES);
        assert_eq!(
            *lines.lock().unwrap(),
            ["[cookies] No cookies found; waiting 5ms then retrying once."]
        );
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn cookie_wait_retries_once_and_cancellation_interrupts_the_wait() {
        let home = tempfile::tempdir().unwrap();
        let fixture = home.path().join("PUBLIC-fixture");
        std::fs::create_dir(&fixture).unwrap();
        let source = fixture.join("PUBLIC-Chrome/Default/Network/Cookies");
        let env = Environment::from([
            (
                "TEMP".into(),
                home.path()
                    .join("PUBLIC-snapshots")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "TMPDIR".into(),
                home.path()
                    .join("PUBLIC-snapshots")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("SWEET_COOKIE_LINUX_KEYRING".into(), "basic".into()),
        ]);
        let config = json!({"cookieSync":true,"chromeCookiePath":source,"cookieNames":["PUBLIC-session"],"cookieSyncWaitMs":400});
        let creator = {
            let fixture = fixture.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                tokio::task::spawn_blocking(move || {
                    crate::browser_cookie_loader::synthetic_source(&fixture)
                })
                .await
                .unwrap()
            })
        };
        let started = std::time::Instant::now();
        let batch = read_ordinary(&config, &env, home.path(), &CancellationToken::new())
            .await
            .unwrap();
        creator.await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert_eq!(batch.cookies.len(), 1);
        let missing = json!({"cookieSync":true,"chromeCookiePath":home.path().join("PUBLIC-missing/Cookies"),"cookieSyncWaitMs":60_000});
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let error = read_ordinary(&missing, &env, home.path(), &cancel)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let no_wait = json!({"cookieSync":true,"chromeCookiePath":home.path().join("PUBLIC-missing/Cookies")});
        let started = std::time::Instant::now();
        assert!(
            read_ordinary(&no_wait, &env, home.path(), &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[tokio::test]
    async fn ordinary_sync_never_reads_for_manual_borrowed_copied_explicit_or_empty_selected_sources()
     {
        let home = tempfile::tempdir().unwrap();
        // A configured home with Chrome roots that must never be opened.
        let env = Environment::from([
            ("HOME".into(), home.path().to_string_lossy().into_owned()),
            (
                "USERPROFILE".into(),
                home.path().to_string_lossy().into_owned(),
            ),
            (
                "LOCALAPPDATA".into(),
                home.path().to_string_lossy().into_owned(),
            ),
        ]);
        for config in [
            json!({"cookieSync":false}),
            json!({"cookieSync":true,"manualLogin":true}),
            json!({"cookieSync":true,"attachRunning":true}),
            json!({"cookieSync":true,"remoteChrome":{"host":"127.0.0.1","port":9}}),
            json!({"cookieSync":true,"copyProfileSource":"PUBLIC-source"}),
            json!({"cookieSync":true,"cookieBootstrapRequired":{"version":1,"kind":"explicit-cookies","source":"inline-arg"}}),
        ] {
            assert!(!ordinary_sync(&config), "{config}");
            let error = read_ordinary(&config, &env, home.path(), &CancellationToken::new())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("fresh owned local profile"), "{error}");
        }
        // An explicitly selected empty array is an account choice, not a fallback to Chrome.
        let mut config = json!({"cookieSync":true,"manualLogin":false});
        assert!(
            prepare_config(
                &json!({"browserInlineCookies":"[]"}),
                &mut config,
                &env,
                home.path()
            )
            .unwrap()
            .is_none()
        );
        assert!(!ordinary_sync(&config));
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }
    #[test]
    fn cookie_load_timeout_follows_the_reference_integer_prefix() {
        for (raw, expected) in [
            (None, 5_000),
            (Some("1500"), 1_500),
            (Some(" 1500ms "), 1_500),
            (Some("0"), 5_000),
            (Some("PUBLIC"), 5_000),
            (Some("-5"), 5_000),
        ] {
            let mut env = Environment::new();
            if let Some(raw) = raw {
                env.insert("ORACLE_COOKIE_LOAD_TIMEOUT_MS".into(), raw.into());
            }
            assert_eq!(
                load_timeout(&env),
                Duration::from_millis(expected),
                "{raw:?}"
            );
        }
    }
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    #[test]
    fn home_cookie_files_follow_explicit_sources_and_stop_at_the_first_parsed_array() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        let raw = r#"[{"name":"PUBLIC-home-json","value":"PUBLIC-home-secret"}]"#;
        std::fs::write(home.path().join("cookies.json"), raw).unwrap();
        std::fs::write(
            home.path().join("cookies.base64"),
            STANDARD.encode(br#"[{"name":"PUBLIC-home-base64"}]"#),
        )
        .unwrap();
        let resolve = |input: &Value| {
            CookieBootstrap::resolve_explicit(input, &json!({}), &env, home.path()).unwrap()
        };
        let batch = resolve(&json!({})).unwrap();
        assert_eq!(batch.source, "home:cookies.json");
        assert_eq!(
            batch.parameters("https://chatgpt.com/").unwrap()[0]["name"],
            "PUBLIC-home-json"
        );
        let explicit =
            resolve(&json!({"browserInlineCookies":r#"[{"name":"PUBLIC-argument"}]"#})).unwrap();
        assert_eq!(explicit.source, "inline-arg");
        assert!(
            resolve(&json!({"browserInlineCookies":"[]"}))
                .unwrap()
                .cookies
                .is_empty()
        );
        std::fs::write(home.path().join("cookies.json"), "[]").unwrap();
        assert!(resolve(&json!({})).unwrap().cookies.is_empty());
        std::fs::write(home.path().join("cookies.json"), "PUBLIC-invalid").unwrap();
        assert_eq!(resolve(&json!({})).unwrap().source, "home:cookies.base64");
        std::fs::write(
            home.path().join("cookies.json"),
            vec![b'x'; MAX_COOKIE_INPUT + 1],
        )
        .unwrap();
        assert_eq!(resolve(&json!({})).unwrap().source, "home:cookies.base64");
        std::fs::write(home.path().join("cookies.base64"), "PUBLIC-invalid").unwrap();
        assert!(resolve(&json!({})).is_none());
    }

    #[test]
    fn home_cookie_selection_stays_runtime_only_and_crosses_the_exact_worker_gate() {
        let home = tempfile::tempdir().unwrap();
        let oracle = home.path().join("PUBLIC-private-home");
        std::fs::create_dir(&oracle).unwrap();
        std::fs::write(
            oracle.join("cookies.json"),
            r#"[{"name":"PUBLIC-home-cookie","value":"PUBLIC-home-secret"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            oracle.to_string_lossy().into_owned(),
        )]);
        let request = crate::execution::resolve(
            &json!({"engine":"browser","prompt":"PUBLIC home cookie privacy regression"}),
            &json!({}),
            &env,
            home.path(),
        )
        .unwrap();
        let batch = request.cookie_bootstrap.as_ref().unwrap();
        let identity = crate::process::ProcessIdentity::current().unwrap();
        let encoded = encode_start_gate(Some(batch), "PUBLIC-home-session", &identity).unwrap();
        let restored = decode_start_gate(&encoded, "PUBLIC-home-session", &identity)
            .unwrap()
            .unwrap();
        assert_eq!(restored.requirement(), batch.requirement());
        assert_eq!(
            restored.parameters("https://chatgpt.com/").unwrap()[0]["value"],
            "PUBLIC-home-secret"
        );
        for saved in [&request.options, &request.browser, &request.browser_preview] {
            let text = saved.to_string();
            assert!(!text.contains("PUBLIC-home-secret"));
            assert!(!text.contains("PUBLIC-private-home"));
        }
        let mut refreshed = request;
        let saved = refreshed.browser.clone();
        refreshed
            .replace_browser_config(saved, &json!({}), &env, home.path())
            .unwrap();
        assert_eq!(
            refreshed.cookie_bootstrap.as_ref().unwrap().requirement()["source"],
            "home:cookies.json"
        );
        let api = crate::execution::resolve(
            &json!({"engine":"api","prompt":"PUBLIC API cookie isolation regression"}),
            &json!({}),
            &env,
            home.path(),
        )
        .unwrap();
        assert!(api.cookie_bootstrap.is_none());
        assert!(api.browser["cookieBootstrapRequired"].is_null());
    }

    #[test]
    fn home_cookie_fallback_uses_the_request_home_and_ignores_nonregular_files() {
        let cwd = tempfile::tempdir().unwrap();
        let home = cwd.path().join("PUBLIC-oracle");
        std::fs::create_dir_all(home.join("cookies.json")).unwrap();
        std::fs::write(
            home.join("cookies.base64"),
            r#"[{"name":"PUBLIC-fallback"}]"#,
        )
        .unwrap();
        let env = Environment::from([("ORACLE_HOME_DIR".into(), "PUBLIC-oracle".into())]);
        let resolve = || {
            CookieBootstrap::resolve_explicit(&json!({}), &json!({}), &env, cwd.path())
                .unwrap()
                .unwrap()
        };
        assert_eq!(resolve().source, "home:cookies.base64");
        #[cfg(unix)]
        {
            std::fs::remove_dir(home.join("cookies.json")).unwrap();
            let linked = cwd.path().join("PUBLIC-linked-cookie.json");
            std::fs::write(&linked, r#"[{"name":"PUBLIC-must-not-load"}]"#).unwrap();
            std::os::unix::fs::symlink(linked, home.join("cookies.json")).unwrap();
            assert_eq!(resolve().source, "home:cookies.base64");
        }
    }

    #[test]
    fn home_cookie_chrome_sync_preference_never_switches_to_saved_home_authentication() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("cookies.json"),
            r#"[{"name":"PUBLIC-home"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        for (input, config, suppressed) in [
            (json!({}), json!({"cookieSync":true}), true),
            (json!({}), json!({"manualLoginCookieSync":true}), true),
            (json!({"browserCookieSync":true}), json!({}), true),
            (
                json!({"browserManualLoginCookieSync":true}),
                json!({}),
                true,
            ),
            (
                json!({"browserNoCookieSync":true}),
                json!({"cookieSync":true}),
                false,
            ),
            (
                json!({"browserCookieSync":false}),
                json!({"cookieSync":true}),
                false,
            ),
        ] {
            let batch =
                CookieBootstrap::resolve_explicit(&input, &config, &env, home.path()).unwrap();
            assert_eq!(batch.is_none(), suppressed, "{input}, {config}");
        }
        let request = crate::execution::resolve(
            &json!({"engine":"browser","prompt":"PUBLIC Chrome authentication selection"}),
            &json!({"browser":{"manualLogin":true,"cookieSync":true}}),
            &env,
            home.path(),
        )
        .unwrap();
        assert!(request.cookie_bootstrap.is_none());
        let explicit = CookieBootstrap::resolve_explicit(
            &json!({"browserCookieSync":true,"browserInlineCookies":r#"[{"name":"PUBLIC-explicit"}]"#}),
            &json!({}),
            &env,
            home.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(explicit.source, "inline-arg");
    }

    #[test]
    fn home_cookie_selected_array_with_invalid_fields_refuses_without_account_fallback() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        std::fs::write(
            home.path().join("cookies.base64"),
            r#"[{"name":"PUBLIC-other-account"}]"#,
        )
        .unwrap();
        for raw in [
            r#"[{"name":"PUBLIC-private-sentinel","value":123}]"#,
            r#"["PUBLIC-private-sentinel"]"#,
        ] {
            std::fs::write(home.path().join("cookies.json"), raw).unwrap();
            let error =
                CookieBootstrap::resolve_explicit(&json!({}), &json!({}), &env, home.path())
                    .unwrap_err();
            assert!(!format!("{error:#}").contains("PUBLIC-private-sentinel"));
            let error = CookieBootstrap::resolve_explicit(
                &json!({"browserInlineCookies":raw}),
                &json!({"inlineCookies":[{"name":"PUBLIC-other-account"}]}),
                &Environment::new(),
                home.path(),
            )
            .unwrap_err();
            assert!(!format!("{error:#}").contains("PUBLIC-private-sentinel"));
        }
    }

    #[test]
    fn home_cookie_fallback_never_replaces_a_different_saved_cookie_requirement() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("cookies.json"),
            r#"[{"name":"PUBLIC-home-other-account"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        for source in [
            "inline-arg",
            "inline-file",
            "env-file",
            "env-payload",
            "configured-inline",
            "home:cookies.base64",
        ] {
            let mut config = json!({"cookieBootstrapRequired":{"version":1,"kind":"explicit-cookies","source":source},"cookieSync":true,"manualLoginCookieSync":true});
            let original = config.clone();
            assert!(
                prepare_config(&json!({}), &mut config, &env, home.path())
                    .unwrap()
                    .is_none(),
                "{source}"
            );
            assert_eq!(config, original);
            let batch = prepare_config(
                &json!({"browserInlineCookies":r#"[{"name":"PUBLIC-new-explicit"}]"#}),
                &mut config,
                &env,
                home.path(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(batch.requirement()["source"], "inline-arg");
        }
    }

    #[test]
    fn detached_gate_binds_exact_worker_session_version_and_bounded_complete_input() {
        let identity = crate::process::ProcessIdentity::current().unwrap();
        let batch = CookieBootstrap::parse(
            r#"[{"name":"PUBLIC-name","value":"PUBLIC-secret-sentinel"}]"#,
            "inline-arg",
        )
        .unwrap()
        .unwrap();
        let encoded = encode_start_gate(Some(&batch), "PUBLIC-session", &identity).unwrap();
        let decoded = decode_start_gate(&encoded, "PUBLIC-session", &identity)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.parameters("https://chatgpt.com/").unwrap()[0]["value"],
            "PUBLIC-secret-sentinel"
        );
        assert!(decode_start_gate(&encoded, "PUBLIC-other", &identity).is_err());
        let mut changed = identity.clone();
        changed.pid += 1;
        assert!(decode_start_gate(&encoded, "PUBLIC-session", &changed).is_err());
        for bytes in [
            encoded[..encoded.len() - 1].to_vec(),
            vec![b'x'; MAX_START_GATE + 1],
            b"ready\ntrailing".to_vec(),
        ] {
            let error = decode_start_gate(&bytes, "PUBLIC-session", &identity).unwrap_err();
            assert!(!format!("{error:#}").contains("PUBLIC-secret-sentinel"));
        }
        let mut gate: Value = serde_json::from_slice(&encoded[b"cookies-v1\n".len()..]).unwrap();
        for key in ["version", "source", "cookies", "unexpected"] {
            let mut invalid = gate.clone();
            invalid[key] = match key {
                "version" => json!(2),
                "source" => json!("PUBLIC-secret-sentinel"),
                "cookies" => json!([]),
                _ => json!(true),
            };
            let mut raw = b"cookies-v1\n".to_vec();
            raw.extend(serde_json::to_vec(&invalid).unwrap());
            assert!(
                decode_start_gate(&raw, "PUBLIC-session", &identity).is_err(),
                "{key}"
            );
        }
        gate["cookies"][0]["value"] = json!("x".repeat(MAX_START_GATE));
        let huge: CookieGate = serde_json::from_value(gate).unwrap();
        assert!(
            encode_start_gate(
                Some(&CookieBootstrap {
                    source: huge.source,
                    cookies: huge.cookies
                }),
                "PUBLIC-session",
                &identity
            )
            .is_err()
        );
        assert!(
            decode_start_gate(
                &encode_start_gate(None, "PUBLIC-session", &identity).unwrap(),
                "PUBLIC-session",
                &identity
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn selected_empty_explicit_cookies_disable_chrome_sync_before_any_account_fallback() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("cookies.json"),
            r#"[{"name":"PUBLIC-other-account"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_HOME_DIR".into(),
            home.path().to_string_lossy().into_owned(),
        )]);
        for (input, mut config) in [
            (
                json!({"browserInlineCookies":"[]"}),
                json!({"cookieSync":true,"manualLoginCookieSync":true}),
            ),
            (
                json!({}),
                json!({"inlineCookies":[],"cookieSync":true,"manualLoginCookieSync":true}),
            ),
        ] {
            assert!(
                prepare_config(&input, &mut config, &env, home.path())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(config["cookieSync"], false);
            assert_eq!(config["manualLoginCookieSync"], false);
            assert!(config["cookieBootstrapRequired"].is_null());
            assert!(config["inlineCookies"].is_null());
        }
    }

    #[tokio::test]
    async fn cookie_application_counts_acknowledged_writes_and_redacts_failures() {
        for succeeds in [true, false] {
            let (client, server) = tokio::io::duplex(65536);
            let page = Cdp::from_socket(
                WebSocketStream::from_raw_socket(client, Role::Client, None).await,
                Duration::from_secs(1),
            );
            let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let task = tokio::spawn(async move {
                for index in 0..3 {
                    let Message::Text(raw) = server.next().await.unwrap().unwrap() else {
                        panic!("Expected CDP cookie command")
                    };
                    let command: Value = serde_json::from_str(&raw).unwrap();
                    assert_eq!(command["method"], "Network.setCookie");
                    let response = if index == 0 {
                        json!({"id":command["id"],"error":{"code":-1,"message":"PUBLIC-secret-sentinel"}})
                    } else {
                        json!({"id":command["id"],"result":{"success":succeeds&&index==2}})
                    };
                    server
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
                server
            });
            let batch = CookieBootstrap::parse(
                r#"[{"name":"PUBLIC-one"},{"name":"PUBLIC-two"},{"name":"PUBLIC-three"}]"#,
                "inline-arg",
            )
            .unwrap()
            .unwrap();
            let result = batch
                .apply(
                    &page,
                    "https://chatgpt.com/",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await;
            if succeeds {
                assert_eq!(result.unwrap(), 1)
            } else {
                assert!(!format!("{:#}", result.unwrap_err()).contains("PUBLIC-secret-sentinel"))
            }
            let server = task.await.unwrap();
            page.disconnect_and_wait(Duration::from_secs(1))
                .await
                .unwrap();
            drop(server);
        }
    }

    #[tokio::test]
    async fn cancellation_sends_no_cookie_command() {
        let (client, server) = tokio::io::duplex(4096);
        let page = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let batch = CookieBootstrap::parse(r#"[{"name":"PUBLIC-one"}]"#, "inline-arg")
            .unwrap()
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            batch
                .apply(
                    &page,
                    "https://chatgpt.com/",
                    Duration::from_secs(1),
                    &cancel
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), server.next())
                .await
                .is_err()
        );
        page.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[test]
    fn saved_browser_replacement_keeps_values_out_of_intent_and_preserves_the_requirement() {
        let home = tempfile::tempdir().unwrap();
        let input = json!({"engine":"browser","prompt":"PUBLIC authentication privacy regression"});
        let env = Environment::new();
        let mut request = crate::execution::resolve(&input, &json!({}), &env, home.path()).unwrap();
        request.replace_browser_config(json!({"desiredModel":"Latest","thinkingTime":"pro","chatgptUrl":"https://chatgpt.com/","inlineCookies":[{"name":"PUBLIC-cookie","value":"PUBLIC-secret-sentinel"}],"inlineCookiesSource":"PUBLIC-private-path"}),&input,&env,home.path()).unwrap();
        assert!(request.cookie_bootstrap.is_some());
        for saved in [&request.options, &request.browser, &request.browser_preview] {
            let saved = saved.to_string();
            assert!(!saved.contains("PUBLIC-secret-sentinel"));
            assert!(!saved.contains("PUBLIC-private-path"));
        }
        assert_eq!(
            request.browser["cookieBootstrapRequired"]["kind"],
            "explicit-cookies"
        );
        let metadata = crate::browser_runner::prepare(
            &crate::sessions::SessionStore::new(home.path()),
            &request,
            home.path(),
        )
        .unwrap();
        assert!(!metadata.to_string().contains("PUBLIC-secret-sentinel"));
    }
    #[test]
    fn explicit_precedence_falls_back_without_saving_source_values_or_paths() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("PUBLIC-cookies.json"),
            r#"[{"name":"PUBLIC-file","value":"PUBLIC-COOKIE-SENTINEL"}]"#,
        )
        .unwrap();
        let env = Environment::from([(
            "ORACLE_BROWSER_COOKIES_JSON".into(),
            r#"[{"name":"PUBLIC-environment"}]"#.into(),
        )]);
        let input = json!({"browserInlineCookiesFile":"PUBLIC-cookies.json","browserInlineCookies":r#"[{"name":"PUBLIC-argument"}]"#});
        let batch = CookieBootstrap::resolve_explicit(&input, &json!({}), &env, home.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            batch.parameters("https://chatgpt.com/").unwrap()[0]["name"],
            "PUBLIC-file"
        );
        assert!(!format!("{batch:?}").contains("PUBLIC-COOKIE-SENTINEL"));
        assert_eq!(
            batch.requirement(),
            json!({"version":1,"kind":"explicit-cookies","source":"inline-file"})
        );
        let mut input = input;
        input["browserInlineCookiesFile"] = json!("PUBLIC-missing");
        let batch = CookieBootstrap::resolve_explicit(&input, &json!({}), &env, home.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            batch.parameters("https://chatgpt.com/").unwrap()[0]["name"],
            "PUBLIC-argument"
        );
    }
    #[test]
    fn empty_selection_overrides_later_sources_and_invalid_inputs_are_redacted() {
        let env = Environment::from([(
            "ORACLE_BROWSER_COOKIES_JSON".into(),
            r#"[{"name":"PUBLIC-environment"}]"#.into(),
        )]);
        assert!(
            CookieBootstrap::resolve_explicit(
                &json!({"browserInlineCookies":"[]"}),
                &json!({}),
                &env,
                Path::new(".")
            )
            .unwrap()
            .unwrap()
            .cookies
            .is_empty()
        );
        let error = CookieBootstrap::resolve_explicit(
            &json!({"browserInlineCookies":"PUBLIC-INVALID-SECRET"}),
            &json!({}),
            &Environment::new(),
            Path::new("."),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("PUBLIC-INVALID-SECRET"));
    }
    #[test]
    fn cookie_normalisation_preserves_defaults_first_domain_name_and_expiry_units() {
        let batch=CookieBootstrap::parse(r#"[{"name":"PUBLIC-name","value":"first","expires":1700000000000},{"name":"PUBLIC-name","value":"second","path":"/other"},{"name":"PUBLIC-domain","domain":".chatgpt.com","expires":13344473600000000},{"name":"PUBLIC-explicit","url":"https://chat.openai.com/","domain":".chatgpt.com","sameSite":"Lax"},{"value":"skipped"}]"#,"inline-arg").unwrap().unwrap();
        let parameters = batch.parameters("https://chatgpt.com/").unwrap();
        assert_eq!(parameters.len(), 3);
        assert_eq!(
            parameters[0],
            json!({"name":"PUBLIC-name","value":"first","path":"/","secure":true,"httpOnly":false,"url":"https://chatgpt.com","expires":1700000000.0})
        );
        assert_eq!(parameters[1]["expires"], 1700000000.0);
        assert_eq!(parameters[1]["domain"], ".chatgpt.com");
        assert!(parameters[2].get("domain").is_none());
    }
    #[test]
    fn provider_parent_domain_cookies_keep_their_domain_without_broadening_cookie_urls() {
        let batch = CookieBootstrap::parse(
            r#"[{"name":"PUBLIC-parent","value":"PUBLIC-value","domain":".openai.com"}]"#,
            "inline-arg",
        )
        .unwrap()
        .unwrap();
        let parameters = batch.parameters("https://chatgpt.com/").unwrap();
        assert_eq!(parameters[0]["domain"], ".openai.com");
        assert!(parameters[0]["url"].is_null());
        let batch = CookieBootstrap::parse(
            r#"[{"name":"PUBLIC-parent","domain":".openai.com","url":"https://openai.com/"}]"#,
            "inline-arg",
        )
        .unwrap()
        .unwrap();
        assert!(batch.parameters("https://chatgpt.com/").is_err());
    }
    #[test]
    fn cookie_inputs_are_bounded_and_external_or_credential_urls_refuse() {
        assert!(
            CookieBootstrap::parse(&"x".repeat(MAX_COOKIE_INPUT + 1), "inline-arg")
                .unwrap()
                .is_none()
        );
        let encoded = STANDARD.encode(br#"[{"name":"PUBLIC-base64"}]"#);
        assert!(
            CookieBootstrap::parse(&encoded, "inline-arg")
                .unwrap()
                .is_some()
        );
        for raw in [
            r#"[{"name":"PUBLIC-name","domain":"foreign.example"}]"#,
            r#"[{"name":"PUBLIC-name","url":"https://user:password@chatgpt.com/"}]"#,
        ] {
            assert!(
                CookieBootstrap::parse(raw, "inline-arg")
                    .unwrap()
                    .unwrap()
                    .parameters("https://chatgpt.com/")
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn stale_conversation_cookies_keep_open_and_resumed_conversations() {
        for targets_fail in [false, true] {
            let (client, server) = tokio::io::duplex(65536);
            let page = Cdp::from_socket(
                WebSocketStream::from_raw_socket(client, Role::Client, None).await,
                Duration::from_secs(1),
            );
            let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let task = tokio::spawn(async move {
                let mut methods = vec![];
                let mut deleted = vec![];
                while let Some(Ok(Message::Text(raw))) = server.next().await {
                    let command: Value = serde_json::from_str(&raw).unwrap();
                    let method = command["method"].as_str().unwrap().to_owned();
                    let result = match method.as_str() {
                        "Target.getTargets" if targets_fail => {
                            json!({"error":{"code":-1,"message":"PUBLIC-cookie-value-sentinel"}})
                        }
                        "Target.getTargets" => json!({"result":{"targetInfos":[
                            {"targetId":"PUBLIC-a","type":"page","url":"https://chatgpt.com/c/PUBLIC-open"},
                            {"targetId":"PUBLIC-b","type":"page","url":"https://chatgpt.com/g/g-p-public/c/PUBLIC-project?x=1"},
                            {"targetId":"PUBLIC-c","type":"page","url":"https://example.com/c/PUBLIC-foreign"}
                        ]}}),
                        "Network.getAllCookies" => {
                            let cookie = |name: &str, domain: &str, path: Option<&str>| {
                                let mut cookie = json!({"name":name,"value":"PUBLIC-cookie-value-sentinel","domain":domain});
                                if let Some(path) = path {
                                    cookie["path"] = json!(path);
                                }
                                cookie
                            };
                            json!({"result":{"cookies":[
                                cookie("conv_key_PUBLIC-open", ".chatgpt.com", Some("/")),
                                cookie("conv_key_PUBLIC-project", "chatgpt.com", Some("/")),
                                cookie("conv_key_PUBLIC-resumed", ".chatgpt.com", Some("/")),
                                cookie("conv_key_PUBLIC-foreign", ".ChatGPT.com", Some("/")),
                                cookie("conv_key_PUBLIC-legacy", "chat.openai.com", None),
                                cookie("conv_key_PUBLIC-failing", ".chatgpt.com", Some("/PUBLIC")),
                                cookie("conv_key_PUBLIC-elsewhere", ".example.com", Some("/")),
                                cookie("PUBLIC-session", ".chatgpt.com", Some("/"))
                            ]}})
                        }
                        "Network.deleteCookies" => {
                            deleted.push(command["params"].clone());
                            if command["params"]["name"] == "conv_key_PUBLIC-failing" {
                                json!({"error":{"code":-1,"message":"PUBLIC-cookie-value-sentinel"}})
                            } else {
                                json!({"result":{}})
                            }
                        }
                        other => panic!("Unexpected cookie cleanup command {other}"),
                    };
                    methods.push(method);
                    let mut response = result;
                    response["id"] = command["id"].clone();
                    server
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
                (methods, deleted)
            });
            let lines = clear_stale_conversation_cookies(
                &page,
                &page,
                &[None, Some("PUBLIC-resumed".into())],
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await;
            page.disconnect_and_wait(Duration::from_secs(1))
                .await
                .unwrap();
            let (methods, deleted) = task.await.unwrap();
            assert!(
                lines
                    .iter()
                    .all(|line| !line.contains("PUBLIC-cookie-value-sentinel"))
            );
            if targets_fail {
                assert_eq!(methods, ["Target.getTargets"]);
                assert_eq!(
                    lines,
                    [
                        "[cookies] Failed to inspect active ChatGPT conversations; skipping stale cookie cleanup: CDP command Target.getTargets was refused by the endpoint."
                    ]
                );
                continue;
            }
            assert_eq!(
                deleted,
                [
                    json!({"name":"conv_key_PUBLIC-foreign","domain":".ChatGPT.com","path":"/"}),
                    json!({"name":"conv_key_PUBLIC-legacy","domain":"chat.openai.com","path":"/"}),
                    json!({"name":"conv_key_PUBLIC-failing","domain":".chatgpt.com","path":"/PUBLIC"}),
                ]
            );
            assert_eq!(
                lines,
                [
                    "[cookies] Failed to clear a stale ChatGPT conversation cookie: CDP command Network.deleteCookies was refused by the endpoint.",
                    "[cookies] Cleared 2 stale ChatGPT conversation cookies.",
                ]
            );
        }
    }
}
