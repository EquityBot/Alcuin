//! Shared CLI wait policy, acknowledged worker attachment and inline execution.
use crate::{
    config::Environment,
    execution::{self, ResolvedRequest},
    process::ProcessIdentity,
    providers::{HttpProvider, Route, Secret},
    remote_session::{self, Connection},
    sessions::SessionStore,
};
use anyhow::{Result, bail};
use serde_json::Value;
use std::io::Write;
use tokio_util::sync::CancellationToken;
pub(crate) fn wait_preference(explicit: Option<bool>, metadata: &Value) -> bool {
    explicit
        .or_else(|| metadata["options"]["waitPreference"].as_bool())
        .unwrap_or_else(|| {
            let mode = metadata["mode"]
                .as_str()
                .or_else(|| metadata["options"]["mode"].as_str())
                .unwrap_or("api");
            mode == "browser"
                || !(metadata["options"]["reasoningMode"] == "pro"
                    || metadata["options"]["model"]
                        .as_str()
                        .is_some_and(|s| s.ends_with("-pro")))
        })
}
pub(crate) fn detach(request: &ResolvedRequest, wait: bool, env: &Environment) -> bool {
    if env.get("ORACLE_NO_DETACH").is_some_and(|s| s == "1") {
        return false;
    }
    if request.engine == "browser"
        && (request.options["model"]
            .as_str()
            .is_some_and(|s| s.ends_with("-pro"))
            || request.browser["thinkingTime"] == "pro")
    {
        return true;
    }
    !wait && request.engine == "api" && request.options["reasoningMode"] == "pro"
}
/// Reference runOracle preamble for a foreground API run, before dispatch.
pub(crate) fn api_header(
    request: &ResolvedRequest,
    route: &Route,
    key: &Secret,
    body: &Value,
    cwd: &std::path::Path,
) -> Result<Vec<String>> {
    let options = &request.options;
    let verbose = options["verbose"] == true;
    let mut lines = vec![];
    if verbose {
        lines.push(format!("[verbose] cwd: {}", cwd.display()));
        lines.push(match request.files.len() {
            0 => "[verbose] No files attached.".into(),
            count => format!(
                "[verbose] Attached files ({count}): {}{}",
                request
                    .files
                    .iter()
                    .take(10)
                    .map(|file| file.display_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if count > 10 {
                    format!(" (+{} more)", count - 10)
                } else {
                    String::new()
                }
            ),
        });
        let total = if request.files.is_empty() {
            0
        } else {
            crate::cli::file_report(request)?.1
        };
        lines.push(format!(
            "[verbose] Attached files use {} tokens",
            crate::cli::grouped(total)
        ));
    }
    if let Some(previous) = options["previousResponseId"].as_str() {
        lines.push(format!("Continuing from response {previous}"));
    }
    let model = &route.model;
    let effective = body["model"].as_str().unwrap_or(model);
    let suffix = if effective != model {
        format!(" (API: {effective})")
    } else {
        String::new()
    };
    let files = match request.files.len() {
        0 => "no files".to_owned(),
        count => format!("{count} files"),
    };
    let estimated = crate::tokenizer::estimate_request(body)?;
    lines.push(format!(
        "Calling {model}{suffix} \u{2014} {} tokens, {files}.",
        token_count(estimated)
    ));
    lines.push(match &route.azure_deployment_name {
        Some(deployment) => format!(
            "Provider: Azure OpenAI | endpoint: {} | deployment: {deployment} | key: {}",
            crate::providers::route_target(options["azure"]["endpoint"].as_str().unwrap_or("")),
            route.key_source
        ),
        None => format!(
            "Provider: {} | base: {} | key: {}",
            route.provider_label,
            // Reference fallback host when no base URL is configured.
            if options["baseUrl"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
                || route.provider_label != "OpenAI"
            {
                crate::providers::route_target(&route.base_url)
            } else {
                "api.openai.com".into()
            },
            route.key_source
        ),
    });
    if verbose && let Some(masked) = crate::providers::mask_api_key(key.expose()) {
        lines.push(format!(
            "Using {}={masked} for model {model}{suffix}",
            route.key_source
        ));
    }
    if effective != model {
        lines.push(format!("Resolved model: {model} \u{2192} {effective}"));
    }
    if let Some(mode) = options["reasoningMode"].as_str() {
        lines.push(format!("Reasoning mode: {mode}"));
    }
    if let Some(effort) = options["reasoningEffort"].as_str() {
        lines.push(format!("Reasoning effort: {effort}"));
    }
    if request.files.is_empty() {
        lines.push("Tip: no files attached \u{2014} Industrious works best with project context. Add files via --file path/to/code or docs.".into());
    }
    if crate::policy::trim(options["prompt"].as_str().unwrap_or(""))
        .chars()
        .count()
        < 80
    {
        lines.push("Tip: brief prompts often yield generic answers \u{2014} aim for 6\u{2013}30 sentences and attach key files.".into());
    }
    // Industrious API runs always use Pro reasoning, the reference long-running class.
    lines.push("This model can take up to 60 minutes (usually replies much faster).".into());
    let overall = execution::api_timeout(options)?;
    let transport = execution::http_timeout(options)?;
    lines.push(format!(
        "Timeouts | overall: {} | transport: {}{}",
        execution::format_elapsed(overall),
        execution::format_elapsed(transport),
        if transport < overall {
            " | note: transport can fail before overall timeout"
        } else {
            ""
        }
    ));
    lines.push("Press Ctrl+C to cancel.".into());
    lines.extend(reported_files(request)?);
    if verbose {
        lines.push(format!(
            "[verbose] Estimated tokens (request body): {}",
            crate::cli::grouped(estimated)
        ));
        lines.push("[verbose] Dispatching request to API...".into());
        lines.push(String::new());
    }
    Ok(lines)
}
/// Reference runOracle finish line (finishLine.ts) for a published foreground API answer.
pub(crate) fn finish_lines(
    store: &SessionStore,
    id: &str,
    response: &Value,
) -> Result<Vec<String>> {
    let meta = store.require(id)?;
    finish_lines_for(
        id,
        &meta["options"],
        &execution::committed_request(store, id)?,
        &meta["usage"],
        meta["elapsedMs"].as_u64().unwrap_or(0),
        response,
    )
}
/// The finish lines from the run's own values, for a worker writing them before completion.
pub(crate) fn finish_lines_for(
    id: &str,
    options: &Value,
    committed: &execution::CommittedRequest,
    usage: &Value,
    elapsed_ms: u64,
    response: &Value,
) -> Result<Vec<String>> {
    let model = &committed.route.model;
    let raw = &response["usage"];
    // Reference formatTokenValue: "*" marks a count the provider did not report.
    let reported = [
        raw["input_tokens"].is_u64(),
        raw["output_tokens"].is_u64(),
        raw["reasoning_tokens"].is_u64()
            || raw["output_tokens_details"]["reasoning_tokens"].is_u64(),
        raw["total_tokens"].is_u64(),
    ];
    let tokens: Vec<_> = [
        "inputTokens",
        "outputTokens",
        "reasoningTokens",
        "totalTokens",
    ]
    .iter()
    .zip(reported)
    .map(|(key, reported)| {
        let text = token_count(usage[key].as_u64().unwrap_or(0) as usize);
        if reported { text } else { format!("{text}*") }
    })
    .collect();
    let effort = options["reasoningEffort"]
        .as_str()
        .map(str::to_owned)
        .or(crate::providers::model_settings(model, options)?.reasoning_effort);
    let reasoning = [options["reasoningMode"].as_str(), effort.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("/");
    let label = if reasoning.is_empty() {
        model.clone()
    } else {
        format!("{model}[{reasoning}]")
    };
    let line1 = [
        Some(elapsed_compact(elapsed_ms)),
        usage["cost"].as_f64().map(|cost| {
            if cost.is_finite() {
                format!("${}", to_fixed(cost, 4))
            } else {
                "n/a".into()
            }
        }),
        (!id.to_lowercase().contains(&model.to_lowercase())).then_some(label),
        Some(format!(
            "\u{2191}{} \u{2193}{} \u{21bb}{} \u{394}{}",
            tokens[0], tokens[1], tokens[2], tokens[3]
        )),
        Some(format!("sid={id}")),
    ];
    let estimated = crate::tokenizer::estimate_request(&committed.body)?;
    let estimate = raw["input_tokens"]
        .as_u64()
        .filter(|_| options["verbose"] == true)
        .map(|actual| {
            let delta = actual as i128 - estimated as i128;
            let magnitude = crate::cli::grouped(delta.unsigned_abs() as usize);
            format!(
                "est\u{2192}actual={}\u{2192}{}{}",
                crate::cli::grouped(estimated),
                crate::cli::grouped(actual as usize),
                match delta {
                    0 => String::new(),
                    delta if delta > 0 => format!(" (+{magnitude})"),
                    _ => format!(" (-{magnitude})"),
                }
            )
        });
    let files = options["file"].as_array().map_or(0, Vec::len);
    let line2: Vec<_> = [
        estimate,
        (options["search"] == false).then(|| "search=off".to_owned()),
        (files > 0).then(|| format!("files={files}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut lines = vec![
        String::new(),
        line1
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{b7} "),
    ];
    if !line2.is_empty() {
        lines.push(line2.join(" | "));
    }
    Ok(lines)
}
/// Reference formatElapsedCompact.
pub(crate) fn elapsed_compact(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}s", to_fixed(ms as f64 / 1000.0, 1))
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, ms % 3_600_000 / 60_000)
    }
}
/// JavaScript Number.prototype.toFixed for finite non-negative values: rounds the exact binary
/// value with ties going up, where Rust's formatter rounds exact ties to even.
pub(crate) fn to_fixed(value: f64, digits: usize) -> String {
    // 1100 places hold every f64's exact decimal expansion, so formatting itself never rounds.
    let exact = format!("{:.1100}", value.max(0.0));
    let (whole, fraction) = exact.split_once('.').unwrap_or((&exact, "0"));
    let mut kept = format!("{whole}{}", &fraction[..digits]).into_bytes();
    if fraction.as_bytes()[digits] >= b'5' {
        let mut index = kept.len();
        loop {
            if index == 0 {
                kept.insert(0, b'1');
                break;
            }
            index -= 1;
            if kept[index] == b'9' {
                kept[index] = b'0';
            } else {
                kept[index] += 1;
                break;
            }
        }
    }
    let point = kept.len() - digits;
    let text = String::from_utf8(kept).unwrap_or_default();
    if digits == 0 {
        text
    } else {
        format!("{}.{}", &text[..point], &text[point..])
    }
}
/// Reference formatTokenCount: thousands as "k" with up to two decimals.
pub(crate) fn token_count(value: usize) -> String {
    if value >= 1000 {
        let text = to_fixed(value as f64 / 1000.0, 2);
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}k")
    } else {
        value.to_string()
    }
}
/// Reference formatSessionLifecycleBlock (src/cli/sessionLifecycle.ts).
pub(crate) fn lifecycle_block(meta: &Value) -> Vec<String> {
    let lifecycle = &meta["lifecycle"];
    if !lifecycle.is_object() {
        return vec![];
    }
    // JavaScript template literals print a missing field as "undefined".
    let field = |value: &Value| value.as_str().unwrap_or("undefined").to_owned();
    let count = meta["models"].as_array().map_or(
        usize::from(meta["model"].as_str().is_some_and(|m| !m.is_empty())),
        Vec::len,
    );
    let detach = match (
        lifecycle["detached"] == true,
        lifecycle["execution"] == "background",
    ) {
        (true, true) => "yes, polling",
        (true, false) => "yes",
        _ => "no",
    };
    vec![
        format!("Session: {}", field(&meta["id"])),
        format!(
            "Mode: {} {}",
            field(&lifecycle["engine"]),
            field(&lifecycle["execution"])
        ),
        format!(
            "Models: {}",
            if count > 1 {
                format!("{count} parallel")
            } else {
                count.max(1).to_string()
            }
        ),
        format!("Detach: {detach}"),
        format!("Reattach: {}", field(&lifecycle["reattachCommand"])),
    ]
}
/// Reference shouldReportFiles: the file token report when requested or over the budget.
pub(crate) fn reported_files(request: &ResolvedRequest) -> Result<Vec<String>> {
    if request.files.is_empty() {
        return Ok(vec![]);
    }
    let options = &request.options;
    let (report, total) = crate::cli::file_report(request)?;
    let settings =
        crate::providers::model_settings(options["model"].as_str().unwrap_or_default(), options)?;
    let budget = options["maxInput"].as_u64().unwrap_or(settings.input_limit) as usize;
    Ok(if options["filesReport"] == true || total > budget {
        report
    } else {
        vec![]
    })
}
fn session_cwd(store: &SessionStore, id: &str) -> Result<std::path::PathBuf> {
    Ok(store.require(id)?["cwd"].as_str().unwrap_or("").into())
}
pub(crate) fn inline_after_launch_error(metadata: &Value, wait: bool) -> bool {
    wait && metadata["status"] == "pending"
        && metadata.get("lifecycle").is_none()
        && metadata.get("dispatch").is_none()
}

#[allow(clippy::too_many_arguments)] // Execution retains the prepared route, session and explicit wait contract.
pub(crate) async fn run(
    store: &SessionStore,
    id: &str,
    request: &ResolvedRequest,
    wait: bool,
    environment: &Environment,
    route: Option<(Route, Secret)>,
    connection: Option<Connection>,
    cancel: &CancellationToken,
) -> Result<i32> {
    anyhow::ensure!(
        connection.is_none() || request.cookie_bootstrap.is_none(),
        "Explicit cookie bootstrap cannot be sent to a remote browser service"
    );
    let detached = connection.is_none() && detach(request, wait, environment);
    if detached && let Some((route, key)) = &route {
        // Reference executeSession logs runOracle's header from the worker; only this process
        // holds the resolved files and key, so it writes the same lines before the launch.
        let header = api_header(
            request,
            route,
            key,
            &execution::committed_request(store, id)?.body,
            &session_cwd(store, id)?,
        )?;
        store.append_log(id, Some(&route.model), &format!("{}\n", header.join("\n")))?;
    }
    if detached {
        match execution::launch_detached_with_bootstrap(
            store,
            id,
            &std::env::current_exe()?,
            environment,
            request.cookie_bootstrap.as_ref(),
        )
        .await
        {
            Ok(launched) => {
                if !wait {
                    for line in lifecycle_block(&store.require(id)?) {
                        println!("{line}");
                    }
                    println!(
                        "Pro runs can take up to 60 minutes (usually 10-15). Add --wait to stay attached."
                    );
                    return Ok(0);
                }
                println!("Reattach via: industrious session {id}");
                let exit = crate::session_attach::attach_launched(
                    store,
                    id,
                    &launched,
                    environment,
                    cancel,
                    &mut std::io::stdout(),
                )
                .await?;
                if exit == 1 {
                    let failed = store.require(id)?;
                    bail!(
                        "Session {id} failed: {}",
                        failed["errorMessage"]
                            .as_str()
                            .unwrap_or("inspect its saved log for details")
                    );
                }
                return Ok(exit);
            }
            Err(error) => {
                // launch_detached records native lifecycle authority before delivering its start gate.
                // start_owner rechecks pending/no-dispatch under the session transaction below.
                if !inline_after_launch_error(&store.require(id)?, wait) {
                    return Err(error.context("Unable to start in background; use --wait to run inline. Inspect the saved session before retrying."));
                }
                eprintln!("Unable to detach session runner ({error:#}). Running inline...");
            }
        }
    }
    if !wait {
        bail!("Unable to start in background; use --wait to run inline.");
    }
    let identity = ProcessIdentity::current()?;
    let started = execution::start_owner(store, id, &identity, false)?;
    // Reference runInteractiveSession prints and logs the lifecycle block before the run. Browser
    // and remote runs print their session log after the run, so they show it from there.
    let block = lifecycle_block(&started);
    if !block.is_empty()
        && let Some(model) = started["model"].as_str()
    {
        if request.engine == "api" {
            for line in &block {
                println!("{line}");
            }
        }
        store.append_log(id, Some(model), &format!("{}\n", block.join("\n")))?;
    }
    let stream = crate::cli::CliStream::default();
    let result = if let Some(connection) = &connection {
        remote_session::execute(store, id, connection, &identity, cancel).await
    } else if let Some((route, key)) = route {
        // Reference combinedLog: the header goes to stdout and the session log alike.
        let header = api_header(
            request,
            &route,
            &key,
            &execution::committed_request(store, id)?.body,
            &session_cwd(store, id)?,
        )?;
        for line in &header {
            println!("{line}");
        }
        store.append_log(id, Some(&route.model), &format!("{}\n", header.join("\n")))?;
        let provider = HttpProvider::new(route, key, execution::http_timeout(&request.options)?)?;
        execution::execute_logged(store, id, &provider, &identity, cancel, Some(&stream)).await
    } else {
        // This inline run owns Chrome; a detached run installs these in its worker.
        crate::browser_signal::install();
        crate::browser_runner::run_native_authenticated(
            store,
            id,
            environment,
            &identity,
            cancel,
            request.cookie_bootstrap.as_ref(),
        )
        .await
    };
    let mut logged = None;
    if let Err(error) = &result
        && connection.is_none()
        && !cancel.is_cancelled()
    {
        match execution::log_failure(store, id, error) {
            Ok(lines) => logged = Some(lines),
            Err(log_error) => eprintln!("Unable to log the session failure: {log_error:#}"),
        }
    }
    if request.engine == "api" {
        // The answer tail and finish line follow publication; failures keep the plain log.
        let response = match &result {
            Ok(_) => Some(serde_json::from_slice::<Value>(&std::fs::read(
                store.directory(id)?.join("provider-response.json"),
            )?)?),
            Err(_) => None,
        };
        let answer = response.as_ref().map(crate::providers::text_output);
        stream.finish(answer.as_deref(), request.options["verbose"] == true)?;
        for line in logged.iter().flatten() {
            println!("{line}");
        }
        if let Some(response) = &response {
            for line in finish_lines(store, id, response)? {
                println!("{line}");
            }
            // The worker logged the same line after the finish lines.
            if let Some(line) = store.require(id)?["artifacts"]
                .as_array()
                .and_then(|artifacts| execution::saved_output_line(artifacts))
            {
                println!("{line}");
            }
        }
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(store.log(id)?.as_bytes())?;
        stdout.flush()?;
        let current = store.require(id)?;
        if current["status"] == "completed"
            && (current["cleanupWarning"].is_string()
                || (current["recoveryRequired"] == true
                    && current["browser"]["runtime"]["targetDispositionPending"] == true))
        {
            eprintln!("Answer saved. Owned browser tab cleanup is pending.");
        }
    }
    if cancel.is_cancelled() {
        return Ok(
            if matches!(
                store.require(id)?["status"].as_str(),
                Some("completed" | "partial")
            ) {
                0
            } else {
                130
            },
        );
    }
    match result {
        Err(error) if logged.is_some() => Err(execution::LoggedFailure(error).into()),
        result => result.map(|_| 0),
    }
}
#[cfg(test)]
mod header_tests {
    use super::api_header;
    use crate::{
        config::Environment,
        execution,
        providers::{self, ApiTransport, Route},
    };
    use serde_json::json;
    fn header(input: serde_json::Value, route: &Route) -> Vec<String> {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("PUBLIC-source.rs"), "fn public() {}\n").unwrap();
        let mut input = input;
        input["engine"] = json!("api");
        input["files"] = json!(["PUBLIC-source.rs"]);
        let request =
            execution::resolve(&input, &json!({}), &Environment::new(), home.path()).unwrap();
        let mut body = providers::build_request(
            &route.model,
            &request.options,
            &request.user_prompt,
            crate::prompts::DEFAULT_SYSTEM,
        )
        .unwrap();
        if let Some(deployment) = &route.azure_deployment_name {
            body["model"] = json!(deployment);
        }
        api_header(
            &request,
            route,
            &providers::Secret::new("PUBLIC-synthetic-key".into()),
            &body,
            home.path(),
        )
        .unwrap()
    }
    fn route(azure: Option<&str>) -> Route {
        Route {
            model: "gpt-6-astra".into(),
            provider_label: if azure.is_some() {
                "Azure OpenAI"
            } else {
                "OpenAI"
            }
            .into(),
            base_url: "https://api.openai.com/v1".into(),
            key_source: "OPENAI_API_KEY".into(),
            azure_deployment_name: azure.map(str::to_owned),
            transport: ApiTransport::Responses,
        }
    }
    #[test]
    fn foreground_header_follows_the_reference_preamble() {
        let lines = header(
            json!({"prompt":"PUBLIC short prompt","filesReport":true}),
            &route(None),
        );
        assert!(
            lines[0].starts_with("Calling gpt-6-astra \u{2014} "),
            "{lines:?}"
        );
        assert!(lines[0].ends_with(" tokens, 1 files."), "{lines:?}");
        assert_eq!(
            lines[1..8],
            [
                "Provider: OpenAI | base: api.openai.com | key: OPENAI_API_KEY",
                "Reasoning mode: pro",
                "Tip: brief prompts often yield generic answers \u{2014} aim for 6\u{2013}30 sentences and attach key files.",
                "This model can take up to 60 minutes (usually replies much faster).",
                "Timeouts | overall: 1h 0m | transport: 1h 0m",
                "Press Ctrl+C to cancel.",
                "File Token Usage",
            ]
        );
        assert!(lines[8].ends_with("  PUBLIC-source.rs"), "{lines:?}");
        assert!(lines[9].starts_with("Total: "), "{lines:?}");
        assert_eq!(lines.len(), 10);
    }
    #[test]
    fn azure_header_names_the_deployment_and_short_transport_timeouts() {
        let lines = header(
            json!({"prompt":"PUBLIC Azure prompt","httpTimeoutMs":1_200_000,"previousResponseId":"resp_PUBLIC","azure":{"endpoint":"https://public.openai.azure.com/"}}),
            &route(Some("PUBLIC-deployment")),
        );
        assert_eq!(lines[0], "Continuing from response resp_PUBLIC");
        assert!(lines[1].starts_with("Calling gpt-6-astra (API: PUBLIC-deployment) \u{2014} "));
        assert_eq!(
            lines[2],
            "Provider: Azure OpenAI | endpoint: public.openai.azure.com | deployment: PUBLIC-deployment | key: OPENAI_API_KEY"
        );
        assert_eq!(
            lines[3],
            "Resolved model: gpt-6-astra \u{2192} PUBLIC-deployment"
        );
        assert!(lines.contains(
            &"Timeouts | overall: 1h 0m | transport: 20m 0s | note: transport can fail before overall timeout".into()
        ));
        assert!(!lines.contains(&"File Token Usage".into()));
    }
    #[test]
    fn verbose_header_adds_the_masked_key_estimate_and_dispatch_lines() {
        let lines = header(
            json!({"prompt":"PUBLIC verbose prompt","verbose":true}),
            &route(None),
        );
        // Reference runOracle verbose preamble, before the header.
        assert!(lines[0].starts_with("[verbose] cwd: "), "{lines:?}");
        assert_eq!(lines[1], "[verbose] Attached files (1): PUBLIC-source.rs");
        assert!(
            lines[2].starts_with("[verbose] Attached files use ") && lines[2].ends_with(" tokens"),
            "{lines:?}"
        );
        assert_eq!(
            lines[5],
            "Using OPENAI_API_KEY=PUBL****-key for model gpt-6-astra"
        );
        let tail = &lines[lines.len() - 3..];
        assert!(
            tail[0].starts_with("[verbose] Estimated tokens (request body): "),
            "{lines:?}"
        );
        assert_eq!(tail[1..], ["[verbose] Dispatching request to API...", ""]);
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("PUBLIC-synthetic-key"))
        );
    }
    #[test]
    fn finish_formatting_follows_javascript_to_fixed() {
        use super::{elapsed_compact, to_fixed, token_count};
        // Exact binary ties round up as in JavaScript; 0.15 is below its tie.
        assert_eq!(to_fixed(1.25, 1), "1.3");
        assert_eq!(to_fixed(0.15, 1), "0.1");
        assert_eq!(to_fixed(9.96, 1), "10.0");
        assert_eq!(to_fixed(0.015_14, 4), "0.0151");
        assert_eq!(token_count(1125), "1.13k");
        assert_eq!(token_count(11_500), "11.5k");
        assert_eq!(token_count(999), "999");
        assert_eq!(elapsed_compact(1250), "1.3s");
        assert_eq!(elapsed_compact(61_999), "1m01s");
        assert_eq!(elapsed_compact(3_725_000), "1h02m");
    }
}
#[cfg(test)]
mod lifecycle_tests {
    use super::lifecycle_block;
    use serde_json::json;
    #[test]
    fn lifecycle_block_follows_the_reference_formatter() {
        let meta = json!({"id":"public-run","model":"gpt-6-astra","lifecycle":{"engine":"browser","execution":"foreground","detached":true,"reattachCommand":"industrious session public-run"}});
        assert_eq!(
            lifecycle_block(&meta),
            [
                "Session: public-run",
                "Mode: browser foreground",
                "Models: 1",
                "Detach: yes",
                "Reattach: industrious session public-run"
            ]
        );
        let parallel = json!({"id":"public-run","models":[{"model":"a"},{"model":"b"}],"lifecycle":{"engine":"api"}});
        let block = lifecycle_block(&parallel);
        assert_eq!(block[1], "Mode: api undefined");
        assert_eq!(block[2], "Models: 2 parallel");
        assert_eq!(block[3], "Detach: no");
        assert_eq!(block[4], "Reattach: undefined");
        assert_eq!(
            lifecycle_block(&json!({"id":"public-run","models":[],"lifecycle":{}}))[2],
            "Models: 1"
        );
        assert!(lifecycle_block(&json!({"id":"public-run","model":"gpt-6-astra"})).is_empty());
    }
}
