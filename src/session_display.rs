//! Plain session listings and attach summaries matching the reference sessionDisplay.ts,
//! sessionTable.ts, sessionLineage.ts and sessionLifecycle.ts, without colour.
use crate::{policy, sessions::SessionStore};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn clean(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(policy::trim)
        .filter(|text| !text.is_empty())
}
fn same_label(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}
fn browser_mode(meta: &Value) -> bool {
    meta["mode"]
        .as_str()
        .or(meta["options"]["mode"].as_str())
        .is_some_and(|mode| mode == "browser")
}
/// Reference resolveSessionBrowserModelDisplayName: verified picker evidence, else the key.
fn browser_model_name(meta: &Value, model: &Value) -> String {
    let session = clean(&meta["model"]);
    let requested = clean(model);
    let applies = match requested {
        None => session.is_none(),
        Some(requested) => session.is_some_and(|session| same_label(requested, session)),
    };
    let evidence = &meta["browser"]["modelSelection"];
    applies
        .then(|| {
            (evidence["verified"] == true)
                .then(|| clean(&evidence["resolvedLabel"]))
                .flatten()
        })
        .flatten()
        .or(requested)
        .unwrap_or("n/a")
        .to_owned()
}
fn browser_model_with_key(meta: &Value, model: &Value) -> String {
    let name = browser_model_name(meta, model);
    match clean(model) {
        Some(requested) if !same_label(&name, requested) => {
            format!("{name} (requested {requested})")
        }
        _ => name,
    }
}
/// Reference formatSessionExecutionLabel.
pub(crate) fn execution_label(meta: &Value) -> String {
    let lifecycle = &meta["lifecycle"];
    if !lifecycle.is_object() {
        return meta["mode"]
            .as_str()
            .or(meta["options"]["mode"].as_str())
            .unwrap_or("api")
            .to_owned();
    }
    let engine = match lifecycle["engine"].as_str() {
        Some("browser") => "br".to_owned(),
        Some(engine) => engine.to_owned(),
        None => "undefined".to_owned(),
    };
    let execution = if lifecycle["execution"] == "background" {
        "bg"
    } else {
        "fg"
    };
    format!("{engine}/{execution}")
}
/// Reference resolveSessionCost: stored cost, else the bundled per-token pricing.
pub(crate) fn session_cost(meta: &Value) -> Option<f64> {
    if browser_mode(meta) {
        return None;
    }
    if let Some(cost) = meta["usage"]["cost"].as_f64() {
        return Some(cost);
    }
    priced_cost(meta).filter(|cost| *cost > 0.0)
}
/// Reference estimateUsdCost from the bundled pricing and the saved token usage.
pub(crate) fn priced_cost(meta: &Value) -> Option<f64> {
    let usage = meta["usage"].as_object()?;
    // Only GPT-6 Astra pricing is bundled; the reference table also priced retired models.
    if meta["model"] != policy::DEFAULT_API_MODEL {
        return None;
    }
    let (input, output) = crate::providers::model_settings(policy::DEFAULT_API_MODEL, &Value::Null)
        .ok()?
        .pricing?;
    let number = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    Some(number("inputTokens") * input + number("outputTokens") * output)
}
/// Reference formatTimestampAligned: en-US local date and 12-hour time.
pub(crate) fn aligned_timestamp(iso: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return "Invalid Date".into();
    };
    let local = time.with_timezone(&chrono::Local);
    let hour = local.format("%-I").to_string();
    format!(
        "{}  {}{}:{}\u{202f}{}",
        local.format("%m/%d/%Y"),
        if hour.len() == 1 { " " } else { "" },
        hour,
        local.format("%M"),
        local.format("%p")
    )
}
pub(crate) fn table_header() -> String {
    format!(
        "{:<9} {:<13} {:<7} {:<19} {:>5} {:>7}  Slug",
        "Status", "Model", "Mode", "Timestamp", "Chars", "Cost"
    )
}
fn pad_end(text: &str, width: usize) -> String {
    let count = text.encode_utf16().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(count)))
}
fn pad_start(text: &str, width: usize) -> String {
    let count = text.encode_utf16().count();
    format!("{}{text}", " ".repeat(width.saturating_sub(count)))
}
/// Reference formatSessionTableRow without colour.
pub(crate) fn table_row(meta: &Value, slug: &str) -> String {
    let model = if browser_mode(meta) {
        browser_model_name(meta, &meta["model"])
    } else {
        meta["model"].as_str().unwrap_or("n/a").to_owned()
    };
    let chars = meta["options"]["prompt"]
        .as_str()
        .or(meta["promptPreview"].as_str())
        .map_or(0, |prompt| prompt.encode_utf16().count());
    let chars = if chars > 0 {
        pad_start(&chars.to_string(), 5)
    } else {
        "    -".into()
    };
    let cost = session_cost(meta).map_or_else(
        || "      -".to_owned(),
        |cost| pad_start(&format!("${}", crate::cli_execution::to_fixed(cost, 3)), 7),
    );
    format!(
        "{} {} {} {} {chars} {cost}  {slug}",
        pad_end(meta["status"].as_str().unwrap_or("unknown"), 9),
        pad_end(&model, 13),
        pad_end(&execution_label(meta), 7),
        pad_end(
            &meta["createdAt"]
                .as_str()
                .map_or_else(|| "Invalid Date".into(), aligned_timestamp),
            19
        ),
    )
}
fn response_id(record: &Value) -> Option<&str> {
    record["responseId"]
        .as_str()
        .or_else(|| record["id"].as_str())
        .filter(|id| id.starts_with("resp_"))
}
/// Reference buildResponseOwnerIndex: the first session listing each response owns it.
pub(crate) fn response_owners(sessions: &[Value]) -> BTreeMap<String, String> {
    let mut owners = BTreeMap::new();
    for session in sessions {
        let Some(id) = session["id"].as_str() else {
            continue;
        };
        let runs = session["models"].as_array().into_iter().flatten();
        for response in std::iter::once(&session["response"])
            .chain(runs.map(|run| &run["response"]))
            .filter_map(response_id)
        {
            owners
                .entry(response.to_owned())
                .or_insert_with(|| id.to_owned());
        }
    }
    owners
}
struct Lineage {
    response: Option<String>,
    session: Option<String>,
}
fn lineage(meta: &Value, owners: Option<&BTreeMap<String, String>>) -> Option<Lineage> {
    let previous = clean(&meta["options"]["previousResponseId"]);
    let mut parent = clean(&meta["options"]["followupSessionId"]).map(str::to_owned);
    if previous.is_none() && parent.is_none() {
        return None;
    }
    if parent.is_none()
        && let (Some(previous), Some(owners)) = (previous, owners)
    {
        parent = owners.get(previous).cloned();
    }
    if parent.as_deref() == meta["id"].as_str() {
        parent = None;
    }
    Some(Lineage {
        response: previous.map(str::to_owned),
        session: parent,
    })
}
/// Reference abbreviateResponseId.
fn abbreviate(response: &str) -> String {
    let chars: Vec<char> = response.chars().collect();
    if chars.len() <= 18 {
        return response.to_owned();
    }
    let head: String = chars[..11].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}
