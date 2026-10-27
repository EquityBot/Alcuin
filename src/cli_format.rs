//! Plain CLI text that mirrors the reference Commander and display helpers.
use serde_json::Value;
use std::ffi::OsString;

const SUBCOMMANDS: &[&str] = &[
    "perf-summary",
    "browser-health",
    "recover-native",
    "serve",
    "recover-remote",
    "project-sources",
    "bridge",
    "tui",
    "doctor",
    "docs",
    "session",
    "status",
    "restart",
];

fn spec_options(spec: &Value) -> impl Iterator<Item = &str> {
    spec["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option["flags"].as_str())
}
fn long_flag(flags: &str) -> Option<&str> {
    flags
        .split_whitespace()
        .find(|word| word.starts_with("--"))
        .map(|word| word.trim_end_matches(','))
}
fn takes_value(flags: &str) -> bool {
    flags.contains('<')
}
fn root_options() -> Value {
    serde_json::from_str(include_str!("cli_spec.json")).expect("captured CLI option definitions")
}
fn commands() -> Value {
    serde_json::from_str(include_str!("cli_commands.json"))
        .expect("captured CLI command definitions")
}
/// The subcommand chain Commander would dispatch to, skipping root option values.
fn command_path(args: &[OsString]) -> Vec<String> {
    let root = root_options();
    let valued = |arg: &str| {
        root.as_array().into_iter().flatten().any(|option| {
            let flags = option["flags"].as_str().unwrap_or("");
            takes_value(flags)
                && !flags.contains("...")
                && flags
                    .split_whitespace()
                    .any(|word| word.trim_end_matches(',') == arg)
        })
    };
    let mut path = vec![];
    let mut skip = false;
    for arg in args.iter().skip(1).filter_map(|arg| arg.to_str()) {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            skip = !arg.contains('=') && path.is_empty() && valued(arg);
            continue;
        }
        if path.is_empty() {
            if !SUBCOMMANDS.contains(&arg) {
                return path;
            }
            path.push(arg.to_owned());
        } else {
            let spec = commands();
            let children = spec
                .as_array()
                .into_iter()
                .flatten()
                .find(|command| command_name(command) == path[0])
                .map(|command| command["children"].clone())
                .unwrap_or_default();
            if children
                .as_array()
                .into_iter()
                .flatten()
                .any(|child| command_name(child) == arg)
            {
                path.push(arg.to_owned());
            }
            return path;
        }
    }
    path
}
/// A program option as Commander's parseOptions sees it.
struct RootOption {
    flags: String,
    short: Option<String>,
    long: String,
    required: bool,
    optional: bool,
    variadic: bool,
}
fn root_option_table() -> Vec<RootOption> {
    let root = root_options();
    let flags = root
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option["flags"].as_str())
        // Commander's .version() registers -V, --version as a program option.
        .chain(["-V, --version"]);
    flags
        .map(|flags| {
            let words: Vec<_> = flags
                .split([' ', ','])
                .filter(|word| !word.is_empty())
                .collect();
            RootOption {
                flags: flags.to_owned(),
                short: words
                    .iter()
                    .find(|word| word.len() == 2 && word.starts_with('-'))
                    .map(|word| (*word).to_owned()),
                long: words
                    .iter()
                    .find(|word| word.starts_with("--"))
                    .map(|word| (*word).to_owned())
                    .unwrap_or_default(),
                required: flags.contains('<'),
                optional: flags.contains('['),
                variadic: flags.contains("..."),
            }
        })
        .collect()
}
/// Commander's negativeNumberArg; no program option uses a digit short flag.
fn negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let (number, exponent) = match rest.split_once('e') {
        Some((number, exponent)) => (number, Some(exponent)),
        None => (rest, None),
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let mantissa = digits(number)
        || number.split_once('.').is_some_and(|(whole, fraction)| {
            whole.bytes().all(|b| b.is_ascii_digit()) && digits(fraction)
        });
    mantissa
        && exponent
            .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}
