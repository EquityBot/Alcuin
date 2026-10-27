//! Restart saved intent through fresh admission and current dispatch policy.
use crate::{
    config::{self, Environment},
    execution::{self, ResolvedRequest},
    policy, providers,
    remote_session::{self, Connection},
    sessions::{SessionStore, merge_object},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const SAVED_OPTIONS: &[&str] = &[
    "prompt",
    "model",
    "models",
    "previousResponseId",
    "browserResumeConversationUrl",
    "effectiveModelId",
    "modelOverrides",
    "reasoningEffort",
    "reasoningMode",
    "file",
    "maxFileSizeBytes",
    "slug",
    "filesReport",
    "maxInput",
    "maxOutput",
    "system",
    "silent",
    "search",
    "provider",
    "baseUrl",
    "azure",
    "timeoutSeconds",
    "httpTimeoutMs",
    "zombieTimeoutMs",
    "zombieUseLastActivity",
    "partialMode",
    "verbose",
    "heartbeatIntervalMs",
    "browserAttachments",
    "browserInlineFiles",
    "browserBundleFiles",
    "browserBundleFormat",
    "generateImage",
    "youtube",
    "editImage",
    "aspectRatio",
    "geminiShowThoughts",
    "geminiAllowModelFallback",
    "outputPath",
    "browserFollowUps",
    "background",
    "renderPlain",
    "writeOutputPath",
    "writeArtifacts",
    "followupSessionId",
    "followupModel",
];

fn saved_request(
    metadata: &Value,
    fallback_cwd: &Path,
    env: &Environment,
) -> Result<(ResolvedRequest, PathBuf)> {
    let id = metadata["id"]
        .as_str()
        .context("Saved session has no identity")?;
    let stored = &metadata["options"];
    ensure!(
        stored["prompt"].as_str().is_some_and(|s| !s.is_empty()),
        "Session {id} has no stored prompt; cannot restart."
    );
    let mode = metadata["mode"]
        .as_str()
        .or_else(|| stored["mode"].as_str())
        .unwrap_or("api");
    let engine = if mode == "browser" { "browser" } else { "api" };
    let browser = metadata["browser"]
        .get("config")
        .filter(|v| v.is_object())
        .or_else(|| stored.get("browserConfig").filter(|v| v.is_object()));
    ensure!(
        engine != "browser" || browser.is_some(),
        "Session {id} is missing browser config; cannot restart."
    );
    let cwd = metadata["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_cwd.to_owned());
    let mut saved = json!({});
    for key in SAVED_OPTIONS {
        if let Some(value) = stored.get(key) {
            saved[*key] = value.clone();
        }
    }
    if saved.get("model").is_none() {
        saved["model"] = json!(policy::DEFAULT_API_MODEL);
    }
    let mut input = saved.clone();
    input["engine"] = json!(engine);
    input["files"] = stored.get("file").cloned().unwrap_or_else(|| json!([]));
    if engine == "browser"
        && input["models"]
            .as_array()
            .is_some_and(|models| models.len() <= 1)
    {
        input
            .as_object_mut()
            .context("Invalid saved options")?
            .remove("models");
    }
    let mut resolution_config = json!({});
    if let Some(overrides) = stored.get("modelOverrides") {
        resolution_config["modelOverrides"] = overrides.clone();
    }
    if let Some(browser) = browser {
        resolution_config["browser"] = browser.clone();
    }
    let mut resolution_env = env.clone();
    // The saved Azure options are restarted as stored, without current fallbacks.
    for key in [
        "ORACLE_MAX_FILE_SIZE_BYTES",
        "AZURE_OPENAI_ENDPOINT",
        "AZURE_OPENAI_DEPLOYMENT",
        "AZURE_OPENAI_API_VERSION",
    ] {
        resolution_env.remove(key);
    }
    let mut request = execution::resolve(&input, &resolution_config, &resolution_env, &cwd)?;
    // Reference restart keeps the saved notification choice.
    match metadata.get("notifications") {
        Some(saved) => request.options["notifications"] = saved.clone(),
        None => {
            if let Some(options) = request.options.as_object_mut() {
                options.remove("notifications");
            }
        }
    }
    let model = request.options["model"].clone();
    let models = request.options.get("models").cloned();
    request.options = merge_object(&request.options, &saved);
    request.options["model"] = model;
    if let Some(models) = models {
        request.options["models"] = models;
    } else {
        request
            .options
            .as_object_mut()
            .context("Invalid restart options")?
            .remove("models");
    }
    request.options["mode"] = json!(engine);
    if engine == "api" {
        request.options["reasoningMode"] = json!(policy::require_api_pro(
            request.options["reasoningMode"].as_str()
        )?);
        providers::model_settings(
            request.options["model"]
                .as_str()
                .context("Restart model missing")?,
            &request.options,
        )?;
    } else {
        let mut browser = browser
            .context("Restart browser configuration missing")?
            .clone();
        if let Some(config) = browser.as_object_mut() {
            config.remove("remoteHost");
            config.remove("remoteToken");
        }
        request.replace_browser_config(browser, &input, &resolution_env, &cwd)?;
        if request.options["search"] == false {
            println!("Note: search is not available in browser engine; ignoring search=false.");
            request
                .options
                .as_object_mut()
                .context("Invalid restart options")?
                .remove("search");
        }
    }
    Ok((request, cwd))
}

pub(crate) struct Options<'a> {
    pub source: &'a str,
    pub wait: Option<bool>,
    pub remote_host: Option<&'a str>,
    pub remote_token: Option<&'a str>,
}
pub(crate) async fn run(
    store: &SessionStore,
    options: Options<'_>,
    environment: &Environment,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<i32> {
    let metadata = store
        .read(options.source)?
        .with_context(|| format!("No session found with ID {}", options.source))?;
    let (mut request, saved_cwd) = saved_request(&metadata, cwd, environment)?;
    let loaded = config::load(
        store.home(),
        cwd,
        environment
            .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(Path::new),
        true,
    );
    for warning in loaded.warnings {
        eprintln!("{warning}");
    }
    crate::cli::refuse_excluded_config(&loaded.config)?;
    let mut routing = loaded.config["browser"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));
    for (value, key) in [
        (options.remote_host, "remoteHost"),
        (options.remote_token, "remoteToken"),
    ] {
        if let Some(value) = value.map(policy::trim).filter(|s| !s.is_empty()) {
            routing[key] = json!(value);
        }
    }
    let connection = Connection::resolve(&routing, environment);
    ensure!(
        connection.is_none() || request.cookie_bootstrap.is_none(),
        "Explicit cookie bootstrap cannot be sent to a remote browser service"
    );
    ensure!(
        connection.is_none() || request.engine == "browser",
        "--remote-host requires a browser session."
    );
    let mut wait = crate::cli_execution::wait_preference(options.wait, &metadata);
    if let Some(connection) = &connection {
        println!("Remote browser host detected: {}", connection.host);
        if !wait {
            println!("Remote browser runs require --wait; ignoring --no-wait.");
            wait = true;
        }
        println!(
            "Routing browser automation to remote host {}",
            connection.host
        );
    }
    request.options["waitPreference"] = json!(wait);
    ensure!(!cancel.is_cancelled(), "Restart cancelled before admission");
    let route = if request.engine == "api" {
        let (route, key) = providers::resolve_route(
            request.options["model"]
                .as_str()
                .context("Restart model missing")?,
            &request.options,
            environment,
        )?;
        let key = key.with_context(|| providers::missing_key_message(&route))?;
        Some((route, key))
    } else {
        None
    };
    let session = if let Some(connection) = &connection {
        remote_session::prepare_restart(store, &request, &saved_cwd, connection, options.source)?
    } else if let Some((route, _)) = &route {
        execution::prepare_restart(store, &request, &saved_cwd, route.clone(), options.source)?
    } else {
        crate::browser_runner::prepare_restart(store, &request, &saved_cwd, options.source)?
    };
    let id = session["id"]
        .as_str()
        .context("Restart session identity missing")?;
    crate::cli_execution::run(
        store,
        id,
        &request,
        wait,
        environment,
        route,
        connection,
        cancel,
    )
    .await
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