fn parent_label(lineage: &Lineage) -> Option<String> {
    let parent = lineage.session.as_ref()?;
    Some(match &lineage.response {
        Some(response) => format!("{parent} ({})", abbreviate(response)),
        None => parent.clone(),
    })
}
/// Reference buildSessionChainLine.
pub(crate) fn chain_line(meta: &Value, sessions: impl FnOnce() -> Vec<Value>) -> String {
    let id = meta["id"].as_str().unwrap_or("");
    let Some(direct) = lineage(meta, None) else {
        return format!("root -> {id}");
    };
    if let Some(label) = parent_label(&direct) {
        return format!("{label} -> {id}");
    }
    let Some(response) = direct.response.clone() else {
        return format!("root -> {id}");
    };
    let owners = response_owners(&sessions());
    let resolved = lineage(meta, Some(&owners)).unwrap_or(direct);
    match parent_label(&resolved) {
        Some(label) => format!("{label} -> {id}"),
        None => format!("{} -> {id}", abbreviate(&response)),
    }
}
/// Reference buildStatusTreeRows: followers nest under a listed parent; a parent outside the
/// listing is named after the row.
pub(crate) fn tree_rows(entries: &[Value], owners: &BTreeMap<String, String>) -> Vec<String> {
    let ids: Vec<&str> = entries
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or(""))
        .collect();
    let listed: BTreeSet<&str> = ids.iter().copied().collect();
    let lineages: Vec<Option<Lineage>> = entries
        .iter()
        .map(|entry| lineage(entry, Some(owners)))
        .collect();
    let parent_of = |index: usize| {
        lineages[index]
            .as_ref()
            .and_then(|lineage| lineage.session.as_deref())
            .filter(|parent| *parent != ids[index] && listed.contains(parent))
    };
    let mut children: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for index in 0..entries.len() {
        if let Some(parent) = parent_of(index) {
            children.entry(parent).or_default().push(index);
        }
    }
    let mut rows = vec![];
    let mut visited = BTreeSet::new();
    fn walk_child(
        index: usize,
        ancestors: &[bool],
        last: bool,
        context: (&[Value], &[&str], &BTreeMap<&str, Vec<usize>>),
        visited: &mut BTreeSet<usize>,
        rows: &mut Vec<String>,
    ) {
        let (entries, ids, children) = context;
        if !visited.insert(index) {
            return;
        }
        let prefix: String = ancestors
            .iter()
            .map(|more| if *more { "│  " } else { "   " })
            .collect();
        let branch = if last { "└─ " } else { "├─ " };
        rows.push(table_row(
            &entries[index],
            &format!("{prefix}{branch}{}", ids[index]),
        ));
        let kids = children.get(ids[index]).cloned().unwrap_or_default();
        let mut next = ancestors.to_vec();
        next.push(!last);
        for (position, child) in kids.iter().enumerate() {
            walk_child(
                *child,
                &next,
                position + 1 == kids.len(),
                context,
                visited,
                rows,
            );
        }
    }
    let roots: Vec<usize> = (0..entries.len())
        .filter(|index| parent_of(*index).is_none())
        .collect();
    for index in roots.into_iter().chain(0..entries.len()) {
        if !visited.insert(index) {
            continue;
        }
        let hidden = lineages[index]
            .as_ref()
            .filter(|lineage| {
                lineage
                    .session
                    .as_deref()
                    .is_some_and(|parent| !listed.contains(parent))
            })
            .and_then(parent_label)
            .map(|label| format!(" <- {label}"))
            .unwrap_or_default();
        rows.push(format!(
            "{}{hidden}",
            table_row(&entries[index], ids[index])
        ));
        let kids = children.get(ids[index]).cloned().unwrap_or_default();
        for (position, child) in kids.iter().enumerate() {
            walk_child(
                *child,
                &[],
                position + 1 == kids.len(),
                (entries, &ids, &children),
                &mut visited,
                &mut rows,
            );
        }
        if let Some(notice) = recovery_notice(&entries[index]) {
            rows.push(notice);
        }
    }
    rows
}
/// Reference formatBrowserRecoveryNotice: a running browser record whose recorded worker and
/// controller are positively gone.
fn recovery_notice(meta: &Value) -> Option<String> {
    let runtime = &meta["browser"]["runtime"];
    (meta["status"] == "running"
        && meta["mode"] == "browser"
        && meta["lifecycle"]["workerPid"].as_u64().is_some_and(|pid| pid > 0)
        && (runtime["chromePort"].as_u64().is_some_and(|port| port > 0)
            || clean(&runtime["chromeBrowserWSEndpoint"]).is_some())
        && crate::browser_historical::controllers_dead(meta))
    .then(|| {
        let id = meta["id"].as_str().unwrap_or("");
        format!(
            "Worker exited for {id}; stored status remains running. Use \"industrious session {id} --harvest\" to inspect the saved conversation."
        )
    })
}
/// Reference matchesModel: trimmed and case-insensitive against the run list or the model.
pub(crate) fn matches_model(entry: &Value, filter: &str) -> bool {
    let filter = policy::trim(filter).to_lowercase();
    if filter.is_empty() {
        return true;
    }
    match entry["models"].as_array() {
        Some(runs) => runs
            .iter()
            .any(|run| run["model"].as_str().map(str::to_lowercase) == Some(filter.clone())),
        None => entry["model"].as_str().map(str::to_lowercase) == Some(filter),
    }
}
/// Reference showStatus usage examples, less the pruning example this build leaves out.
pub(crate) const STATUS_EXAMPLES: &str = "\nUsage Examples\n  industrious status --hours 72 --limit 50\n    Show 72h of history capped at 50 entries.\n  industrious session <session-id>\n    Attach to a specific running/completed session to stream its output.";
/// Reference showStatus listing lines.
pub(crate) fn status_listing(
    entries: &[Value],
    all: &[Value],
    total: usize,
    truncated: bool,
    examples: bool,
) -> Vec<String> {
    let mut lines = vec![];
    if entries.is_empty() {
        lines.push("No stored sessions found.".to_owned());
    } else {
        lines.push("Recent Sessions".to_owned());
        lines.push(table_header());
        lines.extend(tree_rows(entries, &response_owners(all)));
        if truncated {
            lines.push(format!(
                "Showing {} of {total} sessions from the requested range. Rerun with --limit <count> or --all to see more.",
                entries.len()
            ));
        }
    }
    if examples {
        lines.push(STATUS_EXAMPLES.to_owned());
    }
    lines
}
/// Reference formatRelativeDuration.
fn relative_duration(iso: &str) -> Option<String> {
    let time = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let diff = chrono::Utc::now().timestamp_millis() - time.timestamp_millis();
    if diff < 0 {
        return None;
    }
    let seconds = ((diff as f64 / 1000.0).round() as i64).max(1);
    if seconds < 60 {
        return Some(format!("{seconds}s"));
    }
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if minutes < 60 {
        return Some(if rest > 0 {
            format!("{minutes}m {rest}s")
        } else {
            format!("{minutes}m")
        });
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return Some(if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        });
    }
    let (days, hours) = (hours / 24, hours % 24);
    Some(if hours > 0 {
        format!("{days}d {hours}h")
    } else {
        format!("{days}d")
    })
}
fn bytes(size: f64) -> String {
    if size < 1024.0 {
        format!("{size} B")
    } else if size < 1024.0 * 1024.0 {
        format!("{} KB", crate::cli_execution::to_fixed(size / 1024.0, 1))
    } else {
        format!(
            "{} MB",
            crate::cli_execution::to_fixed(size / (1024.0 * 1024.0), 1)
        )
    }
}
fn field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}
fn browser_evidence(meta: &Value) -> Vec<String> {
    let browser = &meta["browser"];
    let mut lines = vec![];
    let model = &browser["modelSelection"];
    if model.is_object() {
        lines.push(format!(
            "model requestedKey={}; target={}; resolvedLabel={}; status={}; strategy={}; verified={}; source={}; capturedAt={}",
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
    let thinking = &browser["thinkingSelection"];
    if thinking.is_object() {
        lines.push(format!(
            "effort requestedLevel={}; status={}; resolvedLabel={}; verified={}; failClosed={}; targetModelKind={}; observedModelKind={}; source={}; capturedAt={}",
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
    for warning in browser["warnings"].as_array().into_iter().flatten() {
        lines.push(format!(
            "warning {}: {}",
            field(&warning["code"]),
            field(&warning["message"])
        ));
    }
    lines
}
fn response_summary(response: &Value) -> Option<String> {
    let parts: Vec<String> = [
        ("response", "responseId"),
        ("request", "requestId"),
        ("status", "status"),
        ("incomplete", "incompleteReason"),
    ]
    .iter()
    .filter_map(|(label, key)| {
        response[*key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(|value| format!("{label}={value}"))
    })
    .collect();
    (!parts.is_empty()).then(|| parts.join(" | "))
}
fn transport_summary(transport: &Value) -> Option<String> {
    let reason = transport["reason"].as_str().filter(|r| !r.is_empty())?;
    let label = match reason {
        "client-timeout" => "client timeout (deadline exceeded)",
        "connection-lost" => "connection lost before completion",
        "client-abort" => "request aborted locally",
        "unknown" => "unknown transport failure",
        _ => "transport error",
    };
    Some(format!("{reason} — {label}"))
}
fn user_error_summary(error: &Value) -> Option<String> {
    let mut parts = vec![];
    if let Some(category) = error["category"].as_str().filter(|c| !c.is_empty()) {
        parts.push(category.to_owned());
    }
    if let Some(message) = error["message"].as_str().filter(|m| !m.is_empty()) {
        parts.push(format!("message={message}"));
    }
    if error["details"].as_object().is_some_and(|d| !d.is_empty()) {
        parts.push(format!("details={}", error["details"]));
    }
    (!parts.is_empty()).then(|| parts.join(" | "))
}
/// Reference attachSession metadata block, in order.
pub(crate) fn attach_metadata(meta: &Value, chain: &str) -> Vec<String> {
    let id = meta["id"].as_str().unwrap_or("");
    let mut lines = vec![];
    if meta["status"] == "running"
        && let Some(elapsed) = meta["startedAt"]
            .as_str()
            .or(meta["createdAt"].as_str())
            .and_then(relative_duration)
    {
        lines.push(format!(
            "Session {id} reattached, request started {elapsed} ago."
        ));
    }
    lines.push(format!("Chain: {chain}"));
    lines.push(format!("Created: {}", field(&meta["createdAt"])));
    lines.push(format!("Status: {}", field(&meta["status"])));
    if meta["lifecycle"].is_object() {
        lines.push(format!(
            "Execution: {} ({})",
            execution_label(meta),
            if meta["lifecycle"]["attached"] == true {
                "attached"
            } else {
                "detached"
            }
        ));
        lines.push(format!(
            "Reattach: {}",
            field(&meta["lifecycle"]["reattachCommand"])
        ));
    }
    let browser = browser_mode(meta);
    match meta["models"].as_array().filter(|runs| !runs.is_empty()) {
        Some(runs) => {
            lines.push("Models:".into());
            for run in runs {
                let count = |key: &str| {
                    crate::cli_execution::token_count(
                        run["usage"][key].as_u64().unwrap_or(0) as usize
                    )
                };
                let usage = if run["usage"].is_object() {
                    format!(" tok={}/{}", count("outputTokens"), count("totalTokens"))
                } else {
                    String::new()
                };
                let label = if browser {
                    browser_model_with_key(meta, &run["model"])
                } else {
                    field(&run["model"])
                };
                lines.push(format!("- {label} — {}{usage}", field(&run["status"])));
            }
        }
        None => {
            if clean(&meta["model"]).is_some() || meta["model"].as_str().is_some() {
                let label = if browser {
                    browser_model_with_key(meta, &meta["model"])
                } else {
                    field(&meta["model"])
                };
                lines.push(format!("Model: {label}"));
            }
        }
    }
    let evidence = browser_evidence(meta);
    if !evidence.is_empty() {
        lines.push("Browser evidence:".into());
        lines.extend(evidence.into_iter().map(|line| format!("- {line}")));
    }
    if let Some(artifacts) = meta["artifacts"].as_array().filter(|a| !a.is_empty()) {
        lines.push("Artifacts:".into());
        for artifact in artifacts {
            let label = artifact["label"]
                .as_str()
                .or(artifact["kind"].as_str())
                .map_or_else(|| "undefined".to_owned(), str::to_owned);
            let size = artifact["sizeBytes"]
                .as_f64()
                .filter(|size| *size != 0.0)
                .map(|size| format!(" ({})", bytes(size)))
                .unwrap_or_default();
            let checksum = artifact["sha256"]
                .as_str()
                .filter(|sha| !sha.is_empty())
                .map(|sha| format!(" sha256={}…", sha.chars().take(12).collect::<String>()))
                .unwrap_or_default();
            let validation = if artifact["validation"].is_object() {
                format!(
                    " validation={}",
                    if artifact["validation"]["ok"] == true {
                        "ok"
                    } else {
                        artifact["validation"]["error"]
                            .as_str()
                            .filter(|e| !e.is_empty())
                            .unwrap_or("failed")
                    }
                )
            } else {
                String::new()
            };
            let transfer = artifact["transfer"]["status"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|status| format!(" transfer={status}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {label} — {}{size}{checksum}{validation}{transfer}",
                field(&artifact["path"])
            ));
        }
    }
    if let Some(summary) = response_summary(&meta["response"]) {
        lines.push(format!("Response: {summary}"));
    }
    if let Some(summary) = transport_summary(&meta["transport"]) {
        lines.push(format!("Transport: {summary}"));
    }
    if let Some(summary) = user_error_summary(&meta["error"]) {
        lines.push(format!("User error: {summary}"));
    }
    lines
}
/// Reference readStoredPrompt: the committed request prompt, else the saved option.
pub(crate) fn stored_prompt(store: &SessionStore, id: &str, meta: &Value) -> Option<String> {
    let request = store.request(id).ok().flatten();
    [
        request
            .as_ref()
            .and_then(|request| request["prompt"].as_str()),
        meta["options"]["prompt"].as_str(),
    ]
    .into_iter()
    .flatten()
    .find(|prompt| !policy::trim(prompt).is_empty())
    .map(str::to_owned)
}
/// Reference buildSessionLogForDisplay.
pub(crate) fn display_log(
    store: &SessionStore,
    id: &str,
    meta: &Value,
    filter: Option<&str>,
) -> anyhow::Result<String> {
    let filter = filter
        .map(|filter| policy::trim(filter).to_lowercase())
        .filter(|filter| !filter.is_empty());
    let runs = meta["models"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        if filter.is_some()
            && let Some(model) = meta["model"].as_str()
        {
            let log = store.log_for_model(id, model)?;
            if !log.is_empty() {
                return Ok(log);
            }
        }
        return store.log(id);
    }
    let candidates: Vec<&Value> = runs
        .iter()
        .filter(|run| {
            filter.as_ref().is_none_or(|filter| {
                run["model"].as_str().map(str::to_lowercase).as_ref() == Some(filter)
            })
        })
        .collect();
    if candidates.is_empty() {
        return Ok(String::new());
    }
    let mut sections = vec![];
    let mut content = false;
    for run in candidates {
        let model = run["model"].as_str().unwrap_or("");
        let body = store.log_for_model(id, model)?;
        content |= !policy::trim(&body).is_empty();
        sections.push(format!("=== {model} ===\n{body}").trim_end().to_owned());
    }
    if !content {
        if filter.is_some() && runs.len() > 1 {
            return Ok(String::new());
        }
        return store.log(id);
    }
    Ok(sections.join("\n\n"))
}
/// Reference trimBeforeFirstAnswer, less the Deep Research placeholder replacement: the log from
/// the first line matching `^(?:Answer:|[^\r\n]+ said:)`.
pub(crate) fn trim_before_first_answer(log: &str) -> &str {
    let mut offset = 0;
    for line in log.split_inclusive('\n') {
        let line_text = line.trim_end_matches(['\r', '\n']);
        let after_first = line_text
            .char_indices()
            .nth(1)
            .map_or("", |(index, _)| &line_text[index..]);
        if line_text.starts_with("Answer:") || after_first.contains(" said:") {
            return &log[offset..];
        }
        offset += line.len();
    }
    log
}
/// Reference formatCompletionSummary with the slug.
pub(crate) fn completion_summary(meta: &Value) -> Option<String> {
    let usage = &meta["usage"];
    if !usage.is_object() {
        return None;
    }
    let elapsed = meta["elapsedMs"].as_f64()?;
    let model = if browser_mode(meta) {
        format!("{}[browser]", browser_model_name(meta, &meta["model"]))
    } else {
        meta["model"].as_str().unwrap_or("n/a").to_owned()
    };
    let tokens: Vec<String> = [
        "inputTokens",
        "outputTokens",
        "reasoningTokens",
        "totalTokens",
    ]
    .iter()
    .map(|key| {
        let text = crate::cli_execution::token_count(usage[*key].as_u64().unwrap_or(0) as usize);
        if usage[*key].is_null() {
            format!("{text}*")
        } else {
            text
        }
    })
    .collect();
    let line1 = [
        Some(if elapsed < 0.0 {
            "unknown".to_owned()
        } else {
            crate::cli_execution::elapsed_compact(elapsed as u64)
        }),
        session_cost(meta).map(|cost| {
            if cost.is_finite() {
                format!("${}", crate::cli_execution::to_fixed(cost, 4))
            } else {
                "n/a".into()
            }
        }),
        Some(model).filter(|model| !model.is_empty()),
        Some(format!(
            "↑{} ↓{} ↻{} Δ{}",
            tokens[0], tokens[1], tokens[2], tokens[3]
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let files = meta["options"]["file"].as_array().map_or(0, Vec::len);
    let line2 = [
        (files > 0).then(|| format!("files={files}")),
        meta["id"].as_str().map(|id| format!("slug={id}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" | ");
    Some(if line2.is_empty() {
        line1
    } else {
        format!("{line1} | {line2}")
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn table_rows_follow_the_reference_columns() {
        assert_eq!(
            table_header(),
            "Status    Model         Mode    Timestamp           Chars    Cost  Slug"
        );
        let meta = json!({"id":"public-row","status":"completed","model":"gpt-6-astra","mode":"api","createdAt":"2026-10-26T01:02:03.000Z","options":{"prompt":"PUBLIC row"},"usage":{"inputTokens":1000,"outputTokens":100},"lifecycle":{"engine":"api","execution":"foreground"}});
        let row = table_row(&meta, "public-row");
        let timestamp = aligned_timestamp("2026-10-26T01:02:03.000Z");
        assert_eq!(
            row,
            format!("completed gpt-6-astra   api/fg  {timestamp}    10  $0.015  public-row")
        );
        let browser = json!({"id":"public-browser","status":"running","model":"gpt-6-pro","mode":"browser","createdAt":"2026-10-26T01:02:03.000Z","browser":{"modelSelection":{"verified":true,"resolvedLabel":"Pro"}}});
        assert!(
            table_row(&browser, "public-browser").starts_with("running   Pro           browser ")
        );
        assert!(table_row(&browser, "x").ends_with("    -       -  x"));
    }
    #[test]
    fn timestamps_use_en_us_twelve_hour_alignment() {
        let local = chrono::DateTime::parse_from_rfc3339("2026-10-26T01:02:03.000Z")
            .unwrap()
            .with_timezone(&chrono::Local);
        let text = aligned_timestamp("2026-10-26T01:02:03.000Z");
        assert!(
            text.starts_with(&local.format("%m/%d/%Y  ").to_string()),
            "{text}"
        );
        assert_eq!(text.chars().count(), 20, "{text}");
        assert!(text.ends_with("\u{202f}AM") || text.ends_with("\u{202f}PM"));
    }
    #[test]
    fn lineage_nests_followers_and_names_hidden_parents() {
        let owners = response_owners(&[
            json!({"id":"public-parent","response":{"responseId":"resp_PUBLIC_parent_response_1234"}}),
        ]);
        let entries = vec![
            json!({"id":"public-parent","status":"completed","createdAt":"2026-10-26T01:00:00Z"}),
            json!({"id":"public-child","status":"completed","createdAt":"2026-10-26T01:00:00Z","options":{"previousResponseId":"resp_PUBLIC_parent_response_1234"}}),
            json!({"id":"public-orphan","status":"completed","createdAt":"2026-10-26T01:00:00Z","options":{"followupSessionId":"public-gone","previousResponseId":"resp_PUBLIC_gone_response_99999"}}),
        ];
        let rows = tree_rows(&entries, &owners);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].ends_with("  public-parent"), "{}", rows[0]);
        assert!(rows[1].ends_with("  └─ public-child"), "{}", rows[1]);
        assert!(
            rows[2].ends_with("  public-orphan <- public-gone (resp_PUBLIC...9999)"),
            "{}",
            rows[2]
        );
        assert_eq!(
            chain_line(&entries[1], || vec![
                json!({"id":"public-parent","response":{"responseId":"resp_PUBLIC_parent_response_1234"}})
            ]),
            "public-parent (resp_PUBLIC...1234) -> public-child"
        );
        assert_eq!(chain_line(&entries[0], Vec::new), "root -> public-parent");
        assert_eq!(
            execution_label(&json!({"lifecycle":{"engine":"browser","execution":"background"}})),
            "br/bg"
        );
        assert_eq!(execution_label(&json!({"mode":"browser"})), "browser");
    }
    #[test]
    fn attach_metadata_and_summary_follow_the_reference_lines() {
        let meta = json!({"id":"public-attach","status":"completed","createdAt":"2026-10-26T01:00:00.000Z","model":"gpt-6-astra","mode":"api","elapsedMs":1500,
            "lifecycle":{"engine":"api","execution":"background","attached":false,"reattachCommand":"industrious session public-attach"},
            "models":[{"model":"gpt-6-astra","status":"completed","usage":{"outputTokens":1200,"totalTokens":3400}}],
            "usage":{"inputTokens":2200,"outputTokens":1200,"reasoningTokens":0,"totalTokens":3400,"cost":0.0822},
            "artifacts":[{"kind":"answer","path":"PUBLIC/answer.md","sizeBytes":2048,"sha256":"0123456789abcdef","validation":{"ok":true}}],
            "response":{"responseId":"resp_PUBLIC","status":"completed"},
            "transport":{"reason":"client-timeout"},
            "error":{"category":"provider","message":"PUBLIC failure","details":{"code":1}},
            "options":{"file":["a.rs"]}});
        assert_eq!(
            attach_metadata(&meta, "root -> public-attach"),
            [
                "Chain: root -> public-attach",
                "Created: 2026-10-26T01:00:00.000Z",
                "Status: completed",
                "Execution: api/bg (detached)",
                "Reattach: industrious session public-attach",
                "Models:",
                "- gpt-6-astra — completed tok=1.2k/3.4k",
                "Artifacts:",
                "- answer — PUBLIC/answer.md (2.0 KB) sha256=0123456789ab… validation=ok",
                "Response: response=resp_PUBLIC | status=completed",
                "Transport: client-timeout — client timeout (deadline exceeded)",
                "User error: provider | message=PUBLIC failure | details={\"code\":1}",
            ]
        );
        assert_eq!(
            completion_summary(&meta).unwrap(),
            "1.5s · $0.0822 · gpt-6-astra · ↑2.2k ↓1.2k ↻0 Δ3.4k | files=1 | slug=public-attach"
        );
        assert_eq!(
            trim_before_first_answer("Industrious header\nmore\nAnswer:\nbody\n"),
            "Answer:\nbody\n"
        );
        assert_eq!(
            trim_before_first_answer("head\nGPT-6 Pro said:\nbody\n"),
            "GPT-6 Pro said:\nbody\n"
        );
        assert_eq!(trim_before_first_answer("no heading\n"), "no heading\n");
    }
}