/// Commander's program parseOptions takes every program option wherever it is typed, before the
/// rest reaches the subcommand. When the first operand names a command, the program options
/// after it move in front of it (values attached with `=`), so clap reads them as program
/// options exactly as it does when they come first. After `session`, `--path` is the view's
/// boolean (reference scopeSessionPathOption). A program option missing its value after the
/// command is Commander's first error.
pub(crate) fn hoist_root_options(args: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let options = root_option_table();
    let find = |arg: &str| {
        options
            .iter()
            .find(|option| option.long == arg || option.short.as_deref() == Some(arg))
    };
    let maybe_option = |arg: &str| arg.len() > 1 && arg.starts_with('-');
    let lossy = |index: usize| args[index].to_string_lossy().into_owned();
    let attached = |option: &RootOption, value: &std::ffi::OsStr| {
        let mut text = OsString::from(format!("{}=", option.long));
        text.push(value);
        text
    };
    // Clap reads a bare optional value from the next argument, so give it its missing value.
    let bare = |option: &RootOption| attached(option, crate::cli::OPTIONAL_VALUE_MISSING.as_ref());
    let mut command: Option<(usize, String)> = None;
    let mut edits = std::collections::BTreeMap::<usize, Option<String>>::new();
    let mut hoisted: Vec<OsString> = vec![];
    let mut variadic: Option<&RootOption> = None;
    let mut group: Option<String> = None;
    let (mut next, mut token) = (1, 0);
    loop {
        let grouped = group.is_some();
        let arg = match group.take() {
            Some(arg) => arg,
            None if next < args.len() => {
                token = next;
                next += 1;
                lossy(token)
            }
            None => break,
        };
        let after = command.is_some();
        if arg == "--" {
            if grouped && after {
                edits.insert(token, Some(arg));
            }
            break;
        }
        if let Some(option) = variadic
            && (!maybe_option(&arg) || negative_number(&arg))
        {
            if after {
                edits.insert(token, None);
                hoisted.push(attached(option, &args[token]));
            }
            continue;
        }
        variadic = None;
        let session_path = |option: &RootOption| {
            option.long == "--path" && command.as_ref().is_some_and(|(_, name)| name == "session")
        };
        if maybe_option(&arg)
            && let Some(option) = find(&arg)
        {
            if session_path(option) {
                continue;
            }
            if after {
                edits.insert(token, None);
            }
            let value = if option.required {
                if next >= args.len() {
                    if after {
                        return Err(format!(
                            "error: option '{}' argument missing\n(use --help for usage)\n",
                            option.flags
                        ));
                    }
                    return Ok(args);
                }
                next += 1;
                Some(next - 1)
            } else if option.optional
                && next < args.len()
                && (!maybe_option(&lossy(next)) || negative_number(&lossy(next)))
            {
                next += 1;
                Some(next - 1)
            } else {
                None
            };
            if after {
                hoisted.push(match value {
                    Some(index) => {
                        edits.insert(index, None);
                        attached(option, &args[index])
                    }
                    None if option.optional => bare(option),
                    None => OsString::from(&option.long),
                });
            }
            variadic = option.variadic.then_some(option);
            continue;
        }
        let mut chars = arg.chars();
        if let (Some('-'), Some(short)) = (chars.next(), chars.next())
            && short != '-'
            && !chars.as_str().is_empty()
            && let Some(option) = find(&format!("-{short}"))
        {
            let rest = chars.as_str();
            if after {
                edits.insert(token, None);
            }
            if option.required || option.optional {
                if after {
                    hoisted.push(attached(option, rest.as_ref()));
                }
            } else {
                if after {
                    hoisted.push(OsString::from(&option.long));
                }
                group = Some(format!("-{rest}"));
            }
            continue;
        }
        if let Some(equals) = arg
            .find('=')
            .filter(|index| arg.starts_with("--") && *index > 2)
            && let Some(option) = find(&arg[..equals]).filter(|o| o.required || o.optional)
        {
            if session_path(option) {
                continue;
            }
            if after {
                edits.insert(token, None);
                hoisted.push(args[token].clone());
            }
            continue;
        }
        if after {
            if grouped {
                edits.insert(token, Some(arg));
            }
            continue;
        }
        // An unknown option before any operand, or a first operand that is not a command,
        // leaves the program to run the root action.
        if maybe_option(&arg) || !SUBCOMMANDS.contains(&arg.as_str()) {
            return Ok(args);
        }
        command = Some((token, arg));
    }
    let Some((index, _)) = command else {
        return Ok(args);
    };
    let mut hoisted_args = args[..index].to_vec();
    hoisted_args.extend(hoisted);
    for (position, arg) in args.into_iter().enumerate().skip(index) {
        match edits.remove(&position) {
            Some(None) => {}
            Some(Some(residue)) => hoisted_args.push(residue.into()),
            None => hoisted_args.push(arg),
        }
    }
    Ok(hoisted_args)
}
fn command_name(spec: &Value) -> &str {
    spec["command"]
        .as_str()
        .and_then(|command| command.split_whitespace().next())
        .unwrap_or("")
}
fn command_spec(path: &[String]) -> Option<Value> {
    let mut current = commands();
    let mut found = None;
    for name in path {
        let next = current
            .as_array()?
            .iter()
            .find(|command| command_name(command) == name)?
            .clone();
        current = next["children"].clone();
        found = Some(next);
    }
    found
}
/// Visible long flags Commander offers as suggestions, nearest command first.
fn candidates(path: &[String]) -> Vec<String> {
    let mut flags = vec![];
    for depth in (1..=path.len()).rev() {
        if let Some(spec) = command_spec(&path[..depth]) {
            flags.extend(
                spec_options(&spec)
                    .filter(|flags| !flags.contains("--clean"))
                    .filter_map(long_flag)
                    .map(str::to_owned),
            );
            flags.push("--help".into());
        }
    }
    let help = include_str!("../fixtures/cli-help.txt");
    let options = help
        .split("\nOptions:")
        .nth(1)
        .and_then(|rest| rest.split("\nCommands:").next())
        .unwrap_or("");
    for line in options.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("  ")
            && rest.starts_with('-')
            && let Some(flag) = rest
                .split("  ")
                .next()
                .and_then(long_flag)
                .map(str::to_owned)
        {
            flags.push(flag);
        }
    }
    flags
}
/// Optimal string alignment distance, as Commander's suggestSimilar computes it.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for j in 1..=b.len() {
        for i in 1..=a.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}
