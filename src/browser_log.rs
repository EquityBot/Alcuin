//! Reference browser session log lines (src/browser/sessionRunner.ts): the run header before
//! dispatch and the evidence, warning, answer and finish lines after capture.
use crate::browser_runner::BrowserRequest;
use crate::cli_execution::{elapsed_compact, to_fixed, token_count};
use serde_json::{Value, json};

const LARGE_PRO_FAST_INPUT_TOKENS: u64 = 25_000;
const LARGE_PRO_FAST_ELAPSED_MS: u64 = 120_000;

fn clean(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(crate::policy::trim)
        .filter(|text| !text.is_empty())
}
/// JavaScript template interpolation: a missing field prints as "undefined".
fn field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}
fn grouped(value: u64) -> String {
    crate::cli::grouped(usize::try_from(value).unwrap_or(usize::MAX))
}
fn input_tokens(request: &BrowserRequest) -> Option<u64> {
    request.extra.get("estimatedInputTokens")?.as_u64()
}
/// Reference automation logger words: a `[browser] ` line containing one prints without --verbose.
const ALWAYS_PRINTED: [&str; 13] = [
    "archive",
    "fallback",
    "follow-up",
    "retry",
    "thinking",
    "research",
    "waiting for chatgpt",
    "remote debugging approval",
    "browser slot",
    "browser control",
    "browser guidance",
    "model selection",
    "model picker",
];
/// Reference automation logger filter (src/browser/sessionRunner.ts).
pub(crate) fn prints(meta: &Value, line: &str) -> bool {
    meta["options"]["verbose"] == true
        || line.starts_with("[browser] ") && {
            let lower = line.to_lowercase();
            ALWAYS_PRINTED.iter().any(|word| lower.contains(word))
        }
}
/// Reference describeBrowserControlPlan and formatBrowserControlPlan.
fn control_plan(config: &Value) -> Vec<String> {
    let tab = clean(&config["browserTabRef"]);
    let mut guidance = vec![];
    let (summary, focus) = if config["attachRunning"] == true {
        guidance.push(match tab {
            Some(tab) => format!("Alcuin reuses the matching ChatGPT tab ({tab}) and leaves the existing browser process alone."),
            None => "Alcuin opens a dedicated tab and leaves the existing browser process alone.".into(),
        });
        if config["keepBrowser"] == true {
            guidance.push("The browser stays open because Alcuin did not launch it.".into());
        }
        (
            if tab.is_some() {
                "attach to an already-running local Chrome tab"
            } else {
                "attach to an already-running local Chrome session"
            },
            true,
        )
    } else if config["remoteChrome"].is_object() {
        guidance.push(match tab {
            Some(tab) => format!("Alcuin reuses the matching ChatGPT tab ({tab}) in the configured remote Chrome session."),
            None => "Alcuin opens a dedicated tab in the configured remote Chrome session.".into(),
        });
        guidance
            .push("Local Chrome launch, cookie copy, and window hiding flags are skipped.".into());
        (
            if tab.is_some() {
                "reuse an existing remote Chrome tab"
            } else {
                "reuse an existing remote Chrome session"
            },
            false,
        )
    } else if config["headless"] == true {
        guidance.push(
            "Headless mode avoids visible UI but may be blocked by ChatGPT or Cloudflare.".into(),
        );
        ("launch headless Chrome", false)
    } else if config["hideWindow"] == true {
        guidance.push(
            if cfg!(target_os = "macos") {
                "On macOS, Alcuin launches Chrome off-screen while keeping the page rendered."
            } else {
                "--browser-hide-window has no effect on Windows or Linux; Chrome stays visible."
            }
            .into(),
        );
        guidance.push(
            "For the calmest shared-desktop flow, prefer --browser-attach-running or --remote-chrome."
                .into(),
        );
        (
            if cfg!(target_os = "macos") {
                "launch Chrome in hidden-window mode"
            } else {
                "launch visible Chrome"
            },
            true,
        )
    } else {
        guidance.push(if config["manualLogin"] == true {
            "Manual-login mode may show the persistent Alcuin Chrome profile for sign-in or automation.".into()
        } else {
            "A visible automation Chrome window may take focus while Alcuin controls ChatGPT.".into()
        });
        guidance.push(
            "Use --browser-attach-running or --remote-chrome to reduce desktop disruption.".into(),
        );
        if config["keepBrowser"] == true {
            guidance.push(
                "Chrome will remain open after the run because --browser-keep-browser is enabled."
                    .into(),
            );
        }
        ("launch visible Chrome", true)
    };
    let risk = if focus {
        "may focus/control the browser UI"
    } else {
        "does not use a visible local browser window"
    };
    let mut lines = vec![format!("[browser] Browser control: {summary}; {risk}.")];
    lines.extend(
        guidance
            .into_iter()
            .map(|entry| format!("[browser] Browser guidance: {entry}")),
    );
    lines
}
/// Reference listIgnoredRemoteChromeFlags note.
fn remote_chrome_note(config: &Value) -> Option<String> {
    if !config["remoteChrome"].is_object() {
        return None;
    }
    let flags = [
        (config["headless"] == true, "--browser-headless"),
        (config["hideWindow"] == true, "--browser-hide-window"),
        (config["keepBrowser"] == true, "--browser-keep-browser"),
        (
            config["attachRunning"] != true && clean(&config["chromePath"]).is_some(),
            "--browser-chrome-path",
        ),
    ]
    .into_iter()
    .filter_map(|(set, flag)| set.then_some(flag))
    .collect::<Vec<_>>();
    (!flags.is_empty()).then(|| {
        format!(
            "Note: --remote-chrome ignores local Chrome flags ({}).",
            flags.join(", ")
        )
    })
}
/// Reference verbose runner lines before the launch header.
fn verbose_lines(meta: &Value, request: &BrowserRequest) -> Vec<String> {
    let mut config = meta["browser"]["config"].clone();
    if let Some(config) = config.as_object_mut() {
        // Reference redactBrowserConfigForDebugLog; a saved remote token is never printed.
        if let Some(count) = config
            .get("inlineCookies")
            .and_then(Value::as_array)
            .map(Vec::len)
        {
            config.insert(
                "inlineCookies".into(),
                json!(format!("[redacted:{count} cookies]")),
            );
            config.insert("inlineCookieCount".into(), json!(count));
        }
        config.remove("remoteToken");
    }
    let mut lines = vec![
        format!("[verbose] Browser config: {config}"),
        format!(
            "[verbose] Browser prompt length: {} chars",
            request.prompt.encode_utf16().count()
        ),
    ];
    let attachments = request
        .extra
        .get("attachmentDisplayPaths")
        .and_then(Value::as_array)
        .map(|paths| paths.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if !attachments.is_empty() {
        lines.push(format!(
            "[verbose] Browser attachments: {}",
            attachments.join(", ")
        ));
        let bundled = request.extra.get("bundled").unwrap_or(&Value::Null);
        if let (Some(count), Some(path)) = (
            bundled["originalCount"].as_u64(),
            bundled["bundlePath"].as_str(),
        ) {
            lines.push(format!("[browser] Bundled {count} files into {path}."));
        }
    } else if meta["options"]["file"]
        .as_array()
        .is_some_and(|files| !files.is_empty())
        && request.extra.get("attachmentMode") == Some(&json!("inline"))
    {
        lines.push("[verbose] Browser will paste file contents inline (no uploads).".into());
    }
    lines
}

/// Reference header: the bundle note, the launch target with its token estimate and the duration hint.
pub(crate) fn header(meta: &Value, request: &BrowserRequest) -> Option<String> {
    let tokens = input_tokens(request)?;
    let config = &meta["browser"]["config"];
    let verbose = meta["options"]["verbose"] == true;
    let mut lines = if verbose {
        verbose_lines(meta, request)
    } else {
        vec![]
    };
    if let Some(count) = request
        .extra
        .get("bundled")
        .and_then(|bundled| bundled["originalCount"].as_u64())
    {
        lines.push(format!(
            "Packed {count} files into 1 bundle (contents counted in token estimate)."
        ));
    }
    let requested = clean(&meta["model"]).unwrap_or("n/a");
    let target = match config["modelStrategy"].as_str() {
        Some(strategy @ ("current" | "ignore")) => {
            format!("picker={strategy}; requested={requested}")
        }
        _ => match clean(&config["desiredModel"]) {
            Some(target) => format!("target={target}; requested={requested}"),
            None => requested.into(),
        },
    };
    lines.push(format!(
        "Launching browser mode ({target}) with ~{} tokens.",
        grouped(tokens)
    ));
    lines.push("This run can take up to an hour (usually ~10 minutes).".into());
    if verbose {
        lines.push("Chrome automation does not stream output; this may take a minute...".into());
    }
    // runBrowserMode then logs the control plan and the remote-Chrome note.
    lines.extend(
        control_plan(config)
            .into_iter()
            .chain(remote_chrome_note(config))
            .filter(|line| prints(meta, line)),
    );
    Some(lines.join("\n") + "\n")
}

/// Reference estimateTokenCount.
fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let words = text
        .split(crate::policy::javascript_whitespace)
        .filter(|word| !word.is_empty())
        .count() as f64;
    let estimate = (words * 0.75).max(text.encode_utf16().count() as f64 / 4.0);
    (estimate + 0.5).floor().max(1.0) as u64
}

