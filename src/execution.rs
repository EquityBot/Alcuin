use crate::{
    artifacts::{capture, publish, sha256},
    config::{self, Environment},
    policy::{self, ThinkingTime},
    process::{Liveness, ProcessIdentity},
    prompts::{self, PromptFile},
    providers::{
        self, ApiTransport, HttpProvider, ResponseObserver, ResponseProvider, Route, Secret,
    },
    sessions::{self, SessionStore, merge_object, timestamp},
    storage::{DirectoryLock, atomic_json, private_dir},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const BROWSER_GATE: &str = "Rust browser dispatch is gated: native runner fixtures pass, but actual DOM qualification, full attachment/bundle delivery, image-only/output contracts, browser follow-ups and auxiliary lifecycle parity remain open. Use the maintained installation.";
#[derive(Clone, Debug)]
pub struct ResolvedRequest {
    pub options: Value,
    pub engine: String,
    pub files: Vec<PromptFile>,
    pub markdown: String,
    pub user_prompt: String,
    pub browser: Value,
    pub browser_preview: Value,
    pub(crate) cookie_bootstrap: Option<crate::browser_auth::CookieBootstrap>,
    /// Reference ORACLE_MIN_PROMPT_CHARS (default 10) by parseInt; None skips the guard.
    pub min_prompt_chars: Option<i64>,
}
/// Reference writeAssistantOutput success line for a published API answer.
pub(crate) fn saved_output_line(artifacts: &[Value]) -> Option<String> {
    artifacts
        .iter()
        .find(|artifact| artifact["kind"] == "transcript")
        .and_then(|artifact| artifact["path"].as_str())
        .map(|path| format!("Saved assistant output to {path}"))
}
/// Reference duplicate prompt refusal; the CLI prints it on stdout without the error marker.
#[derive(Debug)]
pub struct DuplicatePrompt(pub String);
impl std::fmt::Display for DuplicatePrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A session with the same prompt is already running ({0}). Reattach with \"alcuin session {0}\" or rerun with --force to start another run.",
            self.0
        )
    }
}
impl std::error::Error for DuplicatePrompt {}
/// A run failure already reported as `ERROR: ...`; the CLI prints no second error line.
#[derive(Debug)]
pub struct LoggedFailure(pub anyhow::Error);
impl std::fmt::Display for LoggedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for LoggedFailure {}
/// Reference performSessionRun failure log: the error, then formatBrowserReattachGuidance when a
/// browser run that may still be alive had created a conversation or submitted its prompt.
pub(crate) fn failure_lines(meta: &Value, error: &anyhow::Error) -> Vec<String> {
    let mut lines = vec![format!("ERROR: {error:#}")];
    let runtime = &meta["browser"]["runtime"];
    let reattachable = meta["mode"] == "browser"
        && meta["options"]["browserConfig"]["copyProfileSource"].is_null()
        && (runtime["conversationId"]
            .as_str()
            .is_some_and(|id| !policy::trim(id).is_empty())
            || runtime["tabUrl"]
                .as_str()
                .and_then(crate::browser::conversation_id)
                .is_some()
            || runtime["promptSubmitted"] == true);
    if reattachable {
        let id = meta["id"].as_str().unwrap_or_default();
        lines.extend([
            "This run did not return cleanly, but it may still be alive. Reattach:".to_owned(),
            format!("  alcuin session {id} --render    # final markdown when complete"),
            format!("  alcuin session {id} --live      # tail until done"),
            format!("  alcuin session {id} --harvest   # snapshot the current answer now"),
        ]);
    }
    lines
}
/// Appends the reference failure lines to the session log and returns them.
pub(crate) fn log_failure(
    store: &SessionStore,
    id: &str,
    error: &anyhow::Error,
) -> Result<Vec<String>> {
    let meta = store.require(id)?;
    let lines = failure_lines(&meta, error);
    store.append_log(
        id,
        meta["model"].as_str(),
        &format!("{}\n", lines.join("\n")),
    )?;
    Ok(lines)
}
/// Reference normalizeRunSignature: the trimmed prompt and its non-blank trimmed follow-ups.
fn run_signature(options: &Value) -> (&str, Vec<&str>) {
    let follow_ups = options["browserFollowUps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(policy::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    (
        policy::trim(options["prompt"].as_str().unwrap_or("")),
        follow_ups,
    )
}
/// Reference resolveApiModel under the Alcuin policy: every permitted API model is GPT-6 Astra.
pub(crate) fn api_model(s: &str) -> Result<String> {
    policy::assert_model_allowed(Some(s))?;
    if s.contains('/') {
        return Ok(policy::trim(s).to_lowercase());
    }
    if policy::trim(s)
        .to_lowercase()
        .replace([' ', '_'], "-")
        .ends_with("-pro")
    {
        bail!(
            "GPT-6 Pro is an API reasoning mode, not a model slug. Use --model gpt-6-astra --reasoning-mode pro (or --engine browser --model gpt-6-pro)."
        );
    }
    Ok(policy::DEFAULT_API_MODEL.into())
}
pub fn resolve(
    input: &Value,
    config: &Value,
    env: &Environment,
    cwd: &Path,
) -> Result<ResolvedRequest> {
    resolve_origin(input, config, env, cwd, false)
}
/// MCP consultations alone turn `ORACLE_BROWSER_PROFILE_DIR` into manual login
/// (reference consult.ts); CLI runs use it only as the profile directory.
pub fn resolve_mcp(
    input: &Value,
    config: &Value,
    env: &Environment,
    cwd: &Path,
) -> Result<ResolvedRequest> {
    resolve_origin(input, config, env, cwd, true)
}
/// Over the input budget, as the reference PromptValidationError; the CLI adds the file report.
#[derive(Debug)]
pub struct InputTooLarge(pub String);
impl std::fmt::Display for InputTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for InputTooLarge {}
/// JavaScript parseInt(text, 10): optional sign and leading digits after whitespace.
fn parse_int(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let value = digits[..end].parse::<i64>().ok()?;
    Some(if text.starts_with('-') { -value } else { value })
}
/// The reference engine: explicit or configured, Azure auto-API, then multi-model fan-out.
pub fn resolved_engine(input: &Value, config: &Value, env: &Environment) -> Result<String> {
    let mut engine = config::resolve_engine(input["engine"].as_str(), config, env)?;
    let azure = (input["provider"] != "openai")
        .then(|| providers::resolve_azure(&input["azure"], &config["azure"], env, false))
        .flatten();
    // Reference azureAutoApiRequested: when the engine only defaulted to browser, an Azure
    // endpoint from the environment or user config selects the API. A remote browser host (CLI
    // `remoteBrowser`) keeps the browser. Every Alcuin-permitted model is an Azure candidate.
    let env_engine = env
        .get("ORACLE_ENGINE")
        .map(|value| policy::trim(value).to_lowercase());
    if engine == "browser"
        && input["engine"] != "browser"
        && input["remoteBrowser"] != true
        && env_engine.as_deref() != Some("browser")
        && config["engine"] != "browser"
        && azure
            .as_ref()
            .is_some_and(|azure| !policy::trim(azure["endpoint"].as_str().unwrap_or("")).is_empty())
    {
        engine = "api".into();
    }
    if input["models"]
        .as_array()
        .is_some_and(|models| !models.is_empty())
    {
        anyhow::ensure!(
            input["engine"] != "browser",
            "Multi-model fan-out cannot be combined with browser automation."
        );
        engine = "api".into();
    }
    Ok(engine)
}
fn resolve_origin(
    input: &Value,
    config: &Value,
    env: &Environment,
    cwd: &Path,
    mcp: bool,
) -> Result<ResolvedRequest> {
    let prompt = input["prompt"].as_str().context("Prompt is required.")?;
    let engine = resolved_engine(input, config, env)?;
    let azure = (input["provider"] != "openai")
        .then(|| providers::resolve_azure(&input["azure"], &config["azure"], env, false))
        .flatten();
    let mut requested_models = input["models"].as_array().cloned().unwrap_or_default();
    let requested = input["model"].as_str().or(config["model"].as_str());
    for model in requested_models
        .iter()
        .filter_map(Value::as_str)
        .chain(requested)
    {
        policy::assert_model_allowed(Some(model))?;
    }
    if engine == "api"
        && let Some(key) = input
            .as_object()
            .context("Run options must be an object")?
            .keys()
            .find(|key| {
                // Reference: blank browser follow-ups are dropped before the engine check.
                let blank_followups = key.as_str() == "browserFollowUps"
                    && input[key.as_str()].as_array().is_some_and(|turns| {
                        turns
                            .iter()
                            .all(|turn| turn.as_str().is_some_and(|t| policy::trim(t).is_empty()))
                    });
                !blank_followups
                    && (key.starts_with("browser")
                        || matches!(key.as_str(), "generateImage" | "outputPath" | "copyProfile"))
            })
    {
        ensure!(
            key != "browserFollowUps",
            "--browser-follow-up requires --engine browser."
        );
        bail!("API execution cannot honour requested {key}; use engine browser.");
    }
    let model = if engine == "api" {
        api_model(requested.unwrap_or(policy::DEFAULT_API_MODEL))?
    } else {
        let s = requested.unwrap_or("gpt-6-pro");
        if s.to_lowercase().replace([' ', '_'], "-").ends_with("-pro") {
            "gpt-6-pro".into()
        } else {
            policy::DEFAULT_API_MODEL.into()
        }
    };
    for name in &mut requested_models {
        *name = json!(api_model(
            name.as_str().context("Model names must be strings")?
        )?);
    }
    let mut unique = vec![];
    for model in requested_models {
        if !unique.contains(&model) {
            unique.push(model);
        }
    }
    let model = unique
        .first()
        .and_then(Value::as_str)
        .unwrap_or(&model)
        .to_owned();
    let prompt = if let Some(suffix) = config["promptSuffix"]
        .as_str()
        .filter(|s| !policy::trim(s).is_empty())
    {
        format!("{}\n{suffix}", policy::trim(prompt))
    } else {
        prompt.into()
    };
    let file_inputs = input["files"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("Files must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    // Reference resolveMaxFileSizeBytes: explicit, then trimmed environment, then config.
    let max_bytes = crate::project_sources::maximum_file_bytes(input, config, env)?;
    let explicit_maximum = env
        .get("ORACLE_MAX_FILE_SIZE_BYTES")
        .is_some_and(|value| !policy::trim(value).is_empty())
        || input.get("maxFileSizeBytes").is_some_and(|v| !v.is_null())
        || config.get("maxFileSizeBytes").is_some_and(|v| !v.is_null());
    let files = if engine == "browser" && input["renderOnly"] != true {
        crate::browser_prompt::discover(&file_inputs, cwd, explicit_maximum.then_some(max_bytes))?
    } else {
        prompts::resolve_files(&file_inputs, cwd, max_bytes, true)?
    };
    let system = input["system"]
        .as_str()
        .map(policy::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(prompts::DEFAULT_SYSTEM);
    let markdown = prompts::render(system, &prompt, &files);
    let user_prompt = prompts::build_user_prompt(&prompt, &files);
    let mut options = json!({"prompt":prompt,"file":file_inputs,"model":model,"mode":engine,"maxFileSizeBytes":max_bytes,"search":input["search"].as_bool().unwrap_or(config["search"]!="off")});
    // Reference resolveHeartbeatIntervalMs: whole milliseconds, absent when disabled.
    let heartbeat = input["heartbeatSeconds"]
        .as_f64()
        .or(config["heartbeatSeconds"].as_f64())
        .unwrap_or(30.0);
    if heartbeat > 0.0 {
        options["heartbeatIntervalMs"] = json!((heartbeat * 1000.0).round() as u64);
    }
    if let Some(report) = input.get("filesReport").or(config.get("filesReport")) {
        options["filesReport"] = json!(!report.is_null() && *report != false);
    }
    if engine == "browser" && !explicit_maximum {
        options
            .as_object_mut()
            .context("Browser options must be an object")?
            .remove("maxFileSizeBytes");
    }
    if !unique.is_empty() {
        options["models"] = json!(unique);
    }
    for key in [
        "system",
        "slug",
        "reasoningEffort",
        "reasoningMode",
        "maxInput",
        "maxOutput",
        "provider",
        "baseUrl",
        "azure",
        "background",
        "httpTimeoutMs",
        "timeoutSeconds",
        "writeOutputPath",
        "writeArtifacts",
        "previousResponseId",
        "followupSessionId",
        "followupModel",
        "browserResumeConversationUrl",
        "generateImage",
        "outputPath",
        "browserAttachments",
        "browserBundleFiles",
        "browserBundleFormat",
        "browserFollowUps",
        "waitPreference",
        "force",
        "youtube",
        "editImage",
        "aspectRatio",
        "geminiShowThoughts",
        "geminiAllowModelFallback",
        "zombieTimeoutMs",
        "zombieUseLastActivity",
        "partialMode",
        "verbose",
    ] {
        if let Some(value) = input.get(key) {
            options[key] = value.clone();
        }
    }
    if options.get("baseUrl").is_none()
        && let Some(value) = config.get("apiBaseUrl")
    {
        options["baseUrl"] = value.clone();
    }
    // Resolved once at creation (reference CLI and MCP); the worker reads the saved choice.
    options["notifications"] = crate::notify::resolve(input, env, &config["notify"]);
    match azure {
        Some(azure) => options["azure"] = azure,
        None => {
            options
                .as_object_mut()
                .context("Run options must be an object")?
                .remove("azure");
        }
    }
    if options.get("background").is_none()
        && let Some(value) = config.get("background")
    {
        options["background"] = value.clone();
    }
    if engine == "api" {
        options["reasoningMode"] =
            json!(policy::require_api_pro(options["reasoningMode"].as_str())?);
        if let Some(overrides) = config.get("modelOverrides") {
            options["modelOverrides"] = overrides.clone();
        }
        providers::model_settings(&model, &options)?;
    }
    let mut browser = if config["browser"].is_object() {
        config["browser"].clone()
    } else {
        json!({})
    };
    if let Some(source) = input.get("copyProfile") {
        browser["copyProfileSource"] = source.clone();
    }
    let copying = engine == "browser" && !browser["copyProfileSource"].is_null();
    if copying {
        anyhow::ensure!(
            browser["copyProfileSource"]
                .as_str()
                .is_some_and(|source| !source.trim().is_empty()),
            "Copy-profile requires a source user-data directory"
        );
    }
    // Ordinary Chrome sync options select the source profile read by the native worker.
    if input["browserNoCookieSync"] == true {
        browser["cookieSync"] = json!(false);
        browser["manualLoginCookieSync"] = json!(false);
    } else if input["browserCookieSync"] == true {
        browser["cookieSync"] = json!(true);
    }
    if let Some(path) = input["browserCookiePath"].as_str() {
        browser["chromeCookiePath"] = json!(path);
    }
    if let Some(raw) = input["browserCookieNames"]
        .as_str()
        .or_else(|| env.get("ORACLE_BROWSER_COOKIE_NAMES").map(String::as_str))
    {
        let names = raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        browser["cookieNames"] = if names.is_empty() {
            Value::Null
        } else {
            json!(names)
        };
    }
    if let Some(raw) = input["browserCookieWait"].as_str() {
        let wait = crate::followup::attachment_timeout(raw).unwrap_or_else(|| {
            println!(
                "Warning: invalid --browser-cookie-wait duration \"{raw}\"; using fallback 0ms."
            );
            0.0
        });
        browser["cookieSyncWaitMs"] = json!(wait.min(u64::MAX as f64) as u64);
    }
    if input["browserAllowCookieErrors"] == true
        || browser["allowCookieErrors"].is_null()
            && env
                .get("ORACLE_BROWSER_ALLOW_COOKIE_ERRORS")
                .is_some_and(|raw| raw.trim().eq_ignore_ascii_case("true") || raw.trim() == "1")
    {
        // Accepted for compatibility: an ordinary run without applied cookies still refuses
        // before navigation, as the reference does after its permitted read failure.
        browser["allowCookieErrors"] = json!(true);
    }
    if browser["debugPort"].is_null()
        && let Some(port) = env
            .get("ORACLE_BROWSER_PORT")
            .or_else(|| env.get("ORACLE_BROWSER_DEBUG_PORT"))
            .map(|raw| raw.trim())
            .filter(|raw| !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|raw| raw.parse::<u16>().ok())
            .filter(|port| *port > 0)
    {
        browser["debugPort"] = json!(port);
    }
    if let Some(profile) = input.get("browserChromeProfile") {
        anyhow::ensure!(
            copying || profile == "Default" || browser["cookieSync"] == true,
            "Custom Chrome profile selection requires copy-profile or Chrome cookie sync"
        );
        browser["chromeProfile"] = profile.clone();
    }
    let profile = env
        .get("ORACLE_BROWSER_PROFILE_DIR")
        .map(|path| policy::trim(path))
        .filter(|path| !path.is_empty());
    let manual_choice = input["browserManualLogin"]
        .as_bool()
        .or(browser["manualLogin"].as_bool());
    anyhow::ensure!(
        !copying
            || profile.is_none()
                && manual_choice != Some(true)
                && input["browserManualLoginProfileDir"].is_null()
                && browser["manualLoginProfileDir"].is_null(),
        "Copy-profile cannot be combined with a persistent manual-login profile"
    );
    let manual = !copying && (mcp && profile.is_some() || manual_choice.unwrap_or(cfg!(windows)));
    browser["manualLogin"] = json!(manual);
    browser["manualLoginProfileDir"] = if manual {
        input
            .get("browserManualLoginProfileDir")
            .unwrap_or(&browser["manualLoginProfileDir"])
            .as_str()
            .map(policy::trim)
            .filter(|path| !path.is_empty())
            .map(|path| json!(path))
            .or_else(|| profile.map(|s| json!(s)))
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let url = browser["chatgptUrl"]
        .as_str()
        .or(browser["url"].as_str())
        .unwrap_or("https://chatgpt.com/")
        .to_owned();
    browser["url"] = json!(url);
    browser["chatgptUrl"] = json!(url);
    browser["desiredModel"] = json!(
        input["browserModelLabel"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("GPT-6")
    );
    browser["requestedModel"] = json!(requested.unwrap_or(&model));
    browser["modelIsImplicitDefault"] =
        json!(requested.is_none() && input.get("browserModelLabel").is_none());
    let time = input["browserThinkingTime"]
        .as_str()
        .or(browser["thinkingTime"].as_str())
        .unwrap_or("pro");
    browser["thinkingTime"] =
        json!(ThinkingTime::parse(time).context("Unknown browser thinking time")?);
    for (input_key, key) in [
        ("browserModelStrategy", "modelStrategy"),
        ("browserResearchMode", "researchMode"),
        ("browserArchive", "archiveConversations"),
        ("browserKeepBrowser", "keepBrowser"),
        ("browserHeadless", "headless"),
        ("browserHideWindow", "hideWindow"),
    ] {
        if let Some(value) = input.get(input_key) {
            browser[key] = value.clone();
        }
    }
    if let Some(raw) = browser["modelStrategy"].as_str() {
        let normalised = policy::trim(raw).to_lowercase();
        browser["modelStrategy"] = match normalised.as_str() {
            "" => Value::Null,
            "select" | "current" | "ignore" => json!(normalised),
            _ => bail!(
                "Invalid browser model strategy: \"{raw}\". Expected \"select\", \"current\", or \"ignore\"."
            ),
        };
    }
    if !browser["researchMode"].is_null()
        && !matches!(browser["researchMode"].as_str(), Some("search" | "deep"))
    {
        browser["researchMode"] = json!("off");
    }
    for (key, default) in [
        ("keepBrowser", false),
        ("headless", false),
        ("hideWindow", false),
    ] {
        if browser.get(key).is_none() {
            browser[key] = json!(default);
        }
    }
    crate::browser_provider::Consent::resolve(input, &browser)?.save(&mut browser);
    let cookie_bootstrap = if engine == "browser" {
        crate::browser_auth::prepare_config(input, &mut browser, env, cwd)?
    } else {
        None
    };
    anyhow::ensure!(
        !copying
            || browser["keepBrowser"] != true
                && browser["attachRunning"] != true
                && !browser["remoteChrome"].is_object()
                && cookie_bootstrap.is_none(),
        "Copy-profile requires a disposable local browser without keep-browser, borrowed attachment or explicit cookies"
    );
    // Reference shouldSyncBrowserCookies: a manual-login profile needs both switches.
    browser["cookieSync"] = json!(
        browser["cookieSync"] == true && (!manual || browser["manualLoginCookieSync"] == true)
    );
    // The configured value is otherwise ignored; refuse it before any session exists.
    anyhow::ensure!(
        engine != "browser" || browser["captureProviderNative"] != true,
        "browser.captureProviderNative (provider-native capture): this build does not support it. Remove the setting from your configuration."
    );
    if engine == "browser" {
        options["browserConfig"] = browser.clone();
    }
    let mut browser_preview = browser.clone();
    browser_preview["manualLogin"] = json!(manual_choice.unwrap_or(false));
    Ok(ResolvedRequest {
        options,
        engine,
        files,
        markdown,
        user_prompt,
        browser,
        browser_preview,
        cookie_bootstrap,
        min_prompt_chars: parse_int(
            env.get("ORACLE_MIN_PROMPT_CHARS")
                .map_or("10", String::as_str),
        ),
    })
}
impl ResolvedRequest {
    pub(crate) fn replace_browser_config(
        &mut self,
        mut browser: Value,
        input: &Value,
        env: &Environment,
        cwd: &Path,
    ) -> Result<()> {
        self.cookie_bootstrap = crate::browser_auth::prepare_config(input, &mut browser, env, cwd)?;
        self.options["browserConfig"] = browser.clone();
        self.browser_preview = browser.clone();
        self.browser = browser;
        Ok(())
    }
}
/// Reference buildConsultDryRunResolved.
pub fn dry_run_resolved(request: &ResolvedRequest) -> Value {
    let options = &request.options;
    let browser = &request.browser;
    let followups = options["browserFollowUps"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s.as_str().is_some_and(|s| !s.trim().is_empty()))
                .count()
        })
        .unwrap_or(0);
    let mut details = json!({"resolvedEngine":request.engine,"model":options["model"],"files":options["file"],"followUpCount":followups,"guidance":[]});
    if let Some(models) = options.get("models") {
        details["models"] = models.clone();
    }
    let mut guidance = vec![];
    if request.engine == "browser" {
        guidance.push(
            "Browser engine uses the signed-in ChatGPT profile; run dryRun:true before live use."
                .to_owned(),
        );
        if browser["manualLogin"] == true {
            let profile = browser["manualLoginProfileDir"]
                .as_str()
                .unwrap_or("~/.oracle/browser-profile");
            guidance.push(format!("Manual-login browser mode uses Alcuin's private Chrome profile at {profile}, separate from your normal Chrome profile."));
            guidance.push(format!("First-time setup: run alcuin --engine browser --browser-manual-login --browser-keep-browser --browser-manual-login-profile-dir {} -p \"HI\", sign into ChatGPT in that window, then retry the consult.", json!(profile)));
            guidance.push("If this profile is not signed in, non-setup MCP/browser runs fail fast instead of waiting for the full browser timeout.".into());
        }
    } else {
        guidance.push("API engine requires provider credentials. If the operator has ChatGPT Pro but no API key, retry with engine:\"browser\" or preset:\"chatgpt-pro-heavy\".".into());
    }
    let url = browser["chatgptUrl"].as_str().or(browser["url"].as_str());
    if url.is_some_and(|url| url.contains("/project")) {
        guidance.push("This ChatGPT project URL is persistent. Project Sources should be mutated only by the project_sources tool with confirmMutation:true.".into());
    }
    if followups > 0 {
        guidance.push("This is a multi-turn browser consult; all follow-ups stay in one ChatGPT conversation.".into());
    }
    let image = options
        .get("generateImage")
        .or(options.get("outputPath"))
        .cloned()
        .unwrap_or(Value::Null);
    if request.engine == "browser" {
        if !image.is_null() {
            guidance.push("ChatGPT generated images will use the image-aware wait/download path and return saved files in structuredContent.images.".into());
        }
        details["browser"] = json!({"desiredModel":browser["desiredModel"],"thinkingTime":browser["thinkingTime"],"modelStrategy":browser["modelStrategy"],"researchMode":browser["researchMode"],"fallbackProvider":browser["fallbackProvider"],"manualLogin":browser["manualLogin"],"profileDir":browser["manualLoginProfileDir"],"chatgptUrl":url,"imageOutputPath":image,"keepBrowser":browser["keepBrowser"]});
        for (source, target) in [
            ("browserAttachments", "attachments"),
            ("browserBundleFiles", "bundleFiles"),
            ("browserBundleFormat", "bundleFormat"),
        ] {
            if let Some(value) = options.get(source) {
                details["browser"][target] = value.clone();
            }
        }
    }
    details["guidance"] = json!(guidance);
    details
}
/// Reference formatConsultDryRunResolved.
pub fn dry_run_lines(details: &Value) -> Vec<String> {
    let text = |value: &Value, default: &str| value.as_str().unwrap_or(default).to_owned();
    let yes = |value: &Value| if value == true { "yes" } else { "no" };
    let mut lines = vec![
        "[dry-run] MCP resolved request:".to_owned(),
        format!("  engine: {}", text(&details["resolvedEngine"], "")),
        format!("  model: {}", text(&details["model"], "undefined")),
    ];
    if let Some(models) = details["models"].as_array().filter(|a| !a.is_empty()) {
        lines.push(format!(
            "  models: {}",
            models
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.push(format!(
        "  files: {}",
        details["files"].as_array().map_or(0, Vec::len)
    ));
    let browser = &details["browser"];
    if browser.is_object() {
        lines.extend([
            format!(
                "  browser desired model: {}",
                text(&browser["desiredModel"], "(default)")
            ),
            format!(
                "  browser thinking time: {}",
                text(&browser["thinkingTime"], "(default)")
            ),
            format!(
                "  browser model strategy: {}",
                text(&browser["modelStrategy"], "(default)")
            ),
            format!(
                "  browser research mode: {}",
                text(&browser["researchMode"], "off")
            ),
            format!(
                "  browser attachments: {}",
                text(&browser["attachments"], "auto")
            ),
            format!("  browser bundle files: {}", yes(&browser["bundleFiles"])),
            format!(
                "  browser bundle format: {}",
                text(&browser["bundleFormat"], "auto")
            ),
            format!("  browser keep browser: {}", yes(&browser["keepBrowser"])),
            format!("  browser manual login: {}", yes(&browser["manualLogin"])),
        ]);
        for (key, label) in [
            ("profileDir", "browser profile"),
            ("chatgptUrl", "ChatGPT URL"),
            ("imageOutputPath", "image output"),
        ] {
            if let Some(value) = browser[key].as_str().filter(|value| !value.is_empty()) {
                lines.push(format!("  {label}: {value}"));
            }
        }
    }
    lines.push(format!("  follow-ups: {}", details["followUpCount"]));
    lines.extend(
        details["guidance"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|line| format!("  guidance: {line}")),
    );
    lines
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommittedRequest {
    pub version: u8,
    pub reservation_id: String,
    pub session_id: String,
    pub route: Route,
    pub body: Value,
    pub body_sha256: String,
}
pub fn prepare(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    route: Route,
) -> Result<Value> {
    prepare_with_base(store, request, cwd, route, None)
}
pub(crate) fn prepare_restart(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    route: Route,
    source: &str,
) -> Result<Value> {
    prepare_with_base(store, request, cwd, route, Some(source))
}
fn prepare_with_base(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    route: Route,
    source: Option<&str>,
) -> Result<Value> {
    if request.engine == "browser" {
        bail!("{BROWSER_GATE}");
    }
    for key in [
        "generateImage",
        "outputPath",
        "browserAttachments",
        "browserBundleFiles",
        "browserBundleFormat",
        "browserFollowUps",
    ] {
        anyhow::ensure!(
            request.options.get(key).is_none(),
            "API dispatch cannot honour requested {key}; use engine browser."
        );
    }
    anyhow::ensure!(
        request.options["models"]
            .as_array()
            .is_none_or(|a| a.len() <= 1),
        "Rust multi-model dispatch is gated until fan-out and partial-result parity is qualified."
    );
    // Alcuin API runs always use Pro reasoning, which the Chat Completions adapter refuses.
    anyhow::ensure!(
        route.transport == ApiTransport::Responses,
        "--reasoning-mode requires the OpenAI or Azure OpenAI Responses API; OpenRouter and custom --base-url routes use the Chat Completions adapter."
    );
    let length = crate::policy::trim(request.options["prompt"].as_str().unwrap_or(""))
        .encode_utf16()
        .count() as i64;
    if let Some(minimum) = request.min_prompt_chars.filter(|minimum| length < *minimum) {
        bail!(
            "Prompt is too short (<{minimum} chars). This was likely accidental; please provide more detail."
        );
    }
    let mut body = providers::build_request(
        &route.model,
        &request.options,
        &request.user_prompt,
        request.options["system"]
            .as_str()
            .unwrap_or(prompts::DEFAULT_SYSTEM),
    )?;
    if let Some(deployment) = &route.azure_deployment_name {
        body["model"] = json!(deployment);
    }
    let settings = providers::model_settings(&route.model, &request.options)?;
    let estimate = crate::tokenizer::estimate_request(&body)?;
    let budget = request.options["maxInput"]
        .as_u64()
        .unwrap_or(settings.input_limit) as usize;
    if estimate > budget {
        return Err(InputTooLarge(format!(
            "Input too large ({} tokens). Limit is {} tokens.",
            crate::cli::grouped(estimate),
            crate::cli::grouped(budget)
        ))
        .into());
    }
    private_dir(store.home())?;
    let admission = DirectoryLock::acquire(
        &store.home().join("dispatch-admission.lock"),
        Duration::from_secs(10),
    )?;
    if source.is_none() && request.options["force"] != true {
        let signature = run_signature(&request.options);
        for meta in store.list(false)? {
            // Pending sessions also block: they hold an admitted dispatch that has not started.
            if matches!(meta["status"].as_str(), Some("pending" | "running"))
                && run_signature(&meta["options"]) == signature
            {
                return Err(
                    DuplicatePrompt(meta["id"].as_str().unwrap_or("unknown").into()).into(),
                );
            }
        }
    }
    let session = store.create(request.options.clone(), cwd, source)?;
    let id = session["id"].as_str().context("Missing new session ID")?;
    let committed = CommittedRequest {
        version: 1,
        reservation_id: Uuid::new_v4().to_string(),
        session_id: id.into(),
        route,
        body_sha256: sha256(&serde_json::to_vec(&body)?),
        body,
    };
    atomic_json(
        &store.directory(id)?.join("dispatch-request.json"),
        &committed,
    )?;
    admission.release()?;
    Ok(session)
}
pub fn committed_request(store: &SessionStore, id: &str) -> Result<CommittedRequest> {
    let committed: CommittedRequest = serde_json::from_slice(&fs::read(
        store.directory(id)?.join("dispatch-request.json"),
    )?)?;
    anyhow::ensure!(
        committed.version == 1
            && committed.session_id == id
            && sha256(&serde_json::to_vec(&committed.body)?) == committed.body_sha256,
        "Committed request identity or source changed; refusing dispatch."
    );
    policy::assert_model_allowed(Some(&committed.route.model))?;
    policy::require_api_pro(committed.body["reasoning"]["mode"].as_str())?;
    anyhow::ensure!(
        committed.body["reasoning"]["mode"] == "pro",
        "Committed API request lacks Pro reasoning mode."
    );
    Ok(committed)
}
pub fn start_owner(
    store: &SessionStore,
    id: &str,
    identity: &ProcessIdentity,
    detached: bool,
) -> Result<Value> {
    anyhow::ensure!(
        identity.liveness() == Liveness::Alive,
        "Cannot assign an unknown or dead worker process identity"
    );
    if store.require(id)?["mode"] == "browser" {
        crate::browser_runner::start_owner(store, id, identity)?;
        return store.update(id,|meta|Ok(Some(json!({"lifecycle":merge_object(&meta["lifecycle"],&json!({"execution":if detached{"background"}else{"foreground"},"attached":!detached,"detached":detached,"reattachCommand":format!("alcuin session {id}")}))}))));
    }
    store.update(id,|current|{
        anyhow::ensure!(current["status"]=="pending"&&current.get("dispatch").is_none(),"Session has already started; refusing automatic replay.");
        Ok(Some(json!({"status":"running","startedAt":timestamp(),"lifecycle":{"engine":"api","execution":if detached{"background"}else{"foreground"},"attached":!detached,"detached":detached,"workerPid":identity.pid,"workerProcessStartedAtMs":identity.started_at_ms,"processIdentity":identity,"reattachCommand":format!("alcuin session {id}")}})))
    })
}
fn owns(meta: &Value, identity: &ProcessIdentity) -> bool {
    serde_json::from_value::<ProcessIdentity>(meta["lifecycle"]["processIdentity"].clone())
        .is_ok_and(|i| i == *identity)
}
pub(crate) fn controller_identity(metadata: &Value) -> Value {
    let mut controller = json!({});
    for key in ["id", "model", "mode", "startedAt"] {
        if let Some(value) = metadata.get(key) {
            controller[key] = value.clone();
        }
    }
    for key in [
        "engine",
        "execution",
        "detached",
        "workerPid",
        "workerProcessStartedAtMs",
        "processIdentity",
    ] {
        if let Some(value) = metadata["lifecycle"].get(key) {
            controller[format!("lifecycle.{key}")] = value.clone();
        }
    }
    for key in ["controllerPid", "controllerProcessStartedAtMs"] {
        if let Some(value) = metadata["browser"]["runtime"].get(key) {
            controller[format!("browser.{key}")] = value.clone();
        }
    }
    controller
}
pub(crate) struct ExecutionWitness {
    path: std::path::PathBuf,
    directory: crate::native_fs::NativeEntry,
    controller: Value,
    authority: WitnessAuthority,
}
enum WitnessAuthority {
    Worker(ProcessIdentity),
    Recovery(std::sync::Arc<crate::browser_attempt::RecoveryGuard>),
}
impl ExecutionWitness {
    pub(crate) fn capture(
        store: &SessionStore,
        id: &str,
        identity: &ProcessIdentity,
    ) -> Result<Self> {
        anyhow::ensure!(
            *identity == ProcessIdentity::current()? && identity.liveness() == Liveness::Alive,
            "A worker witness requires the exact live current process"
        );
        let path = store.directory(id)?;
        let directory = crate::native_fs::NativeEntry::open(&path, true)?;
        let metadata = store.require(id)?;
        directory.require_matches(&path)?;
        Ok(Self {
            path,
            directory,
            controller: controller_identity(&metadata),
            authority: WitnessAuthority::Worker(identity.clone()),
        })
    }
    pub(crate) fn capture_recovery(
        store: &SessionStore,
        id: &str,
        guard: std::sync::Arc<crate::browser_attempt::RecoveryGuard>,
    ) -> Result<Self> {
        let path = store.directory(id)?;
        let directory = crate::native_fs::NativeEntry::open(&path, true)?;
        let metadata = store.stored_for_update(id)?;
        guard.require_session(id, &metadata)?;
        directory.require_matches(&path)?;
        Ok(Self {
            path,
            directory,
            controller: controller_identity(&metadata),
            authority: WitnessAuthority::Recovery(guard),
        })
    }
    pub(crate) fn require_worker(&self, metadata: &Value) -> Result<()> {
        self.require(metadata)?;
        match &self.authority {
            WitnessAuthority::Worker(identity) => anyhow::ensure!(
                *identity == ProcessIdentity::current()? && identity.liveness() == Liveness::Alive,
                "Browser mutation requires the exact live current worker"
            ),
            WitnessAuthority::Recovery(_) => {
                anyhow::bail!(
                    "Read-only recovery cannot authorise browser submission or another attempt"
                )
            }
        }
        Ok(())
    }
    pub(crate) fn recovery_deadline(&self) -> Option<std::time::Instant> {
        match &self.authority {
            WitnessAuthority::Worker(_) => None,
            WitnessAuthority::Recovery(guard) => Some(guard.deadline()),
        }
    }
    pub(crate) fn seal_recovered_turn(
        &self,
        before: &Value,
        after: &Value,
        observed: &Value,
    ) -> Result<()> {
        if let WitnessAuthority::Recovery(guard) = &self.authority {
            guard.seal_turn(before, after, observed)?;
        }
        Ok(())
    }
    pub(crate) fn seal_recovered_delivery(
        &self,
        before: &Value,
        after: &Value,
        images: &Value,
    ) -> Result<()> {
        if let WitnessAuthority::Recovery(guard) = &self.authority {
            guard.seal_delivery(before, after, images)?;
        }
        Ok(())
    }
    pub(crate) fn require(&self, metadata: &Value) -> Result<()> {
        self.directory.require_matches(&self.path)?;
        anyhow::ensure!(
            controller_identity(metadata) == self.controller,
            "Session controller incarnation changed; refusing late worker writes"
        );
        match &self.authority {
            WitnessAuthority::Worker(identity) => anyhow::ensure!(
                owns(metadata, identity),
                "Session no longer belongs to this worker"
            ),
            WitnessAuthority::Recovery(guard) => guard.require_session(
                metadata["id"]
                    .as_str()
                    .context("Recovery session ID missing")?,
                metadata,
            )?,
        }
        Ok(())
    }
}
/// Overall API timeout: the saved seconds, else the reference Pro default of 60 minutes.
pub(crate) fn api_timeout(options: &Value) -> Result<Duration> {
    Duration::try_from_secs_f64(
        options["timeoutSeconds"]
            .as_f64()
            .filter(|n| n.is_finite() && *n > 0.0)
            .unwrap_or(3600.0),
    )
    .context("Invalid API timeout duration")
}
/// HTTP transport timeout: the saved milliseconds, else the overall timeout (reference run.ts).
pub(crate) fn http_timeout(options: &Value) -> Result<Duration> {
    match options["httpTimeoutMs"]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
    {
        Some(ms) => Ok(Duration::from_millis(ms as u64)),
        None => api_timeout(options),
    }
}
fn heartbeat_interval(options: &Value) -> Option<Duration> {
    options["heartbeatIntervalMs"]
        .as_f64()
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map(|ms| Duration::from_millis(ms as u64))
}
/// Reference `formatElapsed`.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms >= 3_600_000 {
        format!("{}h {}m", ms / 3_600_000, ms % 3_600_000 / 60_000)
    } else if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, ms % 60_000 / 1000)
    } else if ms >= 1000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}
/// Reference heartbeat: logs `message(elapsed)` every interval while `active`, ignoring log
/// errors. It never completes, so callers race it against the work it reports on.
async fn heartbeat(
    output: Option<&dyn ResponseObserver>,
    interval: Option<Duration>,
    active: impl Fn() -> bool,
    message: impl Fn(Duration) -> String,
) {
    let (Some(output), Some(interval)) = (output, interval) else {
        return std::future::pending().await;
    };
    let start = tokio::time::Instant::now();
    let mut ticks = tokio::time::interval_at(start + interval, interval);
    loop {
        ticks.tick().await;
        if !active() {
            return std::future::pending().await;
        }
        let _ = output.log(&message(start.elapsed()));
    }
}
pub async fn execute(
    store: &SessionStore,
    id: &str,
    provider: &impl ResponseProvider,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
) -> Result<Value> {
    execute_observed(store, id, provider, identity, cancel, None).await
}
pub async fn execute_observed(
    store: &SessionStore,
    id: &str,
    provider: &impl ResponseProvider,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    output: Option<&dyn ResponseObserver>,
) -> Result<Value> {
    execute_with(store, id, provider, identity, cancel, output, false).await
}
/// The detached worker's run: reference executeSession hands runOracle the session log
/// writer, so progress lines, the "Answer:" heading and the finish lines go to the log.
pub async fn execute_logged(
    store: &SessionStore,
    id: &str,
    provider: &impl ResponseProvider,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    output: Option<&dyn ResponseObserver>,
) -> Result<Value> {
    execute_with(store, id, provider, identity, cancel, output, true).await
}
/// Sends progress lines to the caller's output and the session log, as reference combinedLog.
struct Tee<'a> {
    output: &'a dyn ResponseObserver,
    log: &'a SessionLog<'a>,
}
impl providers::ResponseObserver for Tee<'_> {
    fn response(&self, response: &Value) -> Result<()> {
        self.output.response(response)
    }
    fn delta(&self, text: &str) -> Result<()> {
        self.output.delta(text)
    }
    fn log(&self, line: &str) -> Result<()> {
        self.output.log(line)?;
        self.log.log(line)
    }
}
/// Writes progress lines to the session's model log, as the reference logLine does.
struct SessionLog<'a> {
    store: &'a SessionStore,
    id: &'a str,
    model: String,
}
impl providers::ResponseObserver for SessionLog<'_> {
    fn response(&self, _: &Value) -> Result<()> {
        Ok(())
    }
    fn delta(&self, _: &str) -> Result<()> {
        // SessionResponseObserver already appends every delta to the same log.
        Ok(())
    }
    fn log(&self, line: &str) -> Result<()> {
        self.store
            .append_log(self.id, Some(&self.model), &format!("{line}\n"))
    }
}
async fn execute_with(
    store: &SessionStore,
    id: &str,
    provider: &impl ResponseProvider,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    output: Option<&dyn ResponseObserver>,
    session_log: bool,
) -> Result<Value> {
    anyhow::ensure!(
        *identity == ProcessIdentity::current()? && identity.liveness() == Liveness::Alive,
        "Executing worker does not have the exact current native process identity"
    );
    let committed = committed_request(store, id)?;
    anyhow::ensure!(
        provider.route() == &committed.route,
        "Provider route changed after prompt commitment."
    );
    let witness = ExecutionWitness::capture(store, id, identity)?;
    let logged = session_log.then(|| SessionLog {
        store,
        id,
        model: committed.route.model.clone(),
    });
    let tee = logged
        .as_ref()
        .zip(output)
        .map(|(log, output)| Tee { output, log });
    let output = match (&tee, &logged) {
        (Some(tee), _) => Some(tee as &dyn ResponseObserver),
        (None, Some(log)) => Some(log as &dyn ResponseObserver),
        (None, None) => output,
    };
    let mut issued = false;
    let reservation=store.update(id,|current|{
        witness.require(current)?;
        anyhow::ensure!(current["status"]=="running","Session worker ownership changed.");
        anyhow::ensure!(current.get("dispatch").is_none(),"Dispatch already issued or uncertain; refusing automatic replay.");
        if cancel.is_cancelled(){bail!("Alcuin API request was cancelled before dispatch.");}
        issued=true;
        Ok(Some(json!({"dispatch":{"reservationId":committed.reservation_id,"state":"started","bodySha256":committed.body_sha256,"startedAt":timestamp()},"recoveryRequired":true})))
    });
    if let Err(error) = reservation {
        if cancel.is_cancelled() {
            store.update(id,|current|{
            if witness.require(current).is_err()||current.get("dispatch").is_some(){return Ok(None);}
            Ok(Some(json!({"status":"cancelled","completedAt":timestamp(),"recoveryRequired":false})))
        })?;
        }
        return Err(error);
    }
    let result = execute_issued(
        store,
        id,
        provider,
        &witness,
        &committed,
        cancel,
        output,
        session_log,
    )
    .await;
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if issued {
                let message = error.to_string();
                store.update(id,|current|{
                    if witness.require(current).is_err()||sessions::terminal(current["status"].as_str().unwrap_or("")){return Ok(None);}
                    Ok(Some(json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":message,"recoveryRequired":true,"dispatch":merge_object(&current["dispatch"],&json!({"state":"uncertain"}))})))
                })?;
                if let Some(provider_error) = error.downcast_ref::<providers::ProviderError>() {
                    // Reference `transport: {reason}`, beside the retained request evidence.
                    store.update(id, |current| {
                        witness.require(current)?;
                        Ok(Some(json!({"transport":{"reason":provider_error.reason},"transportError":{"reason":provider_error.reason,"message":provider_error.message,"requestId":provider_error.request_id,"status":provider_error.status}})))
                    })?;
                }
            }
            Err(error)
        }
    }
}
struct SessionResponseObserver<'a> {
    store: &'a SessionStore,
    id: &'a str,
    model: &'a str,
    witness: &'a ExecutionWitness,
    streamed: AtomicBool,
    output: Option<&'a dyn ResponseObserver>,
    session_log: bool,
}
impl providers::ResponseObserver for SessionResponseObserver<'_> {
    fn response(&self, response: &Value) -> Result<()> {
        self.witness.require(&self.store.require(self.id)?)?;
        atomic_json(
            &self
                .store
                .directory(self.id)?
                .join("provider-response.json"),
            response,
        )?;
        self.store.update(self.id, |current| {
            self.witness.require(current)?;
            let mut dispatch = merge_object(&current["dispatch"], &json!({"state":"acknowledged"}));
            if let Some(id) = response["id"].as_str() {
                anyhow::ensure!(
                    providers::valid_response_id(id),
                    "API returned an invalid response ID"
                );
                if let Some(previous) = current["dispatch"]["responseId"].as_str() {
                    anyhow::ensure!(previous == id, "Provider response identity changed");
                }
                dispatch["responseId"] = json!(id);
            }
            let mut metadata = providers::response_metadata(response);
            if let Some(id) = response.get("id") {
                metadata["id"] = id.clone();
            }
            Ok(Some(json!({"response":metadata,"dispatch":dispatch})))
        })?;
        if let Some(output) = self.output {
            output.response(response)?;
        }
        Ok(())
    }
    fn delta(&self, text: &str) -> Result<()> {
        self.witness.require(&self.store.require(self.id)?)?;
        // Reference ensureAnswerHeader: a blank line and "Answer:" before the first delta.
        let heading = if self.session_log && !self.streamed.load(Ordering::Relaxed) {
            "\nAnswer:\n"
        } else {
            ""
        };
        self.store
            .append_log(self.id, Some(self.model), &format!("{heading}{text}"))?;
        self.streamed.store(true, Ordering::Relaxed);
        if let Some(output) = self.output {
            output.delta(text)?;
        }
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)] // The worker's log choice travels with the run's evidence.
async fn execute_issued(
    store: &SessionStore,
    id: &str,
    provider: &impl ResponseProvider,
    witness: &ExecutionWitness,
    committed: &CommittedRequest,
    cancel: &CancellationToken,
    output: Option<&dyn ResponseObserver>,
    session_log: bool,
) -> Result<Value> {
    let started = Instant::now();
    let options = store.require(id)?["options"].clone();
    let timeout = api_timeout(&options)?;
    let beat = heartbeat_interval(&options);
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .context("API timeout exceeds the native deadline range")?;
    let observer = SessionResponseObserver {
        store,
        id,
        model: &committed.route.model,
        witness,
        streamed: AtomicBool::new(false),
        output,
        session_log,
    };
    let background = committed.body["background"] == true;
    // Reference streaming heartbeat: until the first answer delta arrives.
    let connection_beat = heartbeat(
        output,
        beat.filter(|_| !background),
        || !observer.streamed.load(Ordering::Relaxed),
        |elapsed| {
            let remaining = timeout.saturating_sub(elapsed).as_millis();
            let remaining = if remaining >= 60_000 {
                format!("{} min", remaining.div_ceil(60_000))
            } else {
                format!("{}s", remaining.div_ceil(1000).max(1))
            };
            format!(
                "API connection active — {} elapsed. Timeout in ~{remaining} if no response.",
                format_elapsed(elapsed)
            )
        },
    );
    let log = |line: String| output.map_or(Ok(()), |output| output.log(&line));
    let overall = format!(
        "Timed out waiting for API response after {}.",
        format_elapsed(timeout)
    );
    let created = tokio::select! {biased;_=cancel.cancelled()=>Err(providers::cancelled_error("Alcuin API request was cancelled; dispatch may have reached the provider.")),result=tokio::time::timeout_at(deadline,provider.create_observed(&committed.body,cancel,&observer))=>result.map_err(|_|providers::timeout_error(&overall)).and_then(|result|result),_=connection_beat=>unreachable!("heartbeat never completes")};
    // Reference describeTransportError line before the failure is rethrown.
    if let Err(error) = &created
        && let Some(failure) = error.downcast_ref::<providers::ProviderError>()
    {
        log(providers::describe(failure, Some(timeout)))?;
    }
    let response = created?;
    let response_id = response["id"]
        .as_str()
        .filter(|s| providers::valid_response_id(s))
        .map(str::to_owned);
    anyhow::ensure!(
        !background || response_id.is_some(),
        "API did not return a response ID for the background run."
    );
    let grace_deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(180))
        .context("Invalid foreground grace deadline")?
        .min(deadline);
    if background {
        log(format!(
            "API scheduled background response {} (status={}). Monitoring up to {} minutes for completion...",
            response_id.as_deref().unwrap_or(""),
            response["status"].as_str().unwrap_or("unknown"),
            (timeout.as_millis() as f64 / 60_000.0).round()
        ))?;
    }
    // Reference retry and reconnect notices: printed and logged as they happen.
    let notice = |line: String| {
        if session_log {
            log(line)
        } else {
            store.append_log(id, Some(&committed.route.model), &format!("{line}\n"))
        }
    };
    let ended_early = |response: &Value| {
        let reason = response["incomplete_details"]["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
            .map(|reason| format!(", reason={reason}"))
            .unwrap_or_default();
        log(format!(
            "API ended the run early (status={}{reason}).",
            response["status"].as_str().unwrap_or("unknown")
        ))
    };
    let poll = async {
        let mut response = response;
        let mut last_status: Option<String> = None;
        let mut announced = false;
        loop {
            witness.require(&store.require(id)?)?;
            atomic_json(
                &store.directory(id)?.join("provider-response.json"),
                &response,
            )?;
            if background {
                let status = response["status"].as_str().unwrap_or("completed");
                match &last_status {
                    None => log(format!(
                        "API background response status={status}. We'll keep retrying automatically."
                    ))?,
                    Some(last) if last != status && status != "completed" => {
                        log(format!("API background response status={status}."))?
                    }
                    _ => {}
                }
                last_status = Some(status.to_owned());
            }
            let incomplete = || {
                anyhow::anyhow!(
                    "Response did not complete: {}",
                    response["error"]["message"]
                        .as_str()
                        .or(response["incomplete_details"]["reason"].as_str())
                        .or(response["status"].as_str())
                        .unwrap_or("unknown")
                )
            };
            match response["status"].as_str() {
                Some("completed") | None => break,
                Some("queued" | "in_progress") if background => {}
                Some("in_progress") if response_id.is_some() => {
                    if !announced {
                        log("Response still in_progress; polling until completion...".into())?;
                        announced = true;
                    }
                }
                _ => {
                    if !background {
                        ended_early(&response)?;
                    }
                    return Err(incomplete());
                }
            }
            if started.elapsed() >= timeout {
                return Err(providers::timeout_error(if background {
                    "Timed out waiting for API background response to finish."
                } else {
                    &overall
                }));
            }
            let poll_deadline = if background { deadline } else { grace_deadline };
            let interval = Duration::from_secs(if background { 5 } else { 2 });
            tokio::select! {_=cancel.cancelled()=>return Err(providers::cancelled_error("Alcuin API request was cancelled; scheduled response is retained.")),_=tokio::time::sleep(interval.min(poll_deadline.saturating_duration_since(tokio::time::Instant::now())))=>{}}
            if tokio::time::Instant::now() >= poll_deadline {
                if background {
                    return Err(providers::timeout_error(
                        "Timed out waiting for API background response to finish.",
                    ));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(providers::timeout_error(&overall));
                }
                // Reference grace poll: three minutes of late finalisation, then the run ended.
                ended_early(&response)?;
                return Err(incomplete());
            }
            let response_id = response_id
                .as_deref()
                .context("In-progress response has no response ID")?;
            let mut retries = 0u32;
            loop {
                witness.require(&store.require(id)?)?;
                let result = tokio::select! {biased;_=cancel.cancelled()=>return Err(providers::cancelled_error("Alcuin API request was cancelled; scheduled response is retained.")),result=tokio::time::timeout_at(poll_deadline,provider.retrieve(response_id,cancel))=>result.map_err(|_|providers::timeout_error("Timed out retrieving API response; scheduled response is retained."))?};
                match result {
                    Ok(next) => {
                        anyhow::ensure!(
                            next["id"] == response_id,
                            "Provider response identity changed; refusing result."
                        );
                        observer.response(&next)?;
                        if retries > 0 {
                            notice(format!(
                                "Reconnected to API background response (status={}). API is still working...",
                                next["status"].as_str().unwrap_or("in_progress")
                            ))?;
                        }
                        response = next;
                        break;
                    }
                    Err(error) if background && providers::retryable_retrieval(&error) => {
                        let delay = Duration::from_secs((3u64 << retries.min(3)).min(15));
                        retries = retries.saturating_add(1);
                        let described = error
                            .downcast_ref::<providers::ProviderError>()
                            .map_or_else(
                                || error.to_string(),
                                |failure| providers::describe(failure, Some(timeout)),
                            );
                        notice(format!(
                            "{described} Retrying in {}...",
                            format_elapsed(delay)
                        ))?;
                        tokio::select! {biased;_=cancel.cancelled()=>return Err(providers::cancelled_error("Alcuin API request was cancelled; scheduled response is retained.")),_=tokio::time::sleep(delay.min(poll_deadline.saturating_duration_since(tokio::time::Instant::now())))=>{}}
                        if tokio::time::Instant::now() >= poll_deadline {
                            return Err(providers::timeout_error(
                                "Timed out waiting for API background response to finish.",
                            ));
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok::<_, anyhow::Error>(response)
    };
    let background_beat = heartbeat(
        output,
        beat.filter(|_| background),
        || true,
        |elapsed| {
            format!(
                "API background run still in progress — {} elapsed.",
                format_elapsed(elapsed)
            )
        },
    );
    let response = tokio::select! {result=poll=>result?,_=background_beat=>unreachable!("heartbeat never completes")};
    if let Some(model) = response["model"].as_str() {
        anyhow::ensure!(
            model == committed.route.model,
            "Provider returned a different model; refusing result publication."
        );
    }
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "Alcuin API request was cancelled; completed evidence is retained."
    );
    if let Some(mode) = response["reasoning"]["mode"].as_str() {
        anyhow::ensure!(
            mode == "pro",
            "Provider returned a different reasoning mode; refusing result publication."
        );
    }
    let answer = providers::text_output(&response);
    anyhow::ensure!(
        !answer.trim().is_empty(),
        "Completed API response has no answer text; provider evidence is retained."
    );
    witness.require(&store.require(id)?)?;
    let receipt = capture(
        store,
        id,
        &format!("captured-answer-{}.md", Uuid::new_v4()),
        answer.as_bytes(),
        cancel,
    )?;
    let mut artifacts = vec![];
    if let Some(path) = options["writeOutputPath"].as_str() {
        witness.require(&store.require(id)?)?;
        let artifact = publish(answer.as_bytes(), Path::new(path), false, cancel)?;
        artifacts.push(json!({"kind":"transcript","path":artifact.path,"sizeBytes":artifact.size_bytes,"sha256":artifact.sha256,"validation":{"type":"generic","ok":true},"transfer":{"status":"not-needed"},"origin":{"mode":"local"}}));
    }
    witness.require(&store.require(id)?)?;
    let streamed = observer.streamed.load(Ordering::Relaxed);
    let settings = providers::model_settings(&committed.route.model, &options)?;
    let usage = providers::usage_summary(
        &response,
        crate::tokenizer::estimate_request(&committed.body)? as u64,
        settings.pricing,
    );
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if session_log {
        // Reference runOracle tail, written before the outcome so a reader that stops at
        // completion has every line.
        let status = if options["verbose"] == true {
            "[verbose] Response status: completed\n"
        } else {
            ""
        };
        let body = if streamed {
            format!("\n{status}")
        } else {
            let newline = if answer.ends_with('\n') { "" } else { "\n" };
            format!("{status}\nAnswer:\n{answer}{newline}\n")
        };
        let finish = crate::cli_execution::finish_lines_for(
            id, &options, committed, &usage, elapsed_ms, &response,
        )?;
        store.append_log(
            id,
            Some(&committed.route.model),
            &format!("{body}{}\n", finish.join("\n")),
        )?;
    } else if !streamed {
        store.append_log(id, Some(&committed.route.model), &format!("{answer}\n"))?;
    }
    if session_log && let Some(line) = saved_output_line(&artifacts) {
        // Reference writeAssistantOutput logs after the run's finish lines.
        store.append_log(id, Some(&committed.route.model), &format!("{line}\n"))?;
    }
    let completed = store.update_session_and_model(id,&committed.route.model,|current,run|{
        witness.require(current)?;
        anyhow::ensure!(current["status"]=="running"&&!cancel.is_cancelled()&&run.is_none_or(|r|!sessions::terminal(r["status"].as_str().unwrap_or(""))),"Session outcome ownership or cancellation changed.");
        let complete=json!({"status":"completed","completedAt":timestamp(),"usage":usage,"response":merge_object(&providers::response_metadata(&response),&json!({"id":response_id,"status":"completed"}))});
        let mut patch=complete.clone();patch["dispatch"]=merge_object(&current["dispatch"],&json!({"state":"completed"}));patch["recoveryRequired"]=json!(false);patch["capturedAnswer"]=json!(receipt);patch["artifacts"]=json!(artifacts);patch["elapsedMs"]=json!(elapsed_ms);
        Ok(Some((patch,Some(complete))))
    })?;
    crate::notify::completed(&completed, &answer).await;
    Ok(completed)
}

pub async fn launch_detached(
    store: &SessionStore,
    id: &str,
    executable: &Path,
    env: &Environment,
) -> Result<u32> {
    launch_detached_owned(store, id, executable, env)
        .await
        .map(|identity| identity.pid)
}
pub(crate) async fn launch_detached_owned(
    store: &SessionStore,
    id: &str,
    executable: &Path,
    env: &Environment,
) -> Result<ProcessIdentity> {
    launch_detached_with_bootstrap(store, id, executable, env, None).await
}
pub(crate) async fn launch_detached_with_bootstrap(
    store: &SessionStore,
    id: &str,
    executable: &Path,
    env: &Environment,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
) -> Result<ProcessIdentity> {
    use tokio::io::AsyncWriteExt;
    sessions::assert_id(id)?;
    let mut command = Command::new(executable);
    command
        .args(["--exec-session", id])
        .current_dir(
            store.require(id)?["cwd"]
                .as_str()
                .context("Detached session has no working directory")?,
        )
        .env_clear()
        .envs(env.iter().filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "ORACLE_BROWSER_COOKIES_JSON" | "ORACLE_BROWSER_COOKIES_FILE"
            )
        }))
        .env_remove("ORACLE_PERF_TRACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and does not access parent memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200 | 0x08000000);
    }
    command
        .env("ORACLE_HOME_DIR", store.home())
        .env("ORACLE_DETACHED_START_GATE", "1");
    // Reference buildDetachedPerfTraceEnv: the worker writes its own trace beside the caller's.
    if let Some(trace) =
        crate::perf::detached_value(env.get("ORACLE_PERF_TRACE").map(String::as_str), id)
    {
        command.env("ORACLE_PERF_TRACE", trace);
    }
    let mut child = tokio::process::Command::from(command).spawn()?;
    let pid = child.id().context("Detached worker PID is unavailable")?;
    let mut child_identity = None;
    let prepared = async {
        let identity = ProcessIdentity::read(pid)?
            .context("Detached worker started without process identity")?;
        start_owner(store, id, &identity, true)?;
        let payload = crate::browser_auth::encode_start_gate(bootstrap, id, &identity)?;
        child_identity = Some(identity);
        let mut gate = child
            .stdin
            .take()
            .context("Detached worker started without a writable start gate")?;
        tokio::time::timeout(Duration::from_secs(5), gate.write_all(&payload))
            .await
            .context("Detached worker start gate delivery expired")??;
        drop(gate);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = prepared {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let path = store.directory(id)?.join("worker-started.json");
        if let Ok(bytes) = fs::read(path)
            && let Ok(ack) = serde_json::from_slice::<Value>(&bytes)
            && ack["pid"] == pid
            && ack["sessionId"] == id
            && serde_json::from_value::<ProcessIdentity>(ack["processIdentity"].clone()).ok()
                == child_identity
        {
            tokio::spawn(async move {
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => tokio::time::sleep(Duration::from_millis(250)).await,
                    }
                }
            });
            return child_identity
                .context("Detached worker acknowledgement lost its native identity");
        }
        if Instant::now() >= deadline {
            // The gate was delivered. A missing acknowledgement cannot authorise a
            // kill or second worker; the saved reservation remains the recovery anchor.
            store.update(id,|current|Ok(Some(json!({"lifecycle":merge_object(&current["lifecycle"],&json!({"startAcknowledgement":"uncertain"})),"recoveryRequired":true}))))?;
            tokio::spawn(async move {
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => tokio::time::sleep(Duration::from_millis(250)).await,
                    }
                }
            });
            bail!(
                "Detached worker start acknowledgement was lost; session {id} is retained. Do not replay."
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
pub async fn worker(store: &SessionStore, id: &str, env: &Environment) -> Result<()> {
    use std::io::Read;
    anyhow::ensure!(
        env.get("ORACLE_DETACHED_START_GATE")
            .is_some_and(|v| v == "1"),
        "Detached worker requires an explicit start gate."
    );
    let identity = ProcessIdentity::current()?;
    let bootstrap = {
        let mut bytes = vec![];
        std::io::stdin()
            .take(crate::browser_auth::MAX_START_GATE as u64 + 1)
            .read_to_end(&mut bytes)?;
        crate::browser_auth::decode_start_gate(&bytes, id, &identity)?
    };
    let meta = store.require(id)?;
    anyhow::ensure!(
        owns(&meta, &identity) && identity.liveness() == Liveness::Alive,
        "Detached worker identity does not match its saved owner."
    );
    anyhow::ensure!(
        bootstrap.is_none() || meta["mode"] == "browser",
        "Cookie bootstrap cannot authorise an API worker"
    );
    anyhow::ensure!(
        bootstrap.is_none()
            || crate::remote_session::Connection::resolve(&meta["options"]["browserConfig"], env)
                .is_none(),
        "Explicit cookie bootstrap cannot authorise a remote browser worker"
    );
    let provider = if meta["mode"] == "browser" {
        None
    } else {
        let committed = committed_request(store, id)?;
        let key = env
            .get(&committed.route.key_source)
            .filter(|s| !s.trim().is_empty())
            .context("Saved provider credential source is unavailable")?;
        Some(HttpProvider::new(
            committed.route,
            Secret::new(key.trim().to_owned()),
            http_timeout(&meta["options"])?,
        )?)
    };
    atomic_json(
        &store.directory(id)?.join("worker-started.json"),
        &json!({"sessionId":id,"pid":identity.pid,"processIdentity":identity,"acknowledgedAt":timestamp()}),
    )?;
    let cancel = CancellationToken::new();
    let watcher_cancel = cancel.clone();
    let marker = store
        .directory(id)?
        .join(format!(".cancel-requested-{}", identity.pid));
    let watcher = tokio::spawn(async move {
        loop {
            tokio::select! {_=watcher_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{if marker.exists(){watcher_cancel.cancel();break;}}}
        }
    });
    let remote = crate::remote_session::Connection::resolve(&meta["options"]["browserConfig"], env);
    #[cfg(unix)]
    if provider.is_none() && remote.is_none() {
        crate::browser_signal::install();
    }
    let result = if let Some(provider) = &provider {
        execute_logged(store, id, provider, &identity, &cancel, None).await
    } else if let Some(connection) = &remote {
        crate::remote_session::execute(store, id, connection, &identity, &cancel).await
    } else {
        crate::browser_runner::run_native_authenticated(
            store,
            id,
            env,
            &identity,
            &cancel,
            bootstrap.as_ref(),
        )
        .await
    };
    if let Err(error) = &result
        && remote.is_none()
        && !cancel.is_cancelled()
        && let Err(log_error) = log_failure(store, id, error)
    {
        eprintln!("Unable to log the session failure: {log_error:#}");
    }
    cancel.cancel();
    watcher.await?;
    result.map(|_| ())
}
#[cfg(test)]
mod failure_tests {
    use super::failure_lines;
    use serde_json::json;

    #[test]
    fn browser_failures_add_reattach_guidance_only_when_the_run_may_be_alive() {
        let error = anyhow::anyhow!("PUBLIC failure");
        let guidance = [
            "ERROR: PUBLIC failure",
            "This run did not return cleanly, but it may still be alive. Reattach:",
            "  alcuin session public-run --render    # final markdown when complete",
            "  alcuin session public-run --live      # tail until done",
            "  alcuin session public-run --harvest   # snapshot the current answer now",
        ];
        for runtime in [
            json!({"conversationId":"PUBLIC-conversation"}),
            json!({"tabUrl":"https://chatgpt.com/c/6a1f0c2e-1b2c-4d3e-8f90-a1b2c3d4e5f6"}),
            json!({"promptSubmitted":true}),
        ] {
            let meta = json!({"id":"public-run","mode":"browser","browser":{"runtime":runtime}});
            assert_eq!(failure_lines(&meta, &error), guidance, "{runtime}");
        }
        for meta in [
            json!({"id":"public-run","mode":"browser","browser":{"runtime":{"tabUrl":"https://chatgpt.com/"}}}),
            json!({"id":"public-run","mode":"browser","options":{"browserConfig":{"copyProfileSource":"PUBLIC"}},"browser":{"runtime":{"conversationId":"PUBLIC"}}}),
            json!({"id":"public-run","mode":"api","browser":{"runtime":{"conversationId":"PUBLIC"}}}),
        ] {
            assert_eq!(
                failure_lines(&meta, &error),
                ["ERROR: PUBLIC failure"],
                "{meta}"
            );
        }
    }
}