fn suggest_similar(word: &str, candidates: &[String]) -> String {
    let mut unique: Vec<&str> = vec![];
    for candidate in candidates {
        if !unique.contains(&candidate.as_str()) {
            unique.push(candidate);
        }
    }
    let options = word.starts_with("--");
    let word = if options { &word[2..] } else { word };
    let mut similar: Vec<&str> = vec![];
    let mut best = 3;
    for candidate in unique {
        let candidate = if options {
            candidate.get(2..).unwrap_or("")
        } else {
            candidate
        };
        if candidate.chars().count() <= 1 {
            continue;
        }
        let distance = edit_distance(word, candidate);
        let length = word.chars().count().max(candidate.chars().count());
        if (length as f64 - distance as f64) / length as f64 > 0.4 {
            if distance < best {
                best = distance;
                similar = vec![candidate];
            } else if distance == best {
                similar.push(candidate);
            }
        }
    }
    similar.sort_by_key(|text| text.to_lowercase());
    let similar: Vec<String> = similar
        .into_iter()
        .map(|text| {
            if options {
                format!("--{text}")
            } else {
                text.to_owned()
            }
        })
        .collect();
    match similar.len() {
        0 => String::new(),
        1 => format!("\n(Did you mean {}?)", similar[0]),
        _ => format!("\n(Did you mean one of {}?)", similar.join(", ")),
    }
}
fn option_flags(path: &[String], long: &str) -> Option<String> {
    let scoped = command_spec(path)
        .map(|spec| spec_options(&spec).map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    let root = root_options();
    scoped
        .into_iter()
        .chain(
            root.as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| option["flags"].as_str().map(str::to_owned)),
        )
        .find(|flags| long_flag(flags) == Some(long))
}
fn expected_arguments(path: &[String]) -> usize {
    match command_spec(path) {
        Some(spec) => spec["command"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .skip(1)
            .count(),
        None => 1,
    }
}
/// Reference Commander wording for parse failures that clap reports differently.
pub(crate) fn commander_error(error: &clap::error::Error, args: &[OsString]) -> Option<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let path = command_path(args);
    let invalid = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(value)) => Some(value.clone()),
        Some(ContextValue::Strings(values)) => values.first().cloned(),
        _ => None,
    };
    let message = match error.kind() {
        ErrorKind::UnknownArgument => {
            let invalid = invalid?;
            if invalid.starts_with('-') {
                let suggestion = if invalid.starts_with("--") {
                    suggest_similar(&invalid, &candidates(&path))
                } else {
                    String::new()
                };
                format!("error: unknown option '{invalid}'{suggestion}")
            } else {
                let expected = expected_arguments(&path);
                let received = args
                    .iter()
                    .skip(1)
                    .filter_map(|arg| arg.to_str())
                    .filter(|arg| !arg.starts_with('-'))
                    .count()
                    .saturating_sub(path.len())
                    .max(expected + 1);
                let scope = path
                    .last()
                    .map(|name| format!(" for '{name}'"))
                    .unwrap_or_default();
                format!(
                    "error: too many arguments{scope}. Expected {expected} argument{} but got {received}.",
                    if expected == 1 { "" } else { "s" }
                )
            }
        }
        ErrorKind::InvalidValue => {
            let invalid = invalid?;
            let long = invalid.split_whitespace().next()?.to_owned();
            let flags = option_flags(&path, &long)?;
            let value = match error.get(ContextKind::InvalidValue) {
                Some(ContextValue::String(value)) => value.clone(),
                _ => String::new(),
            };
            if !value.is_empty() {
                return None;
            }
            format!("error: option '{flags}' argument missing")
        }
        ErrorKind::MissingRequiredArgument => {
            let name = invalid?;
            if name.starts_with('-') {
                return None;
            }
            let name = name
                .trim_matches(['<', '>', '[', ']'])
                .trim_end_matches("...");
            format!("error: missing required argument '{name}'")
        }
        _ => return None,
    };
    Some(format!("{message}\n(use --help for usage)\n"))
}
/// Reference formatCompactNumber.
pub(crate) fn compact_number(value: f64) -> String {
    if !value.is_finite() {
        return "0".into();
    }
    let strip = |text: String| text.strip_suffix(".0").map(str::to_owned).unwrap_or(text);
    let abs = value.abs();
    if abs >= 1_000_000.0 {
        format!(
            "{}m",
            strip(crate::cli_execution::to_fixed(value / 1_000_000.0, 1))
        )
    } else if abs >= 1_000.0 {
        format!(
            "{}k",
            strip(crate::cli_execution::to_fixed(value / 1_000.0, 1))
        )
    } else {
        locale_number(value)
    }
}
/// en-US `Number.prototype.toLocaleString` for the integers and short decimals the CLI prints.
pub(crate) fn locale_number(value: f64) -> String {
    let negative = value < 0.0;
    let rounded = (value.abs() * 1000.0).round() / 1000.0;
    let whole = rounded.trunc();
    let mut fraction = format!("{:.3}", rounded - whole);
    fraction = fraction
        .trim_start_matches('0')
        .trim_end_matches('0')
        .to_owned();
    if fraction == "." {
        fraction.clear();
    }
    format!(
        "{}{}{}",
        if negative { "-" } else { "" },
        crate::cli::grouped(whole as usize),
        fraction
    )
}
fn nonempty(value: Option<&str>) -> Option<&str> {
    value
        .map(crate::policy::trim)
        .filter(|value| !value.is_empty())
}
fn azure_note(mode: &str, configured: bool, azure: bool) -> Option<&'static str> {
    if !configured {
        None
    } else if mode == "openai" {
        Some("ignored, --provider openai/--no-azure is active")
    } else if azure {
        Some("active because Azure endpoint is configured")
    } else {
        Some("configured, not used for this model")
    }
}
/// Reference buildProviderRoutePlan: routing failures become a not-ready plan, never an error,
/// and the key value never leaves its masked preview. Model policy failures still refuse.
pub(crate) fn route_plan(
    model: &str,
    options: &Value,
    env: &crate::config::Environment,
) -> anyhow::Result<Value> {
    crate::policy::assert_model_allowed(Some(model))?;
    let mode = options["provider"].as_str().unwrap_or("auto");
    let endpoint = nonempty(options["azure"]["endpoint"].as_str());
    let configured = endpoint.is_some();
    let key = |names: &[&str]| {
        names.iter().find_map(|name| {
            nonempty(env.get(*name).map(String::as_str)).map(|key| (name.to_string(), key))
        })
    };
    let explicit_base = nonempty(options["baseUrl"].as_str())
        .or_else(|| nonempty(env.get("OPENAI_BASE_URL").map(String::as_str)));
    let (azure, provider_label, base, source, secret, deployment, error) =
        match crate::providers::resolve_route(model, options, env) {
            Ok((route, _)) => {
                let azure = route.provider_label == "Azure OpenAI";
                let names: Vec<&str> = route.key_source.split('|').collect();
                let secret = key(&names);
                let base = if azure {
                    endpoint
                        .map(crate::providers::route_target)
                        .unwrap_or_default()
                } else if explicit_base.is_none() && route.base_url == "https://api.openai.com/v1" {
                    "api.openai.com".into()
                } else {
                    crate::providers::route_target(&route.base_url)
                };
                let error = secret
                    .is_none()
                    .then(|| format!("Missing {}.", route.key_source));
                (
                    azure,
                    route.provider_label,
                    base,
                    route.key_source,
                    secret,
                    route.azure_deployment_name,
                    error,
                )
            }
            Err(error) => {
                let azure = (configured && mode != "openai") || mode == "azure";
                let names: &[&str] = if azure {
                    &["AZURE_OPENAI_API_KEY", "OPENAI_API_KEY"]
                } else {
                    &["OPENAI_API_KEY"]
                };
                let secret = key(names);
                let base = if azure {
                    endpoint
                        .map(crate::providers::route_target)
                        .unwrap_or_default()
                } else {
                    explicit_base
                        .map(crate::providers::route_target)
                        .unwrap_or_else(|| "api.openai.com".into())
                };
                (
                    azure,
                    if azure { "Azure OpenAI" } else { "OpenAI" }.to_owned(),
                    base,
                    secret
                        .as_ref()
                        .map(|(name, _)| name.clone())
                        .unwrap_or_else(|| names.join("|")),
                    secret,
                    if azure {
                        nonempty(options["azure"]["deployment"].as_str()).map(str::to_owned)
                    } else {
                        None
                    },
                    Some(format!("{error:#}")),
                )
            }
        };
    let preview = secret
        .as_ref()
        .map(|(name, value)| {
            format!(
                "{name}={}",
                crate::providers::mask_api_key(value).unwrap_or_else(|| "set".into())
            )
        })
        .unwrap_or_else(|| "missing".into());
    let mut plan = serde_json::json!({
        "model": model,
        "ok": error.is_none(),
        "provider": if azure { "azure" } else { "openai" },
        "providerLabel": provider_label,
        "base": base,
        "keySource": source,
        "keyPreview": preview,
        "keyPresent": secret.is_some(),
        "isAzureOpenAI": azure,
        "azureConfigured": configured,
    });
    if let Some(deployment) = deployment {
        plan["azureDeploymentName"] = serde_json::json!(deployment);
    }
    if let Some(note) = azure_note(mode, configured, azure) {
        plan["azureNote"] = serde_json::json!(note);
    }
    if let Some(error) = error {
        plan["error"] = serde_json::json!(error);
    }
    Ok(plan)
}
/// Reference printProviderPlans, without colour.
pub(crate) fn provider_plans(plans: &[Value], title: &str) -> String {
    let mut lines = vec![title.to_owned(), String::new()];
    for plan in plans {
        let text = |key: &str| plan[key].as_str().unwrap_or("").to_owned();
        lines.push(format!(
            "{}: {}",
            text("model"),
            if plan["ok"] == true {
                "ok"
            } else {
                "not ready"
            }
        ));
        lines.push(format!("  provider: {}", text("providerLabel")));
        let base = text("base");
        lines.push(format!(
            "  base: {}",
            if base.is_empty() { "(none)" } else { &base }
        ));
        lines.push(format!("  key: {}", text("keyPreview")));
        if plan["isAzureOpenAI"] == true || plan.get("azureDeploymentName").is_some() {
            lines.push(format!(
                "  azure deployment: {}",
                plan["azureDeploymentName"].as_str().unwrap_or("none")
            ));
        }
        if let Some(note) = plan["azureNote"].as_str() {
            lines.push(format!("  azure: {note}"));
        }
        if let Some(error) = plan["error"].as_str() {
            lines.push(format!("  error: {error}"));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}
/// Reference printProjectSourcesResult without colour.
pub(crate) fn project_sources(result: &Value) -> String {
    let operation = result["operation"].as_str().unwrap_or("");
    let project = result["projectUrl"].as_str().unwrap_or("");
    let mut lines = vec![];
    if result["status"] == "dry-run" {
        lines.push(format!("Project Sources {operation} dry run"));
        lines.push(format!("Project: {project}"));
        let plan = result["plannedUploads"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !plan.is_empty() {
            lines.push(format!("Planned uploads: {}", plan.len()));
            for upload in plan {
                lines.push(format!(
                    "  batch {}: {}",
                    upload["batch"],
                    upload["displayPath"].as_str().unwrap_or("")
                ));
            }
        }
        return lines.join("\n");
    }
    let count = |key: &str| result[key].as_array().map_or(0, Vec::len);
    lines.push(format!("Project Sources {operation} completed"));
    lines.push(format!("Project: {project}"));
    lines.push(format!("Before: {}", count("sourcesBefore")));
    lines.push(format!("After: {}", count("sourcesAfter")));
    if let Some(added) = result["added"].as_array().filter(|added| !added.is_empty()) {
        let names: Vec<&str> = added
            .iter()
            .map(|source| source["name"].as_str().unwrap_or(""))
            .collect();
        lines.push(format!("Added: {}", names.join(", ")));
    }
    lines.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_numbers_follow_the_reference_format() {
        for (value, expected) in [
            (0.0, "0"),
            (999.0, "999"),
            (1000.0, "1k"),
            (1500.0, "1.5k"),
            (12_345.0, "12.3k"),
            (2_000_000.0, "2m"),
            (2_250_000.0, "2.3m"),
            (f64::NAN, "0"),
        ] {
            assert_eq!(compact_number(value), expected, "{value}");
        }
        assert_eq!(locale_number(1_234_567.0), "1,234,567");
        assert_eq!(locale_number(2.5), "2.5");
    }
    #[test]
    fn suggestions_use_commander_similarity() {
        let candidates = vec!["--prompt".to_owned(), "--model".into(), "--models".into()];
        assert_eq!(
            suggest_similar("--promt", &candidates),
            "\n(Did you mean --prompt?)"
        );
        assert_eq!(
            suggest_similar("--modl", &candidates),
            "\n(Did you mean --model?)"
        );
        assert_eq!(suggest_similar("--zzzzzz", &candidates), "");
    }
    #[test]
    // Each row is what Commander's program parseOptions takes after the command, moved in front.
    fn program_options_after_a_command_move_in_front_like_commander() {
        let missing = |flags: &str| {
            Err(format!(
                "error: option '{flags}' argument missing
(use --help for usage)
"
            ))
        };
        type Case<'a> = (&'a [&'a str], Result<&'a [&'a str], String>);
        let cases: &[Case] = &[
            (&["doctor", "--wait"], Ok(&["--wait", "doctor"])),
            (
                &[
                    "--slug",
                    "PUBLIC words",
                    "doctor",
                    "--json",
                    "-m",
                    "PUBLIC-m",
                ],
                Ok(&[
                    "--slug",
                    "PUBLIC words",
                    "--model=PUBLIC-m",
                    "doctor",
                    "--json",
                ]),
            ),
            // Combined short flags, an attached short value and a long value with '='.
            (
                &[
                    "doctor",
                    "-vmPUBLIC-m",
                    "-vm",
                    "=PUBLIC",
                    "--base-url=http://x",
                ],
                Ok(&[
                    "--verbose",
                    "--model=PUBLIC-m",
                    "--verbose",
                    "--model==PUBLIC",
                    "--base-url=http://x",
                    "doctor",
                ]),
            ),
            // A required value is the next argument even when it looks like an option.
            (
                &["status", "--model", "--hours", "5"],
                Ok(&["--model=--hours", "status", "5"]),
            ),
            // Variadic values run to the next option; negative numbers count as values.
            (
                &[
                    "session",
                    "--file",
                    "PUBLIC-a",
                    "-1.5",
                    "PUBLIC-id",
                    "--hours",
                    "2",
                ],
                Ok(&[
                    "--file=PUBLIC-a",
                    "--file=-1.5",
                    "--file=PUBLIC-id",
                    "session",
                    "--hours",
                    "2",
                ]),
            ),
            // A bare optional value takes clap's missing value so the command is not consumed.
            (
                &["project-sources", "add", "--dry-run", "-f", "PUBLIC-a"],
                Ok(&[
                    "--dry-run=summary",
                    "--file=PUBLIC-a",
                    "project-sources",
                    "add",
                ]),
            ),
            (
                &["project-sources", "add", "--dry-run", "json", "-v"],
                Ok(&["--dry-run=json", "--verbose", "project-sources", "add"]),
            ),
            // Unknown options and the residue of a short group stay for the command.
            (
                &["bridge", "doctor", "--verbose", "-vx", "--bogus", "-v"],
                Ok(&[
                    "--verbose",
                    "--verbose",
                    "--verbose",
                    "bridge",
                    "doctor",
                    "-x",
                    "--bogus",
                ]),
            ),
            (&["session", "--version"], Ok(&["--version", "session"])),
            // After session, --path is the view's boolean (reference scopeSessionPathOption).
            (
                &[
                    "--path",
                    "PUBLIC-a",
                    "--slug",
                    "w",
                    "session",
                    "PUBLIC-id",
                    "--path",
                    "--model",
                    "m",
                ],
                Ok(&[
                    "--path",
                    "PUBLIC-a",
                    "--slug",
                    "w",
                    "--model=m",
                    "session",
                    "PUBLIC-id",
                    "--path",
                ]),
            ),
            (
                &["status", "--path", "PUBLIC-a"],
                Ok(&["--path=PUBLIC-a", "status"]),
            ),
            // Commander's literal ends the program's option parsing.
            (
                &["session", "PUBLIC-id", "--", "--model", "m"],
                Ok(&["session", "PUBLIC-id", "--", "--model", "m"]),
            ),
            (
                &["session", "PUBLIC-id", "--model"],
                missing("-m, --model <model>"),
            ),
            (&["status", "--path"], missing("--path <paths...>")),
            // Root runs, an unknown option before the command and a variadic that takes the
            // command name are left to clap unchanged.
            (
                &["PUBLIC prompt", "--model", "m"],
                Ok(&["PUBLIC prompt", "--model", "m"]),
            ),
            (
                &["--bogus", "doctor", "--wait"],
                Ok(&["--bogus", "doctor", "--wait"]),
            ),
            (
                &["--file", "PUBLIC-a", "doctor", "--wait"],
                Ok(&["--file", "PUBLIC-a", "doctor", "--wait"]),
            ),
            (&["--model"], Ok(&["--model"])),
        ];
        for (args, expected) in cases {
            let argv = std::iter::once("alcuin")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect();
            let expected = expected.clone().map(|tail| {
                std::iter::once("alcuin")
                    .chain(tail.iter().copied())
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            });
            assert_eq!(hoist_root_options(argv), expected, "{args:?}");
        }
    }
}