fn elapsed_ms(meta: &Value) -> Option<u64> {
    let started = chrono::DateTime::parse_from_rfc3339(meta["startedAt"].as_str()?).ok()?;
    u64::try_from((chrono::Utc::now() - started.to_utc()).num_milliseconds()).ok()
}

/// Reference buildUnavailableModelSelectionEvidence.
fn model_evidence(meta: &Value) -> Value {
    let browser = &meta["browser"];
    for evidence in [
        &browser["modelSelection"],
        &browser["runtime"]["modelSelection"],
    ] {
        if evidence.is_object() {
            return evidence.clone();
        }
    }
    match browser["config"]["desiredModel"]
        .as_str()
        .filter(|model| !model.is_empty())
    {
        Some(model) => {
            json!({"requestedModel":model,"resolvedLabel":null,"strategy":browser["config"]["modelStrategy"],
            "status":"unavailable","verified":false,"source":"config","capturedAt":crate::sessions::timestamp()})
        }
        None => Value::Null,
    }
}
fn thinking_evidence(meta: &Value) -> &Value {
    let browser = &meta["browser"];
    if browser["thinkingSelection"].is_object() {
        &browser["thinkingSelection"]
    } else {
        &browser["runtime"]["thinkingSelection"]
    }
}

/// Reference buildBrowserRunWarnings.
fn run_warnings(meta: &Value, request: &BrowserRequest) -> Vec<Value> {
    let evidence = model_evidence(meta);
    let pro = [
        &meta["model"],
        &meta["browser"]["config"]["desiredModel"],
        &evidence["requestedModel"],
        &evidence["resolvedLabel"],
    ]
    .iter()
    .filter_map(|value| value.as_str())
    .any(|value| {
        value
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word.eq_ignore_ascii_case("pro"))
    });
    let (Some(tokens), Some(elapsed)) = (input_tokens(request), elapsed_ms(meta)) else {
        return vec![];
    };
    if !pro || tokens < LARGE_PRO_FAST_INPUT_TOKENS || elapsed >= LARGE_PRO_FAST_ELAPSED_MS {
        return vec![];
    }
    let requested = if evidence["requestedModel"].is_null() {
        meta["browser"]["config"]["desiredModel"].clone()
    } else {
        evidence["requestedModel"].clone()
    };
    vec![
        json!({"code":"browser-pro-fast-large-run","severity":"warning",
        "message":format!("Large browser Pro run completed quickly ({}s for ~{} input tokens); verify the stored model selection evidence before claiming Pro Extended output.",
            to_fixed(elapsed as f64 / 1000.0, 0), grouped(tokens)),
        "details":{"inputTokens":tokens,"elapsedMs":elapsed,"requestedModel":requested,
            "resolvedLabel":if evidence["resolvedLabel"].is_null(){Value::Null}else{evidence["resolvedLabel"].clone()}}}),
    ]
}

/// Warnings a completion adds (reference prepareCapturedResult): verified artifacts whose
/// staging cleanup failed, then buildBrowserRunWarnings.
pub(crate) fn completion_warnings(meta: &Value, request: &BrowserRequest) -> Vec<Value> {
    let mut warnings = meta["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|artifact| artifact["cleanupWarning"].is_string())
        .filter_map(|artifact| {
            let kind = artifact["kind"]
                .as_str()
                .filter(|kind| matches!(*kind, "image" | "file"))?;
            let path = artifact["path"].as_str()?;
            Some(crate::remote_protocol::cleanup_warning(
                kind,
                "local",
                std::path::Path::new(path),
            ))
        })
        .collect::<Vec<_>>();
    warnings.extend(run_warnings(meta, request));
    warnings
}
/// Reference archive decision and its log line. Rust sends only in verified
/// Temporary Chat, so a run with a conversation is never archived.
pub(crate) fn archive(meta: &Value) -> (Value, String) {
    let mode = meta["browser"]["config"]["archiveConversations"]
        .as_str()
        .filter(|mode| matches!(*mode, "always" | "never"))
        .unwrap_or("auto");
    let url = clean(&meta["browser"]["runtime"]["tabUrl"]);
    let reason = match (mode, url) {
        ("never", _) => "disabled",
        (_, None) => "missing-conversation-url",
        _ => "temporary-chat",
    };
    let mut record = json!({"mode":mode,"attempted":false,"archived":false,"reason":reason});
    if let Some(url) = url {
        record["conversationUrl"] = json!(url);
    }
    (
        record,
        format!("[browser] ChatGPT archive skipped ({reason}).\n"),
    )
}
/// The session's browser record with completion warnings appended once.
pub(crate) fn with_warnings(browser: &Value, added: &[Value]) -> Value {
    let mut warnings = browser["warnings"].as_array().cloned().unwrap_or_default();
    for warning in added {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
    crate::sessions::merge_object(browser, &json!({"warnings":warnings}))
}

/// Reference completion block: evidence lines, warnings, the labelled answer and the finish lines.
pub(crate) fn completion(
    meta: &Value,
    request: &BrowserRequest,
    warnings: &[Value],
    text: &str,
) -> String {
    let mut lines = vec![];
    let model = model_evidence(meta);
    if model.is_object() {
        lines.push(format!(
            "[browser] Model selection evidence: requestedKey={}; target={}; resolvedLabel={}; status={}; strategy={}; verified={}; source={}; capturedAt={}",
            clean(&meta["model"]).unwrap_or("(none)"),
            clean(&model["requestedModel"]).unwrap_or("(none)"),
            clean(&model["resolvedLabel"]).unwrap_or("(unavailable)"),
            field(&model["status"]),
            model["strategy"].as_str().unwrap_or("(default)"),
            if model["verified"] == true { "yes" } else { "no" },
            field(&model["source"]),
            field(&model["capturedAt"]),
        ));
    }
    let thinking = thinking_evidence(meta);
    if thinking.is_object() {
        lines.push(format!(
            "[browser] Thinking effort evidence: requestedLevel={}; status={}; resolvedLabel={}; verified={}; failClosed={}; targetModelKind={}; observedModelKind={}; source={}; capturedAt={}",
            field(&thinking["requestedLevel"]),
            field(&thinking["status"]),
            clean(&thinking["resolvedLabel"]).unwrap_or("(none)"),
            if thinking["verified"] == true { "yes" } else { "no" },
            if thinking["strictFailClosed"] == true { "yes" } else { "no" },
            thinking["targetModelKind"].as_str().unwrap_or("(none)"),
            thinking["observedModelKind"].as_str().unwrap_or("(none)"),
            field(&thinking["source"]),
            field(&thinking["capturedAt"]),
        ));
    }
    for warning in warnings {
        lines.push(format!("[browser] {}", field(&warning["message"])));
    }
    if meta["options"]["silent"] != true {
        lines.push(crate::browser_output::answer_heading(meta));
        lines.push(if text.is_empty() {
            "(no text output)".into()
        } else {
            text.strip_suffix('\n').unwrap_or(text).to_owned()
        });
        lines.push(String::new());
    }
    let input = input_tokens(request).unwrap_or(0);
    let output = estimate_tokens(text);
    let verified = (model["verified"] == true)
        .then(|| clean(&model["resolvedLabel"]))
        .flatten();
    let name = verified.or_else(|| clean(&meta["model"])).unwrap_or("n/a");
    let tokens = [input, output, 0, input + output].map(|value| token_count(value as usize));
    lines.push(
        [
            elapsed_ms(meta).map_or_else(|| "unknown".into(), elapsed_compact),
            format!("{name}[browser]"),
            format!(
                "\u{2191}{} \u{2193}{} \u{21bb}{} \u{394}{}",
                tokens[0], tokens[1], tokens[2], tokens[3]
            ),
        ]
        .join(" \u{b7} "),
    );
    let files = meta["options"]["file"].as_array().map_or(0, Vec::len);
    if files > 0 {
        lines.push(format!("files={files}"));
    }
    lines.join("\n") + "\n"
}

/// Reference formatElapsed.
fn elapsed(ms: u64) -> String {
    match ms {
        3_600_000.. => format!("{}h {}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
        60_000.. => format!("{}m {}s", ms / 60_000, ms % 60_000 / 1000),
        1000.. => format!("{}s", ms / 1000),
        _ => format!("{ms}ms"),
    }
}

/// Reference startThinkingStatusMonitor, read passively from each answer observation.
/// shortcut: Rust never clicks the thinking disclosure, so it reports only the inline
/// thinking and streaming states, never the sidecar status or its UI progress percentage.
pub(crate) struct ThinkingMonitor {
    interval: Option<u64>,
    next: u64,
    fingerprint: Option<&'static str>,
    changed: u64,
}
impl ThinkingMonitor {
    pub(crate) fn new(options: &Value) -> Self {
        let interval = options["heartbeatIntervalMs"]
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            .map(|ms| (ms.floor() as u64).max(1000));
        Self {
            interval,
            next: interval.unwrap_or(0),
            fingerprint: None,
            changed: 0,
        }
    }
    /// The status line due at `now_ms` since the wait began, if any.
    pub(crate) fn line(&mut self, now_ms: u64, observed: &Value) -> Option<String> {
        let interval = self.interval?;
        if now_ms < self.next {
            return None;
        }
        self.next = now_ms - (now_ms - self.next) % interval + interval;
        let status = if observed["strongThinkingActive"] == true {
            Some("active")
        } else if observed["stopExists"] == true {
            Some("response streaming")
        } else {
            None
        };
        let Some(status) = status else {
            return Some(format!(
                "[browser] Waiting for ChatGPT response - {} elapsed; no thinking status detected yet.",
                elapsed(now_ms)
            ));
        };
        if self.fingerprint != Some(status) {
            self.fingerprint = Some(status);
            self.changed = now_ms;
        }
        let unchanged = now_ms - self.changed;
        let mut line = format!(
            "[browser] ChatGPT thinking - {} elapsed; status={status}",
            elapsed(now_ms)
        );
        if unchanged > 0 {
            line += &format!("; last change {} ago", elapsed(unchanged));
        }
        if unchanged >= 600_000 {
            line += "; stale-hint=no UI progress change";
        }
        Some(line + "; source=inline")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tokens: u64, bundled: Option<u64>) -> BrowserRequest {
        serde_json::from_value(json!({"version":1,"sessionId":"public","reservationId":"r","prompt":"p",
            "promptSha256":"h","optionsSha256":"o","uploadPaths":[],"estimatedInputTokens":tokens,
            "bundled":bundled.map(|count|json!({"originalCount":count,"bundlePath":"/public/bundle.txt","format":"text"}))}))
        .unwrap()
    }
    #[test]
    fn archive_decisions_follow_the_reference_order() {
        let meta = |mode: Value, url: Value| json!({"browser":{"config":{"archiveConversations":mode},"runtime":{"tabUrl":url}}});
        let url = "https://chatgpt.com/c/public";
        for (mode, tab, reason) in [
            (json!("never"), json!(url), "disabled"),
            (json!("always"), Value::Null, "missing-conversation-url"),
            (json!("always"), json!(url), "temporary-chat"),
            (json!("unknown"), json!(url), "temporary-chat"),
        ] {
            let (record, line) = archive(&meta(mode, tab.clone()));
            assert_eq!(record["reason"], reason);
            assert_eq!(record["attempted"], false);
            assert_eq!(record["archived"], false);
            assert_eq!(record["conversationUrl"], tab);
            assert_eq!(
                line,
                format!("[browser] ChatGPT archive skipped ({reason}).\n")
            );
        }
        assert_eq!(
            archive(&meta(json!("unknown"), json!(url))).0["mode"],
            "auto"
        );
    }
    fn started(ms_ago: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::milliseconds(ms_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn header_matches_the_reference_launch_lines() {
        let visible = "[browser] Browser control: launch visible Chrome; may focus/control the browser UI.\n[browser] Browser guidance: A visible automation Chrome window may take focus while Alcuin controls ChatGPT.\n[browser] Browser guidance: Use --browser-attach-running or --remote-chrome to reduce desktop disruption.\n";
        let meta = json!({"model":"gpt-6-pro","browser":{"config":{"desiredModel":"GPT-6 Pro"}}});
        assert_eq!(
            header(&meta, &request(31_234, Some(3))).unwrap(),
            format!(
                "Packed 3 files into 1 bundle (contents counted in token estimate).\nLaunching browser mode (target=GPT-6 Pro; requested=gpt-6-pro) with ~31,234 tokens.\nThis run can take up to an hour (usually ~10 minutes).\n{visible}"
            )
        );
        let current = json!({"model":"gpt-6-pro","browser":{"config":{"modelStrategy":"current","desiredModel":"GPT-6 Pro"}}});
        assert_eq!(
            header(&current, &request(12, None)).unwrap(),
            format!(
                "Launching browser mode (picker=current; requested=gpt-6-pro) with ~12 tokens.\nThis run can take up to an hour (usually ~10 minutes).\n{visible}"
            )
        );
    }
    #[test]
    fn verbose_headers_add_the_reference_runner_lines_and_quiet_headers_drop_them() {
        let config = json!({"desiredModel":"GPT-6","remoteChrome":{"host":"127.0.0.1","port":9222},"headless":true,
            "inlineCookies":[{"name":"PUBLIC"},{"name":"PUBLIC-2"}],"remoteToken":"PUBLIC-secret"});
        let mut bundled = request(40, Some(3));
        bundled.prompt = "PUBLIC \u{1f600}".into();
        bundled
            .extra
            .insert("attachmentDisplayPaths".into(), json!(["bundle.txt"]));
        let meta =
            json!({"model":"gpt-6-pro","options":{"verbose":true},"browser":{"config":config}});
        let text = header(&meta, &bundled).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        let printed: Value =
            serde_json::from_str(lines[0].strip_prefix("[verbose] Browser config: ").unwrap())
                .unwrap();
        assert_eq!(printed["inlineCookies"], "[redacted:2 cookies]");
        assert_eq!(printed["inlineCookieCount"], 2);
        assert!(printed.get("remoteToken").is_none());
        assert!(!text.contains("PUBLIC-secret"));
        assert_eq!(
            &lines[1..],
            [
                "[verbose] Browser prompt length: 9 chars",
                "[verbose] Browser attachments: bundle.txt",
                "[browser] Bundled 3 files into /public/bundle.txt.",
                "Packed 3 files into 1 bundle (contents counted in token estimate).",
                "Launching browser mode (target=GPT-6; requested=gpt-6-pro) with ~40 tokens.",
                "This run can take up to an hour (usually ~10 minutes).",
                "Chrome automation does not stream output; this may take a minute...",
                "[browser] Browser control: reuse an existing remote Chrome session; does not use a visible local browser window.",
                "[browser] Browser guidance: Alcuin opens a dedicated tab in the configured remote Chrome session.",
                "[browser] Browser guidance: Local Chrome launch, cookie copy, and window hiding flags are skipped.",
                "Note: --remote-chrome ignores local Chrome flags (--browser-headless).",
            ]
        );
        let quiet = json!({"model":"gpt-6-pro","browser":{"config":config}});
        let text = header(&quiet, &bundled).unwrap();
        assert!(
            !text.contains("[verbose]")
                && !text.contains("Note:")
                && !text.contains("Chrome automation")
        );
        assert!(
            text.contains("[browser] Browser control: reuse an existing remote Chrome session")
        );
        let inline = json!({"model":"gpt-6-pro","options":{"verbose":true,"file":["a.rs"]},"browser":{"config":{"hideWindow":true}}});
        let mut plain = request(5, None);
        plain.extra.insert("attachmentMode".into(), json!("inline"));
        let text = header(&inline, &plain).unwrap();
        assert!(
            text.contains("\n[verbose] Browser will paste file contents inline (no uploads).\n")
        );
        let hidden = if cfg!(target_os = "macos") {
            "\n[browser] Browser control: launch Chrome in hidden-window mode; may focus/control the browser UI.\n[browser] Browser guidance: On macOS, Alcuin launches Chrome off-screen while keeping the page rendered.\n"
        } else {
            "\n[browser] Browser control: launch visible Chrome; may focus/control the browser UI.\n[browser] Browser guidance: --browser-hide-window has no effect on Windows or Linux; Chrome stays visible.\n"
        };
        assert!(text.contains(hidden), "{text}");
    }
    #[test]
    fn quiet_runs_print_only_the_reference_keyword_browser_lines() {
        let quiet = json!({"options":{}});
        for line in [
            "[browser] Sending follow-up 1/2",
            "[browser] Waiting for ChatGPT generated image response.",
            "[browser] Deep Research activity detected; waiting for plan auto-confirm...",
            "[browser] Inline prompt too large; retrying with file uploads.",
        ] {
            assert!(prints(&quiet, line), "{line}");
        }
        for line in [
            "[browser] url = https://chatgpt.com/",
            "[reattach] chrome port=1 host=h",
            "Deep Research mode activated",
            "Uploading attachment: a.rs",
        ] {
            assert!(!prints(&quiet, line), "{line}");
            assert!(prints(&json!({"options":{"verbose":true}}), line));
        }
    }

    #[test]
    fn completion_block_matches_the_reference_evidence_answer_and_finish_lines() {
        let meta = json!({"model":"gpt-6-pro","startedAt":started(61_000),"options":{"file":["a.rs","b.rs"]},
            "browser":{"config":{"desiredModel":"GPT-6 Pro"},
            "modelSelection":{"requestedModel":"GPT-6 Pro","resolvedLabel":"Pro","status":"selected","strategy":"select","verified":true,"source":"picker","capturedAt":"T1"},
            "thinkingSelection":{"requestedLevel":"pro","status":"selected","resolvedLabel":"Extended","verified":true,"strictFailClosed":true,"source":"composer","capturedAt":"T2"}}});
        let block = completion(&meta, &request(1_500, None), &[], "PUBLIC answer words\n");
        let lines = block.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            "[browser] Model selection evidence: requestedKey=gpt-6-pro; target=GPT-6 Pro; resolvedLabel=Pro; status=selected; strategy=select; verified=yes; source=picker; capturedAt=T1"
        );
        assert_eq!(
            lines[1],
            "[browser] Thinking effort evidence: requestedLevel=pro; status=selected; resolvedLabel=Extended; verified=yes; failClosed=yes; targetModelKind=(none); observedModelKind=(none); source=composer; capturedAt=T2"
        );
        assert_eq!(lines[2], crate::browser_output::answer_heading(&meta));
        assert_eq!(&lines[3..5], ["PUBLIC answer words", ""]);
        assert!(lines[5].starts_with("1m0"), "{}", lines[5]);
        assert!(
            lines[5].ends_with(
                " \u{b7} Pro[browser] \u{b7} \u{2191}1.5k \u{2193}5 \u{21bb}0 \u{394}1.5k"
            ),
            "{}",
            lines[5]
        );
        assert_eq!(lines[6], "files=2");
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn large_fast_pro_runs_warn_like_the_reference() {
        let meta = json!({"model":"gpt-6-pro","startedAt":started(60_000),"browser":{"config":{"desiredModel":"GPT-6 Pro"}}});
        let warnings = run_warnings(&meta, &request(30_000, None));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["code"], "browser-pro-fast-large-run");
        assert_eq!(warnings[0]["details"]["requestedModel"], "GPT-6 Pro");
        let message = warnings[0]["message"].as_str().unwrap();
        assert!(
            message.starts_with("Large browser Pro run completed quickly (6"),
            "{message}"
        );
        assert!(message.ends_with("s for ~30,000 input tokens); verify the stored model selection evidence before claiming Pro Extended output."));
        let block = completion(&meta, &request(30_000, None), &warnings, "x");
        assert!(block.contains(&format!("\n[browser] {message}\n")));
        // Small, slow or non-Pro runs do not warn.
        assert!(run_warnings(&meta, &request(24_999, None)).is_empty());
        let slow = json!({"model":"gpt-6-pro","startedAt":started(121_000)});
        assert!(run_warnings(&slow, &request(30_000, None)).is_empty());
        let plain = json!({"model":"gpt-6","startedAt":started(60_000)});
        assert!(run_warnings(&plain, &request(30_000, None)).is_empty());
    }

    #[test]
    fn staging_cleanup_failures_become_structured_session_warnings_once() {
        let meta = json!({"model":"gpt-6","artifacts":[
            {"kind":"image","path":"/public/a.png","cleanupWarning":"PUBLIC stage left"},
            {"kind":"file","path":"/public/b.csv"},
            {"kind":"transcript","path":"/public/t.md","cleanupWarning":"ignored kind"}]});
        let added = completion_warnings(&meta, &request(10, None));
        assert_eq!(
            added,
            vec![
                json!({"code":"artifact-staging-cleanup-failed","severity":"warning",
            "message":"Artifact publication succeeded and was verified, but staging cleanup failed; residual staging data may remain.",
            "details":{"artifactKind":"image","origin":"local","filename":"a.png","path":"/public/a.png"}})
            ]
        );
        let browser = with_warnings(&json!({"warnings":[{"code":"PUBLIC earlier"}]}), &added);
        assert_eq!(with_warnings(&browser, &added), browser);
        assert_eq!(browser["warnings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_monitor_logs_the_reference_status_lines_on_its_interval() {
        let mut monitor = ThinkingMonitor::new(&json!({"heartbeatIntervalMs":30_000}));
        let thinking = json!({"strongThinkingActive":true,"stopExists":true});
        assert_eq!(monitor.line(29_999, &thinking), None);
        assert_eq!(
            monitor.line(30_400, &json!({"pending":true})).unwrap(),
            "[browser] Waiting for ChatGPT response - 30s elapsed; no thinking status detected yet."
        );
        assert_eq!(monitor.line(59_000, &thinking), None);
        assert_eq!(
            monitor.line(60_100, &thinking).unwrap(),
            "[browser] ChatGPT thinking - 1m 0s elapsed; status=active; source=inline"
        );
        assert_eq!(
            monitor.line(90_000, &thinking).unwrap(),
            "[browser] ChatGPT thinking - 1m 30s elapsed; status=active; last change 29s ago; source=inline"
        );
        assert_eq!(
            monitor.line(660_100, &thinking).unwrap(),
            "[browser] ChatGPT thinking - 11m 0s elapsed; status=active; last change 10m 0s ago; stale-hint=no UI progress change; source=inline"
        );
        assert_eq!(
            monitor.line(690_000, &json!({"stopExists":true})).unwrap(),
            "[browser] ChatGPT thinking - 11m 30s elapsed; status=response streaming; source=inline"
        );
        // A disabled heartbeat logs nothing.
        let mut off = ThinkingMonitor::new(&json!({}));
        assert_eq!(off.line(3_600_000, &thinking), None);
    }
}
