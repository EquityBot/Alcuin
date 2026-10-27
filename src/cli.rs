use crate::{
    config::{self, Environment},
    execution, followup, mcp, prompts, providers,
    sessions::{self, SessionStore},
};
use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, ArgMatches, Command, parser::ValueSource};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Every captured top-level command; `run_command` has a dispatch arm for each.
const COMMANDS: &[&str] = &[
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
const HEALTH_FLAGS: &[&str] = &[
    "model",
    "browser-chrome-path",
    "browser-manual-login-profile-dir",
    "browser-max-concurrent-tabs",
    "browser-thinking-time",
    "browser-timeout",
];
const PROJECT_SOURCE_FLAGS: &[&str] = &[
    "chatgpt-url",
    "browser-url",
    "json",
    "verbose",
    "max-file-size-bytes",
    "remote-host",
    "remote-chrome",
    "browser-chrome-path",
    "browser-timeout",
    "browser-input-timeout",
    "browser-max-concurrent-tabs",
    "browser-attach-running",
    "browser-headless",
    "browser-hide-window",
    "browser-keep-browser",
    "browser-manual-login",
    "browser-manual-login-profile-dir",
    "browser-inline-cookies",
    "browser-inline-cookies-file",
    "copy-profile",
    "browser-chrome-profile",
    "browser-cookie-sync",
    "browser-no-cookie-sync",
    "browser-cookie-path",
    "browser-cookie-wait",
    "browser-allow-cookie-errors",
    "browser-profile-lock-timeout",
];

/// The value a bare `[mode]` option takes (`--dry-run`, `--preview`).
pub(crate) const OPTIONAL_VALUE_MISSING: &str = "summary";
fn arg(flags: &str) -> Arg {
    let words: Vec<_> = flags.split_whitespace().collect();
    let long = words
        .iter()
        .find(|s| s.starts_with("--"))
        .expect("reference option has a long flag")
        .trim_end_matches(',')
        .trim_start_matches("--")
        .to_owned();
    let short = words
        .iter()
        .find(|s| s.starts_with('-') && !s.starts_with("--"))
        .and_then(|s| s.chars().nth(1));
    let mut argument = Arg::new(long.clone()).long(long);
    if let Some(value) = words.iter().find(|s| s.starts_with(['<', '['])) {
        argument = argument.value_name(
            value
                .trim_matches(['<', '>', '[', ']'])
                .trim_end_matches("...")
                .to_owned(),
        );
    }
    if let Some(short) = short {
        argument = argument.short(short);
    }
    if flags.contains('<') || flags.contains('[') {
        argument = argument.action(ArgAction::Append);
        if flags.contains("...") {
            argument = argument.num_args(1..);
        } else if flags.contains('[') {
            argument = argument
                .num_args(0..=1)
                .default_missing_value(OPTIONAL_VALUE_MISSING);
        } else {
            argument = argument.num_args(1);
        }
    } else {
        argument = argument.action(ArgAction::SetTrue);
    }
    argument
}
/// A subcommand option with the captured reference help text and visibility.
fn described_arg(option: &Value) -> Arg {
    arg(option["flags"].as_str().expect("reference flags"))
        .help(
            option["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
        .hide(option["hidden"] == true)
}
fn command(spec: &Value) -> Command {
    let raw = spec["command"].as_str().expect("reference command");
    let mut parts = raw.split_whitespace();
    let name = parts.next().expect("reference command name");
    let mut command = Command::new(name.to_owned())
        .about(
            spec["about"]
                .as_str()
                .expect("reference command about")
                .to_owned(),
        )
        .disable_help_subcommand(true);
    for (index, part) in parts.enumerate() {
        let id = part
            .trim_matches(['<', '>', '[', ']'])
            .trim_end_matches("...")
            .to_owned();
        let mut argument = Arg::new(id)
            .index(index + 1)
            .required(part.starts_with('<'));
        if part.contains("...") {
            argument = argument.num_args(1..);
        }
        command = command.arg(argument);
    }
    let mut seen = std::collections::BTreeSet::new();
    for option in spec["options"].as_array().into_iter().flatten() {
        let argument = described_arg(option);
        if seen.insert(argument.get_id().to_string()) {
            command = command.arg(argument);
        }
    }
    for child in spec["children"].as_array().into_iter().flatten() {
        let mut child = self::command(child);
        if name == "project-sources" {
            for option in spec["options"].as_array().into_iter().flatten() {
                let argument = described_arg(option);
                if !child
                    .get_arguments()
                    .any(|existing| existing.get_id() == argument.get_id())
                {
                    child = child.arg(argument);
                }
            }
        }
        command = command.subcommand(child);
    }
    command
}
pub fn parser() -> Command {
    let options: Value = serde_json::from_str(include_str!("cli_spec.json"))
        .expect("captured CLI option definitions");
    let commands: Value = serde_json::from_str(include_str!("cli_commands.json"))
        .expect("captured CLI command definitions");
    let mut root = Command::new("industrious")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_help_subcommand(true)
        .arg(
            Arg::new("exec-window-keeper")
                .long("exec-window-keeper")
                .hide(true)
                .num_args(1)
                .requires("keeper-ready"),
        )
        .arg(
            Arg::new("keeper-ready")
                .long("keeper-ready")
                .hide(true)
                .num_args(1)
                .requires("exec-window-keeper"),
        )
        .arg(Arg::new("positional-prompt").index(1));
    for option in options.as_array().expect("options array") {
        let argument = arg(option["flags"].as_str().expect("option flags"));
        let global = matches!(
            argument.get_id().as_str(),
            "remote-chrome" | "browser-attach-running" | "browser-approval-wait"
        );
        let global = global || HEALTH_FLAGS.contains(&argument.get_id().as_str());
        // Root help is a captured fixture; like the reference, subcommand help omits root options.
        root = root.arg(argument.global(global).hide(global));
    }
    for spec in commands.as_array().expect("command array") {
        root = root.subcommand(command(spec));
    }
    root
}
fn flag(m: &ArgMatches, id: &str) -> bool {
    m.try_get_one::<bool>(id)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}
fn text<'a>(m: &'a ArgMatches, id: &str) -> Option<&'a str> {
    m.try_get_many::<String>(id)
        .ok()
        .flatten()
        .and_then(|mut values| values.next_back())
        .map(String::as_str)
}
fn values(m: &ArgMatches, id: &str) -> Vec<String> {
    m.try_get_many::<String>(id)
        .ok()
        .flatten()
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}
fn specified(m: &ArgMatches, id: &str) -> bool {
    m.try_get_raw(id).is_ok() && m.value_source(id) == Some(ValueSource::CommandLine)
}
fn option_scope<'a>(root: &'a ArgMatches, scopes: &[&'a ArgMatches], id: &str) -> &'a ArgMatches {
    scopes
        .iter()
        .copied()
        .find(|m| specified(m, id))
        .unwrap_or(root)
}
/// Reference parseDurationOption: whole numbers with ms, s, m or h units, compound values
/// allowed, bare integers in milliseconds.
pub fn duration_option(raw: &str, label: &str) -> Result<f64> {
    anyhow::ensure!(
        !crate::policy::trim(raw).is_empty(),
        "{label} must be a duration like 30m, 10s, 500ms, or 2h."
    );
    followup::attachment_timeout(raw)
        .filter(|ms| *ms > 0.0)
        .with_context(|| {
            format!("{label} must be a positive duration like 30m, 10s, 500ms, or 2h.")
        })
}
/// Reference parseTimeoutOption in seconds: "auto" is `Some(None)`, invalid is `None`.
fn timeout_seconds(raw: &str) -> Option<Option<f64>> {
    let value = crate::policy::trim(raw).to_lowercase();
    if value == "auto" {
        return Some(None);
    }
    let decimal = value.split_once('.').map_or(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        |(whole, fraction)| {
            !whole.is_empty()
                && !fraction.is_empty()
                && whole
                    .bytes()
                    .chain(fraction.bytes())
                    .all(|b| b.is_ascii_digit())
        },
    );
    if decimal && let Some(seconds) = value.parse::<f64>().ok().filter(|s| *s > 0.0) {
        return Some(Some(seconds));
    }
    followup::attachment_timeout(&value)
        .filter(|ms| *ms > 0.0)
        .map(|ms| Some(ms / 1000.0))
}
fn output_path(raw: &str, cwd: &Path, env: &Environment, store: &SessionStore) -> Result<PathBuf> {
    if raw == "-" || raw == "/dev/stdout" {
        return Ok(PathBuf::from(raw));
    }
    let path = if let Some(tail) = raw.strip_prefix("~/") {
        PathBuf::from(
            env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .context("Unable to expand output home directory")?,
        )
        .join(tail)
    } else {
        cwd.join(raw)
    };
    let resolved = config::resolve_existing_prefix(&path)?;
    let root = config::resolve_existing_prefix(&store.root())?;
    if resolved.starts_with(&root) {
        bail!(
            "Refusing to write output inside session storage ({}). Choose another path.",
            root.display()
        );
    }
    Ok(path)
}
#[derive(Default)]
pub(crate) struct CliStream {
    printed: AtomicBool,
}
impl CliStream {
    /// Reference runOracle answer tail: separates streamed text, or prints the "Answer:" header
    /// and the published answer when nothing streamed. `answer` is set only after publication.
    /// Progress lines (retry notices among them) were printed as they happened.
    pub(crate) fn finish(&self, answer: Option<&str>, verbose: bool) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        let status =
            (verbose && answer.is_some()).then_some("[verbose] Response status: completed");
        if self.printed.load(Ordering::Relaxed) {
            writeln!(stdout)?;
            if let Some(status) = status {
                writeln!(stdout, "{status}")?;
            }
        } else if let Some(answer) = answer {
            if let Some(status) = status {
                writeln!(stdout, "{status}")?;
            }
            let newline = if answer.ends_with('\n') { "" } else { "\n" };
            write!(stdout, "\nAnswer:\n{answer}{newline}\n")?;
        }
        stdout.flush()?;
        Ok(())
    }
}
impl providers::ResponseObserver for CliStream {
    fn response(&self, _: &Value) -> Result<()> {
        Ok(())
    }
    fn delta(&self, text: &str) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        if !self.printed.load(Ordering::Relaxed) {
            stdout.write_all(b"\nAnswer:\n")?;
        }
        stdout.write_all(text.as_bytes())?;
        stdout.flush()?;
        self.printed.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn log(&self, line: &str) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{line}")?;
        stdout.flush()?;
        Ok(())
    }
}
/// Runs one CLI invocation with its optional performance trace (reference perfTrace.ts).
pub async fn run(args: impl IntoIterator<Item = OsString>) -> Result<i32> {
    // Reference LEGACY_FLAG_ALIASES: literal `--[no-]` spellings from old docs.
    let mut args: Vec<_> = args
        .into_iter()
        .enumerate()
        .map(|(index, arg)| match arg.to_str() {
            Some("--[no-]notify") if index > 0 => "--notify".into(),
            Some("--[no-]notify-sound") if index > 0 => "--notify-sound".into(),
            Some("--[no-]background") if index > 0 => "--background".into(),
            _ => arg,
        })
        .collect();
    // The npx alias serves MCP; its name must never become a prompt (bin/oracle-cli.ts main).
    if args.get(1).is_some_and(|arg| arg == "oracle-mcp") {
        crate::mcp::serve_stdio().await?;
        return Ok(0);
    }
    // Non-UTF-8 arguments are left to the parser; they cannot name a trace.
    if let Some(rest) = args.get(1..).and_then(|rest| {
        rest.iter()
            .map(|a| a.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    }) {
        let (normalised, trace, error) = crate::perf::normalise_args(&rest);
        if let Some(error) = error {
            eprintln!("error: {error}\n(use --help for usage)");
            return Ok(1);
        }
        crate::perf::start(trace, &normalised, &std::env::current_dir()?);
        args.truncate(1);
        args.extend(normalised.into_iter().map(OsString::from));
    }
    let result = run_command(args).await;
    // Reference top-level SIGINT handler; commands return 130 only after an interrupt.
    if matches!(result, Ok(130)) {
        println!("\nCancelled.");
    }
    crate::perf::flush(*result.as_ref().unwrap_or(&1));
    result
}
async fn run_command(args: Vec<OsString>) -> Result<i32> {
    let args = match crate::cli_format::hoist_root_options(args) {
        Ok(args) => args,
        Err(message) => {
            eprint!("{message}");
            return Ok(1);
        }
    };
    let matches = match parser().try_get_matches_from(&args) {
        Ok(matches) => matches,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            if args.iter().any(|a| a == "--help" || a == "-h")
                && !args
                    .iter()
                    .any(|a| a.to_str().is_some_and(|a| COMMANDS.contains(&a)))
            {
                print!("{}", include_str!("../fixtures/cli-help.txt"));
                if args.iter().any(|a| a == "--verbose" || a == "-v") {
                    print!("{}", include_str!("../fixtures/cli-debug-help.txt"));
                }
            } else {
                print!("{error}");
            }
            return Ok(0);
        }
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayVersion => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        Err(error) => {
            match crate::cli_format::commander_error(&error, &args) {
                Some(message) => eprint!("{message}"),
                None => eprint!("{error}"),
            }
            return Ok(1);
        }
    };
    for (id, spelling, label) in [
        ("http-timeout", "--http-timeout <ms|s|m|h>", "HTTP timeout"),
        (
            "zombie-timeout",
            "--zombie-timeout <ms|s|m|h>",
            "Zombie timeout",
        ),
    ] {
        for value in values(&matches, id) {
            if let Err(error) = duration_option(&value, label) {
                eprintln!(
                    "error: option '{spelling}' argument '{value}' is invalid. {error}\n(use --help for usage)"
                );
                return Ok(1);
            }
        }
    }
    if let Some(value) = values(&matches, "timeout")
        .into_iter()
        .find(|value| timeout_seconds(value).is_none())
    {
        eprintln!(
            "error: option '--timeout <seconds|duration|auto>' argument '{value}' is invalid. Timeout must be a positive number of seconds, a duration like \"10m\", or \"auto\".\n(use --help for usage)"
        );
        return Ok(1);
    }
    for (id, spelling, allowed) in [
        ("engine", "-e, --engine <mode>", "api, browser"),
        ("provider", "--provider <provider>", "auto, openai, azure"),
        ("preview", "--preview [mode]", "summary, json, full"),
        ("dry-run", "--dry-run [mode]", "summary, json, full"),
        (
            "reasoning-effort",
            "--reasoning-effort <effort>",
            "none, low, medium, high, xhigh, max",
        ),
        ("reasoning-mode", "--reasoning-mode <mode>", "standard, pro"),
        ("mode", "--mode <mode>", "api, browser"),
        ("partial", "--partial <mode>", "fail, ok"),
        (
            "browser-model-strategy",
            "--browser-model-strategy <mode>",
            "select, current, ignore",
        ),
        (
            "browser-research",
            "--browser-research <mode>",
            "off, search, deep",
        ),
        (
            "browser-archive",
            "--browser-archive <mode>",
            "auto, always, never",
        ),
        (
            "browser-attachments",
            "--browser-attachments <mode>",
            "auto, never, always",
        ),
        (
            "browser-bundle-format",
            "--browser-bundle-format <format>",
            "auto, text, zip",
        ),
    ] {
        if let Some(value) = text(&matches, id)
            && !allowed.split(", ").any(|allowed| allowed == value)
        {
            eprintln!(
                "error: option '{spelling}' argument '{value}' is invalid. Allowed choices are {allowed}.\n(use --help for usage)"
            );
            return Ok(1);
        }
    }
    if let Some(value) = text(&matches, "search")
        && !matches!(
            crate::policy::trim(value).to_lowercase().as_str(),
            "on" | "off" | "true" | "false" | "1" | "0" | "yes" | "no"
        )
    {
        eprintln!(
            "error: option '--search <mode>' argument '{value}' is invalid. Search mode must be \"on\" or \"off\".\n(use --help for usage)"
        );
        return Ok(1);
    }
    if let Some(name) = matches.subcommand_name()
        && matches!(name, "perf-summary" | "browser-health")
    {
        crate::perf::suppress();
    }
    crate::perf::pre_action(matches.subcommand_name().unwrap_or("root"));
    if flag(&matches, "debug-help") {
        print!("{}", include_str!("../fixtures/cli-debug-help.txt"));
        return Ok(0);
    }
    if let Some(identity) = text(&matches, "exec-window-keeper") {
        internal_entry(&matches, "exec-window-keeper", &["keeper-ready"])?;
        crate::window_keeper::run(
            &serde_json::from_str(identity)?,
            std::path::Path::new(
                text(&matches, "keeper-ready").context("Keeper readiness path missing")?,
            ),
        )?;
        return Ok(0);
    }
    let env = config::runtime_environment();
    let home = config::oracle_home(&env)?;
    let store = SessionStore::new(home);
    let cwd = std::env::current_dir()?;
    if let Some(id) = text(&matches, "exec-session") {
        internal_entry(&matches, "exec-session", &[])?;
        execution::worker(&store, id, &env).await?;
        return Ok(0);
    }
    if let Some((name, sub)) = matches.subcommand() {
        let session_view = matches!(name, "session" | "status");
        let ignored = if session_view {
            ignored_root_flags(&matches)?
        } else {
            vec![]
        };
        // Session views report or ignore root flags themselves.
        if !session_view {
            require_flags(
                &matches,
                if name == "project-sources" {
                    PROJECT_SOURCE_FLAGS
                } else if name == "serve" {
                    &[
                        "browser-attach-running",
                        "remote-chrome",
                        "browser-approval-wait",
                        "browser-cookie-sync",
                    ]
                } else if name == "browser-health" {
                    HEALTH_FLAGS
                } else {
                    &[]
                },
            )?;
        }
        match name {
            "browser-health" => {
                let mut allowed = HEALTH_FLAGS.to_vec();
                allowed.push("json");
                require_flags(sub, &allowed)?;
                let loaded = load_config(&store, &cwd, &env, false);
                let mut input = json!({});
                for (source, target) in [
                    ("browser-chrome-path", "chromePath"),
                    ("browser-manual-login-profile-dir", "manualLoginProfileDir"),
                    ("browser-max-concurrent-tabs", "maxConcurrentTabs"),
                    ("browser-thinking-time", "thinkingTime"),
                ] {
                    if let Some(value) = text(&matches, source) {
                        input[target] = json!(value);
                    } else if let Some(value) = loaded.config["browser"].get(target) {
                        input[target] = value.clone();
                    }
                }
                input["desiredModel"] = json!(
                    text(&matches, "model")
                        .or(loaded.config["model"].as_str())
                        .unwrap_or("Latest")
                );
                let timeout = text(&matches, "browser-timeout")
                    .map(|s| duration_option(s, "Browser health timeout"))
                    .transpose()?
                    .unwrap_or(30_000.0);
                anyhow::ensure!(
                    timeout.is_finite()
                        && timeout.fract() == 0.0
                        && (1.0..=300_000.0).contains(&timeout),
                    "Browser health timeout must be between 1ms and 5m."
                );
                let cancel = CancellationToken::new();
                let signal_cancel = cancel.clone();
                let signal = tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        signal_cancel.cancel();
                    }
                });
                let result = crate::browser_health::inspect(
                    &input,
                    &env,
                    &cwd,
                    Duration::from_millis(timeout as u64),
                    &cancel,
                )
                .await;
                signal.abort();
                if cancel.is_cancelled() {
                    return Ok(130);
                }
                let result = result?;
                if flag(sub, "json") {
                    println!("{}", serde_json::to_string(&result)?);
                } else {
                    println!("{}", crate::browser_health::format(&result));
                }
                return Ok(
                    if result["status"] == "ready" && result["cleanupComplete"] == true {
                        0
                    } else {
                        1
                    },
                );
            }
            "bridge" => {
                require_flags(sub, &[])?;
                let (operation, child) = sub.subcommand().context("bridge requires a command")?;
                let allowed: &[&str] = match operation {
                    "host" => &[
                        "bind",
                        "token",
                        "write-connection",
                        "ssh",
                        "ssh-remote-port",
                        "ssh-identity",
                        "ssh-extra-args",
                        "background",
                        "foreground",
                        "respawn",
                        "print",
                        "print-token",
                    ],
                    "doctor" => &["verbose"],
                    "client" => &[
                        "connect",
                        "config",
                        "no-write-config",
                        "no-test",
                        "print-env",
                    ],
                    "codex-config" | "claude-config" => &[
                        "print-token",
                        "local-browser",
                        "oracle-home-dir",
                        "browser-profile-dir",
                    ],
                    _ => bail!("Bridge {operation} is not yet qualified in the Rust candidate"),
                };
                require_flags(child, allowed)?;
                let mut options = json!({});
                for key in allowed {
                    if let Some(value) = text(child, key) {
                        options[*key] = json!(value);
                    } else if flag(child, key) {
                        options[*key] = json!(true);
                    }
                }
                // Reference runBridgeHost reads optsWithGlobals, so a program --background applies.
                if operation == "host" && flag(&matches, "background") {
                    options["background"] = json!(true);
                }
                if operation == "host" {
                    crate::bridge_host::run(&options, store, cwd, env, &CancellationToken::new())
                        .await?;
                    return Ok(0);
                }
                let mut exit_code = 0;
                let output = if operation == "client" {
                    crate::bridge::configure_client(
                        &options,
                        &store,
                        &cwd,
                        &CancellationToken::new(),
                    )
                    .await?
                } else {
                    let loaded = load_config(&store, &cwd, &env, true);
                    if operation == "doctor" {
                        let (output, code) =
                            crate::bridge::doctor(&loaded, &store, &env, &CancellationToken::new())
                                .await?;
                        exit_code = code;
                        output
                    } else {
                        crate::bridge::mcp_snippet(operation, &options, &loaded.config, &env)?
                    }
                };
                println!("{output}");
                if matches!(operation, "codex-config" | "claude-config")
                    && options["print-token"] != true
                    && options["local-browser"] != true
                {
                    eprintln!();
                    eprintln!(
                        "Tip: rerun with --print-token to include ORACLE_REMOTE_TOKEN in the snippet."
                    );
                }
                return Ok(exit_code);
            }
            "serve" => {
                require_flags(
                    sub,
                    &[
                        "host",
                        "port",
                        "token",
                        "max-concurrent-runs",
                        "max-queued-runs",
                        "manual-login",
                        "manual-login-profile-dir",
                        "browser-cookie-sync",
                        "remote-chrome",
                        "browser-attach-running",
                        "browser-approval-wait",
                    ],
                )?;
                let count = |name: &str| -> Result<Option<usize>> {
                    text(sub, name)
                        .map(|raw| {
                            let number = raw
                                .trim()
                                .parse::<f64>()
                                .with_context(|| format!("--{name} must be an integer"))?;
                            anyhow::ensure!(
                                number.is_finite()
                                    && number.fract() == 0.0
                                    && (0.0..=9_007_199_254_740_991.0).contains(&number),
                                "--{name} must be a nonnegative safe integer"
                            );
                            usize::try_from(number as u64).with_context(|| {
                                format!("--{name} exceeds the native integer range")
                            })
                        })
                        .transpose()
                };
                let concurrent = count("max-concurrent-runs")?;
                let queued = count("max-queued-runs")?;
                anyhow::ensure!(
                    concurrent.is_some() || queued.is_none(),
                    "--max-queued-runs requires --max-concurrent-runs."
                );
                anyhow::ensure!(
                    concurrent != Some(0),
                    "--max-concurrent-runs must be a positive integer."
                );
                let port = text(sub, "port")
                    .unwrap_or("0")
                    .parse::<u16>()
                    .context("Service port must be between 0 and 65535")?;
                let address =
                    tokio::net::lookup_host((text(sub, "host").unwrap_or("0.0.0.0"), port))
                        .await?
                        .next()
                        .context("Service bind host did not resolve")?;
                let loaded = load_config(&store, &cwd, &env, true);
                let mut flags = json!({"manualLogin":flag(sub,"manual-login"),"cookieSync":flag(sub,"browser-cookie-sync") || flag(&matches,"browser-cookie-sync")});
                for (source, target) in [
                    ("remote-chrome", "remoteChrome"),
                    ("browser-approval-wait", "approvalWait"),
                ] {
                    if let Some(value) = text(&matches, source) {
                        flags[target] = json!(value);
                    }
                }
                if specified(&matches, "browser-attach-running") {
                    flags["attachRunning"] = json!(flag(&matches, "browser-attach-running"));
                }
                if let Some(path) = text(sub, "manual-login-profile-dir") {
                    flags["manualLoginProfileDir"] = json!(path);
                }
                let browser =
                    crate::remote_service::browser_config(&loaded.config, &flags, &env, &cwd)?;
                // Reference resolveBrowserConfig: config, then a digits-only environment cap,
                // normalised like any browser tab limit.
                let configured = &loaded.config["browser"]["maxConcurrentTabs"];
                let cap = crate::browser_health::tab_limit(&if configured.is_null() {
                    browser_capacity_override(configured, &env).unwrap_or(Value::Null)
                } else {
                    configured.clone()
                });
                let token = text(sub, "token")
                    .map(str::to_owned)
                    .unwrap_or_else(crate::remote_service::access_token);
                let options = crate::remote_host::HostOptions {
                    address,
                    token,
                    max_concurrent_runs: concurrent,
                    max_queued_runs: queued,
                    max_browser_tabs: cap,
                };
                Box::pin(crate::remote_service::serve(
                    store,
                    options,
                    browser,
                    env,
                    cwd,
                    &CancellationToken::new(),
                ))
                .await?;
                return Ok(0);
            }
            "docs" => {
                require_flags(sub, &[])?;
                let (operation, child) = sub.subcommand().context("docs requires check")?;
                anyhow::ensure!(operation == "check", "Unsupported docs command");
                require_flags(child, &["docs-path", "json"])?;
                let result = crate::docs_check::check(&cwd, &values(child, "docs-path"))?;
                if flag(child, "json") {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("{}", crate::docs_check::format_result(&result));
                }
                return Ok(if result.issues.is_empty() { 0 } else { 1 });
            }
            "perf-summary" => {
                require_flags(sub, &["traces", "json"])?;
                let files = sub
                    .get_many::<String>("traces")
                    .map(|v| v.map(PathBuf::from).collect::<Vec<_>>())
                    .unwrap_or_default();
                let summary = crate::perf::summarise(&files)?;
                if flag(sub, "json") {
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    println!("{}", crate::perf::format_summary(&summary));
                }
                return Ok(0);
            }
            "session" | "status" => {
                require_flags(sub, SESSION_FLAGS)?;
                return session_command(&store, &matches, sub, name, &env, &ignored).await;
            }
            "restart" => {
                require_flags(
                    sub,
                    &["id", "wait", "no-wait", "remote-host", "remote-token"],
                )?;
                anyhow::ensure!(
                    !(flag(sub, "wait") && flag(sub, "no-wait")),
                    "Cannot combine --wait and --no-wait."
                );
                let cancel = CancellationToken::new();
                // Restart can run its browser consultation inline.
                let signal = crate::browser_signal::cancel_on_ctrl_c(cancel.clone());
                let result = crate::restart::run(
                    &store,
                    crate::restart::Options {
                        source: text(sub, "id").context("restart requires a session ID")?,
                        // Reference optsWithGlobals: program --wait, --no-wait, --remote-host
                        // and --remote-token apply; the later restart value wins.
                        wait: [sub, &matches].into_iter().find_map(|m| {
                            if specified(m, "wait") {
                                Some(flag(m, "wait"))
                            } else {
                                flag(m, "no-wait").then_some(false)
                            }
                        }),
                        remote_host: text(sub, "remote-host").or(text(&matches, "remote-host")),
                        remote_token: text(sub, "remote-token").or(text(&matches, "remote-token")),
                    },
                    &env,
                    &cwd,
                    &cancel,
                )
                .await;
                signal.abort();
                let _ = signal.await;
                return result.map_err(|error| anyhow::anyhow!("{error:#}"));
            }
            "recover-native" => {
                require_flags(sub, &["request", "write-output"])?;
                let request = text(sub, "request").context(
                    "recover-native requires --request <path> and --write-output <path>",
                )?;
                // Reference optsWithGlobals: a program --write-output applies.
                let output = text(
                    option_scope(&matches, &[sub], "write-output"),
                    "write-output",
                )
                .context("recover-native requires --request <path> and --write-output <path>")?;
                let cancel = CancellationToken::new();
                let signal_cancel = cancel.clone();
                let signal = tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        signal_cancel.cancel();
                    }
                });
                let result = crate::native_reply::recover(
                    &store,
                    &env,
                    &cwd,
                    Path::new(request),
                    Path::new(output),
                    &cancel,
                )
                .await;
                signal.abort();
                let _ = signal.await;
                if cancel.is_cancelled() {
                    return Ok(130);
                }
                let count = result?;
                let mut stdout = std::io::stdout().lock();
                writeln!(
                    stdout,
                    "Recovered completed reply ({} characters).",
                    grouped(count)
                )?;
                stdout.flush()?;
                return Ok(0);
            }
            "recover-remote" => {
                require_flags(sub, &["run-id", "host", "token"])?;
                let host =
                    text(sub, "host").context("recover-remote requires --host <host:port>")?;
                let connection = crate::remote_session::Connection {
                    host: host.into(),
                    token: text(sub, "token").map(str::to_owned),
                };
                let run_id = text(sub, "run-id").context("Missing remote recovery run ID")?;
                let (id, result) = crate::remote_session::recover(
                    &store,
                    &cwd,
                    &connection,
                    run_id,
                    &CancellationToken::new(),
                )
                .await?;
                println!("Recovered session: {id}");
                println!(
                    "{}",
                    result["answerMarkdown"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .or(result["answerText"].as_str())
                        .unwrap_or("")
                );
                return Ok(0);
            }
            "doctor" => return doctor(&store, &matches, sub, &env, &cwd),
            "project-sources" => {
                require_flags(&matches, PROJECT_SOURCE_FLAGS)?;
                require_flags(sub, PROJECT_SOURCE_FLAGS)?;
                let (operation, child) = sub
                    .subcommand()
                    .context("project-sources requires list or add")?;
                let mut child_flags = PROJECT_SOURCE_FLAGS.to_vec();
                child_flags.extend(["file", "dry-run"]);
                require_flags(child, &child_flags)?;
                // Reference optsWithGlobals: program file inputs merge like mergePathLikeOptions
                // and a program --dry-run applies.
                let mut files = values(&matches, "file");
                files.extend(values(child, "file"));
                for key in ["include", "files", "path", "paths"] {
                    files.extend(values(&matches, key));
                }
                let dry_run = flag(child, "dry-run") || specified(&matches, "dry-run");
                let mut input = json!({"operation":operation,"dryRun":dry_run,"confirmMutation":operation=="add","files":prompts::collect_paths(&files)});
                for (flag, key) in [
                    ("browser-inline-cookies", "browserInlineCookies"),
                    ("browser-inline-cookies-file", "browserInlineCookiesFile"),
                ] {
                    if let Some(value) = text(option_scope(&matches, &[child, sub], flag), flag) {
                        input[key] = json!(value);
                    }
                }
                if let Some(url) = text(child, "chatgpt-url")
                    .or_else(|| text(sub, "chatgpt-url"))
                    .or_else(|| text(&matches, "chatgpt-url"))
                    .or_else(|| text(&matches, "browser-url"))
                {
                    input["chatgptUrl"] = json!(url);
                }
                anyhow::ensure!(
                    operation != "add" || input["files"].as_array().is_some_and(|a| !a.is_empty()),
                    "project-sources add requires at least one --file."
                );
                let server = mcp::IndustriousMcp { store, cwd, env };
                let mut overrides = browser_runtime_overrides_at_scope(
                    &matches,
                    &[child, sub],
                    &server.configured()["browser"],
                    &server.env,
                )?;
                for (flag, key) in [
                    ("browser-headless", "headless"),
                    ("browser-hide-window", "hideWindow"),
                    ("browser-keep-browser", "keepBrowser"),
                    ("browser-manual-login", "manualLogin"),
                ] {
                    let scope = option_scope(&matches, &[child, sub], flag);
                    if specified(scope, flag) {
                        overrides[key] = json!(self::flag(scope, flag));
                    }
                }
                sources_cookie_options(
                    &matches,
                    &[child, sub],
                    &mut input,
                    &mut overrides,
                    &server.env,
                );
                for (flag, key) in [
                    ("browser-manual-login-profile-dir", "manualLoginProfileDir"),
                    ("remote-host", "remoteHost"),
                ] {
                    if let Some(value) = text(option_scope(&matches, &[child, sub], flag), flag) {
                        overrides[key] = json!(value);
                    }
                }
                if let Some(raw) = text(
                    option_scope(&matches, &[child, sub], "max-file-size-bytes"),
                    "max-file-size-bytes",
                ) {
                    input["maxFileSizeBytes"] = json!(
                        raw.parse::<u64>()
                            .context("max-file-size-bytes must be a positive integer")?
                    );
                    anyhow::ensure!(
                        input["maxFileSizeBytes"].as_u64() != Some(0),
                        "max-file-size-bytes must be positive"
                    );
                }
                let cancel = CancellationToken::new();
                let signal = crate::browser_signal::cancel_on_ctrl_c(cancel.clone());
                crate::browser_signal::install();
                // Boxed: browser futures must not enlarge the stack-pinned CLI future.
                let result = Box::pin(server.project_sources(&input, &overrides, &cancel)).await;
                signal.abort();
                let _ = signal.await;
                if cancel.is_cancelled() {
                    return Ok(130);
                }
                let result = result?.structured_content.unwrap_or_default();
                if flag(option_scope(&matches, &[child, sub], "json"), "json") {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("{}", crate::cli_format::project_sources(&result));
                }
                return Ok(0);
            }
            "tui" => bail!(
                "industrious tui is not supported in this build: the terminal UI is not included. Use `industrious status` and `industrious session <id>`."
            ),
            // Clap yields only the captured commands, and every one in COMMANDS has an arm.
            _ => bail!("Unknown command {name}."),
        }
    }
    crate::perf::mark("root-command-start", None);
    anyhow::ensure!(
        env.get("ORACLE_FORCE_TUI").map(String::as_str) != Some("1"),
        "ORACLE_FORCE_TUI=1 requests the terminal UI, which is not supported in this build. Unset it to run a consultation."
    );
    anyhow::ensure!(
        !(text(&matches, "models")
            .is_some_and(|models| models.split(',').any(|m| !m.trim().is_empty()))
            && specified(&matches, "model")
            && text(&matches, "model").is_some_and(|model| !crate::policy::trim(model).is_empty())),
        "--models cannot be combined with --model."
    );
    // Reference root --status and --session aliases ignore the other root flags silently.
    if flag(&matches, "status") {
        ignored_root_flags(&matches)?;
        return session_command(&store, &matches, &matches, "status", &env, &[]).await;
    }
    if let Some(id) = text(&matches, "session") {
        ignored_root_flags(&matches)?;
        return attach(&store, id, &attach_view(&matches, &matches), &env).await;
    }
    let mut prompt = text(&matches, "prompt")
        .filter(|s| !s.is_empty())
        .or_else(|| text(&matches, "message"))
        .or_else(|| {
            matches
                .get_one::<String>("positional-prompt")
                .map(String::as_str)
        })
        .map(str::to_owned);
    if prompt.as_deref() == Some("-") {
        anyhow::ensure!(
            !std::io::stdin().is_terminal(),
            "\"-p -\" requires piped input, for example: echo \"prompt\" | industrious -p -."
        );
        let mut stdin = String::new();
        std::io::stdin().read_to_string(&mut stdin)?;
        anyhow::ensure!(
            !crate::policy::trim(&stdin).is_empty(),
            "\"-p -\" received empty stdin."
        );
        prompt = Some(crate::policy::trim(&stdin).to_owned());
    }
    if no_routing_arguments(&args) {
        // Reference zero-argument entry; its `industrious tui` pointer names an excluded command.
        println!("No prompt or subcommand supplied. Run `industrious --help` for usage.");
        print!("{}", include_str!("../fixtures/cli-help.txt"));
        return Ok(0);
    }
    // Reference shouldRequirePrompt: route checks bypass the prompt unless a preview or
    // Markdown render is also requested.
    if prompt.is_none()
        && (flag(&matches, "render-markdown")
            || specified(&matches, "preview")
            || specified(&matches, "dry-run")
            || !(flag(&matches, "route") || flag(&matches, "preflight")))
    {
        println!("Prompt is required. Provide it via --prompt \"<text>\" or positional [prompt].");
        eprint!("{}", include_str!("../fixtures/cli-help.txt"));
        return Ok(1);
    }
    refuse_unimplemented_flags(&matches)?;
    let render = flag(&matches, "render") || flag(&matches, "render-markdown");
    anyhow::ensure!(
        !specified(&matches, "dry-run") || !render,
        "--dry-run cannot be combined with --render-markdown."
    );
    anyhow::ensure!(
        !(flag(&matches, "no-azure") && text(&matches, "provider") == Some("azure")),
        "--provider azure cannot be combined with --no-azure."
    );
    if flag(&matches, "no-azure") || text(&matches, "provider") == Some("openai") {
        anyhow::ensure!(
            !["azure-endpoint", "azure-deployment", "azure-api-version"]
                .iter()
                .any(|key| specified(&matches, key)),
            "--provider openai/--no-azure cannot be combined with Azure options."
        );
    }
    let loaded = load_config(&store, &cwd, &env, true);
    refuse_excluded_config(&loaded.config)?;
    // The reference hidden alias appends include to file before the general path merge adds
    // include again, so its duplicate warning is part of the CLI output contract.
    let mut files = values(&matches, "file");
    files.extend(values(&matches, "include"));
    for key in ["include", "files", "path", "paths"] {
        files.extend(values(&matches, key));
    }
    let collected = prompts::collect_paths(&files);
    let mut deduped = vec![];
    let mut duplicates = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for file in collected {
        let key = if file.starts_with('!') || file.contains(['*', '?', '[', '{', '(']) {
            file.clone()
        } else {
            prompts::absolute(&cwd, &file)
                .to_string_lossy()
                .into_owned()
        };
        if seen.insert(key) {
            deduped.push(file);
        } else {
            duplicates.push(file);
        }
    }
    if !duplicates.is_empty() {
        println!(
            "Ignoring duplicate --file inputs: {}{}",
            duplicates
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if duplicates.len() > 8 {
                format!(" (+{} more)", duplicates.len() - 8)
            } else {
                String::new()
            }
        );
    }
    let mut input = json!({"prompt":prompt.unwrap_or_else(||"Provider route inspection".into()),"files":deduped});
    for (key, target) in [
        ("model", "model"),
        ("engine", "engine"),
        ("slug", "slug"),
        ("reasoning-effort", "reasoningEffort"),
        ("reasoning-mode", "reasoningMode"),
        ("base-url", "baseUrl"),
        ("provider", "provider"),
        ("generate-image", "generateImage"),
        ("output", "outputPath"),
        ("browser-thinking-time", "browserThinkingTime"),
        ("browser-inline-cookies", "browserInlineCookies"),
        ("browser-inline-cookies-file", "browserInlineCookiesFile"),
        ("copy-profile", "copyProfile"),
        ("browser-chrome-profile", "browserChromeProfile"),
        ("browser-cookie-path", "browserCookiePath"),
        ("browser-cookie-names", "browserCookieNames"),
        ("browser-cookie-wait", "browserCookieWait"),
        ("browser-model-strategy", "browserModelStrategy"),
        ("browser-research", "browserResearchMode"),
        ("browser-archive", "browserArchive"),
        ("browser-fallback", "browserFallback"),
        ("browser-attachments", "browserAttachments"),
        ("browser-bundle-format", "browserBundleFormat"),
        // Gemini web options: saved as in the reference, which ignores them for GPT-6.
        ("youtube", "youtube"),
        ("edit-image", "editImage"),
        ("aspect", "aspectRatio"),
        (
            "browser-manual-login-profile-dir",
            "browserManualLoginProfileDir",
        ),
    ] {
        if let Some(value) = text(&matches, key) {
            input[target] = json!(value);
        }
    }
    if flag(&matches, "browser") {
        input["engine"] = json!("browser");
    } else if input.get("engine").is_none() {
        if let Some(mode) = text(&matches, "mode") {
            input["engine"] = json!(mode);
        } else if flag(&matches, "no-azure")
            || text(&matches, "provider").is_some_and(|provider| provider != "auto")
            || ["azure-endpoint", "azure-deployment", "azure-api-version"]
                .iter()
                .any(|key| specified(&matches, key))
        {
            input["engine"] = json!("api");
        }
    }
    if flag(&matches, "no-azure") {
        input["provider"] = json!("openai");
    }
    for (key, target) in [
        ("azure-endpoint", "endpoint"),
        ("azure-deployment", "deployment"),
        ("azure-api-version", "apiVersion"),
    ] {
        if let Some(value) = text(&matches, key) {
            if !input["azure"].is_object() {
                input["azure"] = json!({});
            }
            input["azure"][target] = json!(value);
        }
    }
    if let Some(search) = text(&matches, "search") {
        input["search"] = json!(match search.trim().to_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            _ => bail!("Search mode must be \"on\" or \"off\"."),
        });
    }
    for (key, target) in [
        ("max-file-size-bytes", "maxFileSizeBytes"),
        ("max-input", "maxInput"),
        ("max-output", "maxOutput"),
    ] {
        if let Some(value) = text(&matches, key) {
            let n: u64 = value
                .parse()
                .with_context(|| format!("{key} must be a positive integer"))?;
            anyhow::ensure!(n > 0, "{key} must be positive");
            input[target] = json!(n);
        }
    }
    if let Some(value) = text(&matches, "http-timeout") {
        input["httpTimeoutMs"] = json!(duration_option(value, "HTTP timeout")?);
    }
    let timeout = text(&matches, "timeout")
        .and_then(timeout_seconds)
        .flatten();
    if let Some(seconds) = timeout {
        input["timeoutSeconds"] = json!(seconds);
    }
    for (key, target) in [
        ("background", "background"),
        ("browser-bundle-files", "browserBundleFiles"),
        ("browser-keep-browser", "browserKeepBrowser"),
        ("browser-headless", "browserHeadless"),
        ("browser-hide-window", "browserHideWindow"),
        ("browser-manual-login", "browserManualLogin"),
        ("browser-cookie-sync", "browserCookieSync"),
        ("browser-no-cookie-sync", "browserNoCookieSync"),
        ("browser-allow-cookie-errors", "browserAllowCookieErrors"),
        ("force", "force"),
    ] {
        if specified(&matches, key) {
            input[target] = json!(flag(&matches, key));
        }
    }
    if flag(&matches, "no-background") {
        input["background"] = json!(false);
    }
    if flag(&matches, "browser-inline-files") {
        input["browserAttachments"] = json!("never");
    }
    if specified(&matches, "browser-follow-up") {
        input["browserFollowUps"] = json!(values(&matches, "browser-follow-up"));
    }
    let copy_requested = flag(&matches, "copy-markdown") || flag(&matches, "copy");
    if render || copy_requested {
        input["renderOnly"] = json!(true);
    }
    let preview = text(&matches, "dry-run").or_else(|| text(&matches, "preview"));
    if let Some(models) = text(&matches, "models") {
        input["models"] = json!(
            models
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        );
    }
    if flag(&matches, "wait") && flag(&matches, "no-wait") {
        bail!("Cannot combine --wait and --no-wait.");
    }
    let mut browser_followup = None;
    if let Some(reference) = text(&matches, "followup") {
        anyhow::ensure!(
            !input["models"]
                .as_array()
                .is_some_and(|models| !models.is_empty()),
            "--followup cannot be combined with --models."
        );
        match followup::resolve(&store, reference, text(&matches, "followup-model"))? {
            followup::Followup::Api {
                response_id,
                session_id,
            } => {
                input["previousResponseId"] = json!(response_id);
                if let Some(session_id) = session_id {
                    input["followupSessionId"] = json!(session_id);
                }
                if let Some(model) = text(&matches, "followup-model") {
                    input["followupModel"] = json!(model);
                }
            }
            followup::Followup::Browser {
                session_id,
                model,
                config,
            } => {
                input["engine"] = json!("browser");
                input["model"] = json!(model);
                input["followupSessionId"] = json!(session_id);
                input["browserResumeConversationUrl"] = config["resumeConversationUrl"].clone();
                browser_followup = Some(config);
            }
        }
    }
    if let Some(path) =
        text(&matches, "write-output").filter(|path| !crate::policy::trim(path).is_empty())
    {
        let path = output_path(path, &cwd, &env, &store)?;
        input["writeOutputPath"] = json!(path);
    }
    if flag(&matches, "write-artifacts") {
        input["writeArtifacts"] = json!(true);
    }
    if flag(&matches, "files-report") {
        input["filesReport"] = json!(true);
    }
    if flag(&matches, "verbose") {
        input["verbose"] = json!(true);
    }
    // Reference: the zombie cutoff follows an explicit --timeout unless set itself.
    if let Some(value) = text(&matches, "zombie-timeout") {
        input["zombieTimeoutMs"] = json!(duration_option(value, "Zombie timeout")?);
    } else if let Some(seconds) = timeout {
        input["zombieTimeoutMs"] = json!(seconds * 1000.0);
    }
    // Reference multi-model failure policy; Industrious's GPT-6 policy leaves one model, so it is
    // saved but never changes an outcome.
    if flag(&matches, "allow-partial") {
        input["partialMode"] = json!("ok");
    } else if let Some(mode) = text(&matches, "partial") {
        anyhow::ensure!(
            matches!(mode, "fail" | "ok"),
            "option '--partial <mode>' argument '{mode}' is invalid. Allowed choices are fail, ok."
        );
        input["partialMode"] = json!(mode);
    }
    if flag(&matches, "zombie-last-activity") {
        input["zombieUseLastActivity"] = json!(true);
    }
    notification_flags(&matches, &mut input);
    if flag(&matches, "gemini-show-thoughts") {
        input["geminiShowThoughts"] = json!(true);
    }
    if flag(&matches, "no-gemini-fallback") {
        input["geminiAllowModelFallback"] = json!(false);
    }
    if let Some(value) = text(&matches, "heartbeat") {
        input["heartbeatSeconds"] = json!(heartbeat_seconds(value)?);
    }
    let mut remote_config = loaded.config["browser"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));
    for (flag, key) in [
        ("remote-host", "remoteHost"),
        ("remote-token", "remoteToken"),
    ] {
        if let Some(value) = text(&matches, flag)
            .map(crate::policy::trim)
            .filter(|value| !value.is_empty())
        {
            remote_config[key] = json!(value);
        }
    }
    let remote = crate::remote_session::Connection::resolve(&remote_config, &env);
    if let Some(connection) = &remote {
        println!("Remote browser host detected: {}", connection.host);
        input["remoteBrowser"] = json!(true);
    }
    let mut request = execution::resolve(&input, &loaded.config, &env, &cwd)?;
    if flag(&matches, "write-artifacts") {
        anyhow::ensure!(
            request.engine == "browser",
            "--write-artifacts requires --engine browser."
        );
        anyhow::ensure!(
            request.options["writeOutputPath"]
                .as_str()
                .is_some_and(|path| !path.is_empty()),
            "--write-artifacts requires --write-output <path>."
        );
    }
    if request.engine != "api" {
        for key in ["reasoning-mode", "reasoning-effort"] {
            anyhow::ensure!(!specified(&matches, key), "--{key} requires --engine api.");
        }
    }
    // Reference: ORACLE_BROWSER_ATTACHMENT_TIMEOUT stands in for the flag on fresh runs only.
    let attachment_env = env
        .get("ORACLE_BROWSER_ATTACHMENT_TIMEOUT")
        .map(|raw| crate::policy::trim(raw).to_owned())
        .filter(|raw| !raw.is_empty() && browser_followup.is_none());
    let attachment_timeout = if request.engine == "browser"
        && (specified(&matches, "browser-attachment-timeout") || attachment_env.is_some())
    {
        let raw = text(&matches, "browser-attachment-timeout")
            .filter(|_| specified(&matches, "browser-attachment-timeout"))
            .or(attachment_env.as_deref())
            .context("Missing browser attachment timeout")?;
        let milliseconds=followup::attachment_timeout(raw).unwrap_or_else(||{println!("Warning: invalid --browser-attachment-timeout duration \"{raw}\"; using fallback 45000ms.");45_000.0});
        Some(if milliseconds < u64::MAX as f64 {
            json!(milliseconds as u64)
        } else {
            json!(milliseconds)
        })
    } else {
        None
    };
    anyhow::ensure!(
        request.engine == "browser" || text(&matches, "browser-tab").is_none(),
        "--browser-tab requires --engine browser."
    );
    if request.engine == "browser" && browser_followup.is_none() {
        let overrides = browser_runtime_overrides(&matches, &request.browser, &env)?;
        request.browser = config::deep_merge(&request.browser, &overrides);
        request.browser_preview = config::deep_merge(&request.browser_preview, &overrides);
        request.options["browserConfig"] = request.browser.clone();
    }
    if let Some(mut saved) = browser_followup {
        if let Some(capacity) = browser_capacity_override(&saved, &env) {
            saved["maxConcurrentTabs"] = capacity;
        }
        if let Some(timeout) = attachment_timeout {
            saved["attachmentTimeoutMs"] = timeout;
        }
        request.replace_browser_config(saved, &input, &env, &cwd)?;
    } else if let Some(timeout) = attachment_timeout {
        request.browser["attachmentTimeoutMs"] = timeout.clone();
        request.browser_preview["attachmentTimeoutMs"] = timeout;
        request.options["browserConfig"] = request.browser.clone();
    }
    anyhow::ensure!(
        remote.is_none() || request.browser["copyProfileSource"].is_null(),
        "--copy-profile cannot be sent to a remote browser service"
    );
    anyhow::ensure!(
        remote.is_none() || request.cookie_bootstrap.is_none(),
        "Explicit cookie bootstrap cannot be sent to a remote browser service"
    );
    anyhow::ensure!(
        remote.is_none() || request.engine == "browser",
        "--remote-host requires --engine browser."
    );
    anyhow::ensure!(
        remote.is_none() || text(&matches, "remote-chrome").is_none(),
        "--remote-host cannot be combined with --remote-chrome."
    );
    if input.get("previousResponseId").is_some() {
        followup::require_api(
            &request.engine,
            request.options["model"]
                .as_str()
                .context("Follow-up child has no model")?,
            &request.options,
            &env,
        )?;
    }
    if flag(&matches, "route") || flag(&matches, "preflight") {
        let mut options = request.options.clone();
        match providers::resolve_azure(&input["azure"], &loaded.config["azure"], &env, true) {
            Some(azure) => options["azure"] = azure,
            None => {
                if let Some(options) = options.as_object_mut() {
                    options.remove("azure");
                }
            }
        }
        // Reference engineModels for route checks: API model names, whatever the engine.
        let requested = match input["models"]
            .as_array()
            .filter(|models| !models.is_empty())
        {
            Some(models) => models
                .iter()
                .filter_map(|model| model.as_str().map(str::to_owned))
                .collect(),
            None => vec![
                text(&matches, "model")
                    .or(loaded.config["model"].as_str())
                    .unwrap_or(crate::policy::DEFAULT_API_MODEL)
                    .to_owned(),
            ],
        };
        let mut models: Vec<String> = vec![];
        for model in requested {
            let model = execution::api_model(&model)?;
            if !models.contains(&model) {
                models.push(model);
            }
        }
        options["models"] = json!(models);
        return route(&options, &env, flag(&matches, "preflight"));
    }
    if flag(&matches, "browser") {
        println!("`--browser` is deprecated; use `--engine browser` instead.");
    }
    if render || copy_requested {
        let tokens = render_tokens(&request);
        let threshold = 196_000;
        if tokens > threshold {
            println!(
                "Warning: bundle is ~{} tokens (>{}); may exceed model limits.",
                grouped(tokens),
                grouped(threshold)
            );
        }
        if render {
            println!("{}", request.markdown.trim_end_matches('\n'));
        }
        if copy_requested {
            println!(
                "{}",
                copy_report(&copy(&request.markdown), tokens, request.files.len())
            );
        }
        return Ok(0);
    }
    if let Some(mode) = preview {
        if request.engine == "api" {
            providers::resolve_route(
                request.options["model"]
                    .as_str()
                    .context("Missing preview model")?,
                &request.options,
                &env,
            )?;
            api_preview(&request)?;
        } else {
            let mut prompt = crate::browser_prompt::BrowserPrompt::assemble(
                &request.files,
                request.options["prompt"].as_str().unwrap_or(""),
                request.options["system"].as_str(),
                &request.options,
                &std::env::temp_dir(),
            )?;
            let followups = request.options["browserFollowUps"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for line in prompt.preview_lines(
                &request.browser_preview,
                request.options["model"].as_str().unwrap_or("gpt-6-pro"),
                mode,
                &followups,
            )? {
                println!("{line}");
            }
            prompt.cleanup()?;
        }
        return Ok(0);
    }
    let preference = if flag(&matches, "wait") {
        Some(true)
    } else if flag(&matches, "no-wait") {
        Some(false)
    } else {
        None
    };
    let mut wait = crate::cli_execution::wait_preference(
        preference,
        &json!({"mode":request.engine,"options":request.options}),
    );
    if remote.is_some() {
        if !wait {
            println!("Remote browser runs require --wait; ignoring --no-wait.");
        }
        wait = true;
    }
    request.options["waitPreference"] = json!(wait);
    let (session, route) = if let Some(connection) = &remote {
        (
            crate::remote_session::prepare(&store, &request, &cwd, connection)?,
            None,
        )
    } else if request.engine == "browser" {
        (
            crate::browser_runner::prepare(&store, &request, &cwd)?,
            None,
        )
    } else {
        let (route, key) = providers::resolve_route(
            request.options["model"].as_str().context("Missing model")?,
            &request.options,
            &env,
        )?;
        let key = key.with_context(|| providers::missing_key_message(&route))?;
        // Reference runOracle prints the file report before the over-budget refusal.
        let session =
            execution::prepare(&store, &request, &cwd, route.clone()).inspect_err(|error| {
                if error.is::<execution::InputTooLarge>() {
                    for line in crate::cli_execution::reported_files(&request).unwrap_or_default() {
                        println!("{line}");
                    }
                }
            })?;
        (session, Some((route, key)))
    };
    let id = session["id"].as_str().context("Missing session ID")?;
    let cancel = CancellationToken::new();
    let signal = crate::browser_signal::cancel_on_ctrl_c(cancel.clone());
    let result =
        crate::cli_execution::run(&store, id, &request, wait, &env, route, remote, &cancel).await;
    signal.abort();
    result
}
pub(crate) fn grouped(value: usize) -> String {
    let text = value.to_string();
    let mut result = String::new();
    for (index, digit) in text.chars().enumerate() {
        if index > 0 && (text.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}
fn api_preview(request: &execution::ResolvedRequest) -> Result<()> {
    for line in api_preview_lines(request)? {
        println!("{line}");
    }
    Ok(())
}
/// Reference runApiDryRun lines.
pub(crate) fn api_preview_lines(request: &execution::ResolvedRequest) -> Result<Vec<String>> {
    let system = request.options["system"]
        .as_str()
        .map(crate::policy::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(prompts::DEFAULT_SYSTEM);
    let estimate = crate::tokenizer::estimate_chat(system, &request.user_prompt)?;
    let mut lines = vec![format!(
        "[dry-run] Industrious ({}) would call {} with ~{} tokens and {} files.",
        env!("CARGO_PKG_VERSION"),
        request.options["model"]
            .as_str()
            .context("Missing preview model")?,
        grouped(estimate),
        request.files.len()
    )];
    if request.files.is_empty() {
        lines.push("[dry-run] No files matched the provided --file patterns.".into());
    } else {
        lines.extend(file_report(request)?.0);
    }
    Ok(lines)
}
/// Reference printFileTokenStats lines and the total file tokens, against the input budget.
pub(crate) fn file_report(request: &execution::ResolvedRequest) -> Result<(Vec<String>, usize)> {
    let settings = providers::model_settings(
        request.options["model"]
            .as_str()
            .context("Missing preview model")?,
        &request.options,
    )?;
    let budget = request.options["maxInput"]
        .as_u64()
        .unwrap_or(settings.input_limit) as usize;
    let mut stats =
        request
            .files
            .iter()
            .map(|file| {
                Ok((
                    file,
                    crate::tokenizer::count_text(crate::policy::trim_end(
                        &prompts::file_sections(std::slice::from_ref(file), true, true),
                    ))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
    stats.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let mut lines = vec!["File Token Usage".to_owned()];
    let mut total = 0;
    for (file, tokens) in stats {
        total += tokens;
        lines.push(format!(
            "{:>10}  {:>8}  {}",
            grouped(tokens),
            format!("{:.2}%", tokens as f64 / budget as f64 * 100.0),
            file.display_path
        ));
    }
    lines.push(format!(
        "Total: {} tokens ({:.2}% of {})",
        grouped(total),
        total as f64 / budget as f64 * 100.0,
        grouped(budget)
    ));
    Ok((lines, total))
}
/// Reference --notify/--no-notify and --notify-sound/--no-notify-sound: the later flag wins.
fn notification_flags(matches: &ArgMatches, input: &mut Value) {
    for (on, off, key) in [
        ("notify", "no-notify", "notify"),
        ("notify-sound", "no-notify-sound", "notifySound"),
    ] {
        let last = |id: &str| {
            flag(matches, id)
                .then(|| matches.indices_of(id).and_then(Iterator::max))
                .flatten()
        };
        match (last(on), last(off)) {
            (Some(yes), Some(no)) => input[key] = json!(yes > no),
            (Some(_), None) => input[key] = json!(true),
            (None, Some(_)) => input[key] = json!(false),
            (None, None) => {}
        }
    }
}
/// Reference parseHeartbeatOption: seconds, "" for the default, "false"/"off" to disable.
fn heartbeat_seconds(value: &str) -> Result<f64> {
    let value = crate::policy::trim(value).to_lowercase();
    if value.is_empty() {
        return Ok(30.0);
    }
    if value == "false" || value == "off" {
        return Ok(0.0);
    }
    // Number.parseFloat reads the longest leading decimal number and ignores the rest.
    let prefix = value
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-'))
        .count();
    (1..=prefix)
        .rev()
        .find_map(|end| value[..end].parse::<f64>().ok())
        .filter(|seconds| *seconds >= 0.0)
        .context("Heartbeat interval must be zero or a positive number.")
}
/// Reference arguments left once the trace options are removed (routingCliArgs).
fn no_routing_arguments(args: &[OsString]) -> bool {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--perf-trace") => {}
            Some("--perf-trace-path") => {
                rest.next();
            }
            Some(arg) if arg.starts_with("--perf-trace-path=") => {}
            _ => return false,
        }
    }
    true
}
/// Flags for features Industrious leaves out; refused with the reason instead of a gate.
fn excluded_flag(id: &str) -> Option<&'static str> {
    Some(match id {
        "retain-hours" | "clear" | "clean" => {
            "session pruning is not included; stored sessions are kept for recovery"
        }
        "browser-recheck-delay" | "browser-recheck-timeout" => {
            "delayed assistant rechecks are not included; recover with `industrious session <id>`"
        }
        "browser-auto-reattach-delay"
        | "browser-auto-reattach-interval"
        | "browser-auto-reattach-timeout" => {
            "automatic reattach is not included; recover with `industrious session <id>`"
        }
        "browser-reuse-wait" => "waiting to reuse a starting Chrome is not included",
        "verbose-render" => "terminal Markdown rendering is not included; output is plain text",
        "browser-approval-wait" => "approval wait applies only to `industrious serve`",
        "browser-capture-provider-native" => "provider-native conversation capture is not included",
        _ => return None,
    })
}
/// Reference loadUserConfig: every load reports unreadable or invalid files on stderr.
fn load_config(
    store: &SessionStore,
    cwd: &Path,
    env: &Environment,
    include_project: bool,
) -> config::LoadedConfig {
    let loaded = config::load(
        store.home(),
        cwd,
        env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(Path::new),
        include_project,
    );
    for warning in &loaded.warnings {
        eprintln!("{warning}");
    }
    loaded
}
/// Configuration keys the reference reads for features this build excludes, with the flag
/// whose reason applies.
const EXCLUDED_CONFIG_KEYS: &[(&str, &str, &str)] = &[
    ("", "sessionRetentionHours", "retain-hours"),
    (
        "browser",
        "assistantRecheckDelayMs",
        "browser-recheck-delay",
    ),
    (
        "browser",
        "assistantRecheckTimeoutMs",
        "browser-recheck-timeout",
    ),
    ("browser", "reuseChromeWaitMs", "browser-reuse-wait"),
    (
        "browser",
        "autoReattachDelayMs",
        "browser-auto-reattach-delay",
    ),
    (
        "browser",
        "autoReattachIntervalMs",
        "browser-auto-reattach-interval",
    ),
    (
        "browser",
        "autoReattachTimeoutMs",
        "browser-auto-reattach-timeout",
    ),
];
/// Refuses a configuration that sets a key for an excluded feature.
pub(crate) fn refuse_excluded_config(config: &Value) -> Result<()> {
    for (section, key, flag) in EXCLUDED_CONFIG_KEYS {
        let scope = if section.is_empty() {
            config
        } else {
            &config[*section]
        };
        if scope.get(*key).is_some_and(|value| !value.is_null()) {
            let path = if section.is_empty() {
                (*key).to_owned()
            } else {
                format!("{section}.{key}")
            };
            let reason = excluded_flag(flag).unwrap_or("this feature is not included");
            bail!("config key {path} is not supported in this build: {reason}.");
        }
    }
    Ok(())
}
fn refuse_excluded(id: &str) -> Result<()> {
    match excluded_flag(id) {
        Some(reason) => bail!("--{id} is not supported in this build: {reason}."),
        None => Ok(()),
    }
}
/// Commander parses program options anywhere, and a command reads only the ones it defines or
/// takes from optsWithGlobals; it ignores the rest. Excluded features keep their own refusal.
fn require_flags(m: &ArgMatches, supported: &[&str]) -> Result<()> {
    for id in m.ids() {
        if !supported.contains(&id.as_str()) && specified(m, id.as_str()) {
            refuse_excluded(id.as_str())?;
        }
    }
    Ok(())
}
/// Internal worker entries run exactly what their parent saved; any other input is refused
/// rather than ignored, so a stray option or command can never start a different workflow.
fn internal_entry(m: &ArgMatches, entry: &str, companions: &[&str]) -> Result<()> {
    let other = m.ids().map(clap::Id::as_str).any(|id| {
        id != entry
            && !companions.contains(&id)
            && !matches!(id, "perf-trace" | "perf-trace-path")
            && specified(m, id)
    });
    anyhow::ensure!(
        !other && m.subcommand_name().is_none(),
        "--{entry} is an internal worker entry and takes no other options or commands."
    );
    Ok(())
}
/// Root options a root run reads. Every other root option is excluded, so a root run never
/// ignores an option (checked by every_root_option_is_read_or_refused_on_a_root_run).
const ROOT_RUN_FLAGS: &[&str] = &[
    "positional-prompt",
    "prompt",
    "message",
    "followup",
    "followup-model",
    "file",
    "include",
    "files",
    "path",
    "paths",
    "copy-markdown",
    "copy",
    "slug",
    "model",
    "models",
    "reasoning-effort",
    "reasoning-mode",
    "engine",
    "mode",
    "timeout",
    "background",
    "no-background",
    "http-timeout",
    "preview",
    "dry-run",
    "route",
    "preflight",
    "exec-session",
    "session",
    "status",
    "render-markdown",
    "render",
    "render-plain",
    "write-output",
    "write-artifacts",
    "files-report",
    "verbose",
    "heartbeat",
    "youtube",
    "edit-image",
    "aspect",
    "gemini-show-thoughts",
    "no-gemini-fallback",
    "zombie-timeout",
    "zombie-last-activity",
    "allow-partial",
    "partial",
    "notify",
    "no-notify",
    "notify-sound",
    "no-notify-sound",
    "browser-tab",
    "search",
    "max-input",
    "max-output",
    "max-file-size-bytes",
    "base-url",
    "provider",
    "no-azure",
    "azure-endpoint",
    "azure-deployment",
    "azure-api-version",
    "browser",
    "remote-host",
    "remote-token",
    "browser-model-strategy",
    "browser-thinking-time",
    "browser-inline-cookies",
    "browser-inline-cookies-file",
    "copy-profile",
    "browser-chrome-profile",
    "browser-cookie-sync",
    "browser-no-cookie-sync",
    "browser-cookie-path",
    "browser-cookie-names",
    "browser-cookie-wait",
    "browser-allow-cookie-errors",
    "browser-attachment-timeout",
    "browser-research",
    "browser-archive",
    "browser-fallback",
    "browser-follow-up",
    "browser-attachments",
    "browser-inline-files",
    "browser-bundle-files",
    "browser-bundle-format",
    "browser-keep-browser",
    "browser-headless",
    "browser-hide-window",
    "browser-manual-login",
    "browser-manual-login-profile-dir",
    "remote-chrome",
    "browser-chrome-path",
    "browser-timeout",
    "browser-input-timeout",
    "browser-max-concurrent-tabs",
    "browser-profile-lock-timeout",
    "browser-attach-running",
    "browser-port",
    "browser-debug-port",
    "chatgpt-url",
    "browser-url",
    "generate-image",
    "output",
    "force",
    "debug-help",
    "wait",
    "no-wait",
    // Requests the only behaviour this build has: no provider-native capture.
    "no-browser-capture-provider-native",
];
fn refuse_unimplemented_flags(m: &ArgMatches) -> Result<()> {
    require_flags(m, ROOT_RUN_FLAGS)
}
/// Project Sources cookie-sync options. Kept out of `run`, whose Windows debug frame is near its limit.
fn sources_cookie_options(
    matches: &ArgMatches,
    scopes: &[&ArgMatches],
    input: &mut Value,
    overrides: &mut Value,
    env: &Environment,
) {
    let scoped = |flag| option_scope(matches, scopes, flag);
    if self::flag(scoped("browser-no-cookie-sync"), "browser-no-cookie-sync") {
        overrides["cookieSync"] = json!(false);
        overrides["manualLoginCookieSync"] = json!(false);
        input["browserNoCookieSync"] = json!(true);
    } else if self::flag(scoped("browser-cookie-sync"), "browser-cookie-sync") {
        overrides["cookieSync"] = json!(true);
        input["browserCookieSync"] = json!(true);
    }
    if let Some(path) = text(scoped("browser-cookie-path"), "browser-cookie-path") {
        overrides["chromeCookiePath"] = json!(path);
    }
    if let Some(names) = crate::project_sources::cookie_names(
        text(scoped("browser-cookie-names"), "browser-cookie-names"),
        env,
    ) {
        overrides["cookieNames"] = names;
    }
    if let Some(raw) = text(scoped("browser-cookie-wait"), "browser-cookie-wait") {
        let wait = followup::attachment_timeout(raw).unwrap_or_else(|| {
            println!(
                "Warning: invalid --browser-cookie-wait duration \"{raw}\"; using fallback 0ms."
            );
            0.0
        });
        overrides["cookieSyncWaitMs"] = json!(wait.min(u64::MAX as f64) as u64);
    }
    if self::flag(
        scoped("browser-allow-cookie-errors"),
        "browser-allow-cookie-errors",
    ) {
        overrides["allowCookieErrors"] = json!(true);
    }
}
fn browser_runtime_overrides(m: &ArgMatches, current: &Value, env: &Environment) -> Result<Value> {
    let mut overrides = browser_runtime_overrides_at_scope(m, &[], current, env)?;
    if let Some(raw) = text(m, "chatgpt-url").or_else(|| text(m, "browser-url")) {
        let url = crate::remote_driver::normalise_url(Some(&json!(raw)))?;
        overrides["chatgptUrl"] = json!(url);
        overrides["url"] = json!(url);
    }
    Ok(overrides)
}
fn browser_runtime_overrides_at_scope(
    m: &ArgMatches,
    scopes: &[&ArgMatches],
    current: &Value,
    env: &Environment,
) -> Result<Value> {
    let text = |key| self::text(option_scope(m, scopes, key), key);
    let flag = |key| self::flag(option_scope(m, scopes, key), key);
    let specified = |key| self::specified(option_scope(m, scopes, key), key);
    let mut overrides = json!({});
    if flag("browser-attach-running") || current["attachRunning"] == true {
        // Reference validateAttachRunningOptions, in its order.
        let named = |key| text(key).is_some_and(|value: &str| !value.is_empty());
        let mut conflicts = vec![];
        for option in ["browser-chrome-profile", "browser-cookie-path"] {
            if named(option) {
                conflicts.push(format!("--{option}"));
            }
        }
        for option in [
            "browser-cookie-sync",
            "browser-no-cookie-sync",
            "browser-headless",
            "browser-hide-window",
            "browser-keep-browser",
            "browser-manual-login",
        ] {
            if flag(option) {
                conflicts.push(format!("--{option}"));
            }
        }
        if named("browser-manual-login-profile-dir") {
            conflicts.push("--browser-manual-login-profile-dir".into());
        }
        if text("copy-profile").is_some() || current["copyProfileSource"].is_string() {
            conflicts.push("--copy-profile".into());
        }
        if named("browser-inline-cookies") || named("browser-inline-cookies-file") {
            conflicts.push("--browser-inline-cookies/--browser-inline-cookies-file".into());
        }
        if specified("browser-port") || specified("browser-debug-port") {
            conflicts.push("--browser-port/--browser-debug-port".into());
        }
        anyhow::ensure!(
            conflicts.is_empty(),
            "--browser-attach-running cannot be combined with {} because attach mode reuses an already-running browser instead of launching and configuring its own Chrome instance.",
            conflicts.join(", ")
        );
    }
    if specified("browser-attach-running") {
        overrides["attachRunning"] = json!(flag("browser-attach-running"));
    }
    if let Some(path) = text("browser-chrome-path") {
        overrides["chromePath"] = json!(path);
    }
    if let Some(reference) = text("browser-tab").filter(|r| !crate::policy::trim(r).is_empty()) {
        overrides["browserTabRef"] = json!(reference);
    }
    if let Some(source) = text("copy-profile") {
        anyhow::ensure!(
            !flag("browser-manual-login")
                && current["manualLogin"] != true
                && current["manualLoginProfileDir"].is_null()
                && text("browser-manual-login-profile-dir").is_none()
                && !env
                    .get("ORACLE_BROWSER_PROFILE_DIR")
                    .is_some_and(|path| !path.trim().is_empty())
                && !flag("browser-keep-browser")
                && text("remote-chrome").is_none()
                && text("remote-host").is_none(),
            "--copy-profile cannot be combined with manual login, keep-browser or a remote browser"
        );
        overrides["copyProfileSource"] = json!(source);
        overrides["manualLogin"] = json!(false);
    }
    if let Some(profile) = text("browser-chrome-profile") {
        anyhow::ensure!(
            text("copy-profile").is_some()
                || current["copyProfileSource"].is_string()
                || profile == "Default"
                || current["cookieSync"] == true
                || flag("browser-cookie-sync"),
            "Custom Chrome profile selection requires --copy-profile or Chrome cookie sync"
        );
        overrides["chromeProfile"] = json!(profile);
    }
    if let Some(raw) = text("browser-port").or_else(|| text("browser-debug-port")) {
        let port = crate::policy::trim(raw)
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .context("Browser port must be an integer between 1 and 65535.")?;
        overrides["debugPort"] = json!(port);
    }
    if let Some(remote) = text("remote-chrome") {
        let endpoint = crate::remote_service::endpoint(&json!(remote))?;
        anyhow::ensure!(endpoint.is_object(), "Remote Chrome requires host:port");
        overrides["remoteChrome"] = endpoint;
        overrides["remoteChromeBrowserWSEndpoint"] = Value::Null;
    }
    for (flag, key, fallback) in [
        ("browser-timeout", "timeoutMs", 1_200_000u64),
        ("browser-input-timeout", "inputTimeoutMs", 60_000u64),
    ] {
        if let Some(raw) = text(flag) {
            let milliseconds = followup::attachment_timeout(raw).unwrap_or_else(|| {
                println!(
                    "Warning: invalid --{flag} duration \"{raw}\"; using fallback {fallback}ms."
                );
                fallback as f64
            });
            overrides[key] = if milliseconds < u64::MAX as f64 {
                json!(milliseconds as u64)
            } else {
                json!(milliseconds)
            };
        }
    }
    if let Some(raw) = text("browser-profile-lock-timeout") {
        // Reference parseBrowserDuration; Rust bounds profile admission and never skips it at 0.
        let milliseconds = followup::attachment_timeout(raw).unwrap_or_else(|| {
            println!(
                "Warning: invalid --browser-profile-lock-timeout duration \"{raw}\"; using fallback 0ms."
            );
            0.0
        });
        overrides["profileLockTimeoutMs"] = json!(milliseconds.min(u64::MAX as f64) as u64);
    }
    if let Some(raw) = text("browser-max-concurrent-tabs") {
        let raw = crate::policy::trim(raw);
        let end = raw
            .char_indices()
            .find(|(i, c)| !c.is_ascii_digit() && !(*i == 0 && matches!(c, '+' | '-')))
            .map_or(raw.len(), |(i, _)| i);
        let parsed = raw[..end]
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0)
            .context("--browser-max-concurrent-tabs must be a positive integer.")?;
        overrides["maxConcurrentTabs"] = if parsed < u64::MAX as f64 {
            json!(parsed as u64)
        } else {
            json!(parsed)
        };
    } else if let Some(capacity) = browser_capacity_override(current, env) {
        overrides["maxConcurrentTabs"] = capacity;
    }
    Ok(overrides)
}
fn browser_capacity_override(current: &Value, env: &Environment) -> Option<Value> {
    if !current["maxConcurrentTabs"].is_null() {
        return None;
    }
    let raw = crate::policy::trim(env.get("ORACLE_BROWSER_MAX_CONCURRENT_TABS")?);
    (!raw.is_empty()
        && raw.bytes().all(|byte| byte.is_ascii_digit())
        && raw
            .parse::<f64>()
            .is_ok_and(|number| number.is_finite() && number > 0.0 && number.fract() == 0.0))
    .then(|| json!(crate::browser_health::tab_limit(&json!(raw))))
}

/// Options the session and status commands read; the reference SESSION_OPTION_KEYS.
const SESSION_FLAGS: &[&str] = &[
    "id",
    "hours",
    "limit",
    "all",
    "clear",
    "clean",
    "hide-prompt",
    "render",
    "render-markdown",
    "model",
    "path",
    "harvest",
    "live",
    "write-output",
    "browser-tabs",
    "browser-tab",
    "no-recover",
];
/// Reference listIgnoredFlags: typed root options a session view ignores, as Commander attribute
/// names. Negations record false, which the reference skips; excluded features still refuse.
fn ignored_root_flags(m: &ArgMatches) -> Result<Vec<String>> {
    let mut ignored = vec![];
    for id in m.ids().map(clap::Id::as_str) {
        let session_key = SESSION_FLAGS.contains(&id);
        let traced = matches!(id, "perf-trace" | "perf-trace-path");
        if session_key || traced || id == "positional-prompt" || !specified(m, id) {
            continue;
        }
        refuse_excluded(id)?;
        if id.starts_with("no-") {
            continue;
        }
        let mut words = id.split('-');
        let mut name = words.next().unwrap_or_default().to_owned();
        for word in words {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                name.extend(first.to_uppercase());
                name.push_str(chars.as_str());
            }
        }
        ignored.push(name);
    }
    Ok(ignored)
}
/// Reference handleSessionCommand and the status action read these through optsWithGlobals, and
/// Commander's program parser takes them wherever they are typed, so a program value applies.
/// Options the view does not define (status has no --write-output) stay unread.
fn view_scope<'a>(root: &'a ArgMatches, view: &'a ArgMatches, id: &str) -> &'a ArgMatches {
    let global = matches!(
        id,
        "model" | "render" | "render-markdown" | "write-output" | "browser-tab"
    );
    if global && view.try_get_raw(id).is_ok() {
        option_scope(root, &[view], id)
    } else {
        view
    }
}
fn attach_view<'a>(
    root: &'a ArgMatches,
    m: &'a ArgMatches,
) -> crate::session_attach::AttachView<'a> {
    let scoped = |id| view_scope(root, m, id);
    crate::session_attach::AttachView {
        model: text(scoped("model"), "model"),
        hide_prompt: flag(m, "hide-prompt"),
        render: flag(scoped("render"), "render")
            || flag(scoped("render-markdown"), "render-markdown"),
    }
}
/// Reference sessionCommand and showStatus.
async fn session_command(
    store: &SessionStore,
    root: &ArgMatches,
    m: &ArgMatches,
    name: &str,
    env: &Environment,
    ignored: &[String],
) -> Result<i32> {
    let id = m
        .try_get_one::<String>("id")
        .ok()
        .flatten()
        .map(String::as_str)
        .or_else(|| text(m, "session"));
    if flag(m, "clear") || flag(m, "clean") {
        if id.is_some() {
            bail!(
                "Cannot combine a session ID with --clear. Remove the ID to delete cached sessions."
            );
        }
        refuse_excluded(if flag(m, "clear") { "clear" } else { "clean" })?;
    }
    if flag(m, "browser-tabs") {
        if id.is_some() {
            bail!(
                "Cannot combine a session ID with --browser-tabs. Remove the ID to inspect live browser tabs."
            );
        }
        let cancel = CancellationToken::new();
        let signal_cancel = cancel.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_cancel.cancel();
            }
        });
        let result = crate::browser_sessions::tabs_status(store, &cancel).await;
        signal.abort();
        if cancel.is_cancelled() {
            return Ok(130);
        }
        print!("{}", result?);
        return Ok(0);
    }
    if matches!(id, Some("clear" | "clean")) {
        bail!(
            "Session cleanup now uses --clear. Run \"industrious {name} --clear --hours <n>\" instead."
        );
    }
    if flag(m, "path") {
        let id = id.context("The --path flag requires a session ID.")?;
        let dir = store.directory(id)?;
        let missing = ["meta.json", "output.log"]
            .into_iter()
            .filter(|file| !dir.join(file).exists())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!("Session \"{id}\" is missing: {}", missing.join(", "));
        }
        println!(
            "Session dir: {}\nMetadata: {}\nRequest: {}\nLog: {}",
            dir.display(),
            dir.join("meta.json").display(),
            dir.join("request.json").display(),
            dir.join("output.log").display()
        );
        return Ok(0);
    }
    if flag(m, "harvest") && flag(m, "live") {
        bail!("Cannot combine --harvest and --live. Choose one.");
    }
    if text(view_scope(root, m, "write-output"), "write-output").is_some()
        && !flag(m, "harvest")
        && !flag(m, "live")
    {
        bail!("The --write-output flag requires --harvest or --live.");
    }
    if flag(m, "harvest") || flag(m, "live") {
        let option = if flag(m, "harvest") {
            "--harvest"
        } else {
            "--live"
        };
        let id = id.with_context(|| format!("The {option} flag requires a session ID."))?;
        return inspect_session(store, id, root, m, env).await;
    }
    if let Some(id) = id {
        if !ignored.is_empty() {
            println!("Ignoring flags on session attach: {}", ignored.join(", "));
        }
        return attach(store, id, &attach_view(root, m), env).await;
    }
    let all = store.list(true)?;
    let mut entries = all.clone();
    entries.retain(|entry| !crate::browser_attempt::internal(entry));
    for entry in &mut entries {
        entry["status"] = store.listed_status(entry);
    }
    if let Some(model) = text(view_scope(root, m, "model"), "model") {
        entries.retain(|entry| crate::session_display::matches_model(entry, model));
    }
    let hours = text(m, "hours")
        .unwrap_or("24")
        .parse::<f64>()
        .context("Value must be a number.")?;
    let limit = text(m, "limit")
        .unwrap_or("100")
        .parse::<usize>()
        .context("Value must be an integer.")?;
    // Reference usesDefaultStatusFilters; the root --status alias always shows the examples.
    let examples = !["hours", "limit", "all"].iter().any(|id| specified(m, id));
    let (entries, total, truncated) = sessions::filter_range(entries, hours, flag(m, "all"), limit);
    for line in crate::session_display::status_listing(&entries, &all, total, truncated, examples) {
        println!("{line}");
    }
    Ok(0)
}
fn print_inspected_answer(
    store: &SessionStore,
    id: &str,
    root: &ArgMatches,
    m: &ArgMatches,
    answer: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    anyhow::ensure!(!cancel.is_cancelled(), "Browser inspection cancelled");
    if let Some(destination) = text(view_scope(root, m, "write-output"), "write-output") {
        let meta = store.require(id)?;
        let cwd = Path::new(
            meta["cwd"]
                .as_str()
                .context("Browser output has no saved working directory")?,
        );
        match crate::browser_sessions::write_output(store, cwd, destination, answer, cancel)? {
            Some(path) => println!("Wrote harvested assistant output to {}", path.display()),
            None => {
                print!("{answer}{}", if answer.ends_with('\n') { "" } else { "\n" });
                return Ok(());
            }
        }
    }
    if !answer.is_empty() {
        anyhow::ensure!(!cancel.is_cancelled(), "Browser inspection cancelled");
        print!("{answer}{}", if answer.ends_with('\n') { "" } else { "\n" });
    }
    Ok(())
}
/// A settled completed session's verified saved answer. The owner retires its tab once it has
/// captured the answer, so a live tail reads this instead of inspecting a closing target.
fn completed_answer(store: &SessionStore, id: &str) -> Result<Option<String>> {
    let meta = store.require(id)?;
    if !crate::browser_inspection::settled_completion(&meta) {
        return Ok(None);
    }
    let receipt: crate::artifacts::CaptureReceipt =
        serde_json::from_value(meta["browser"]["capturedAnswer"].clone())?;
    Ok(Some(String::from_utf8(crate::artifacts::verify_capture(
        store, &receipt,
    )?)?))
}
async fn inspect_session(
    store: &SessionStore,
    id: &str,
    root: &ArgMatches,
    m: &ArgMatches,
    env: &Environment,
) -> Result<i32> {
    let reference = text(view_scope(root, m, "browser-tab"), "browser-tab");
    if reference.is_some_and(|reference| crate::policy::trim(reference).is_empty()) {
        bail!("The --browser-tab reference must not be empty.");
    }
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = async {
        let options = crate::browser_inspection::InspectionOptions {
            reference: reference.map(str::to_owned),
            recover: !flag(m, "no-recover"),
            progress: Some(|line| println!("{line}")),
            warning: Some(|line| eprintln!("{line}")),
            ..Default::default()
        };
        if options.reference.is_none()
            && options.recover
            && let Some(completion) =
                crate::browser_historical::resume_creatorless_absence(store, id, &cancel).await?
        {
            let receipt: crate::artifacts::CaptureReceipt =
                serde_json::from_value(completion.session["browser"]["capturedAnswer"].clone())?;
            let answer = String::from_utf8(crate::artifacts::verify_capture(store, &receipt)?)?;
            println!("Recovered completed session {id}; its original browser target is absent.");
            print_inspected_answer(store, id, root, m, &answer, &cancel)?;
            if let Some(error) = completion.cleanup_error {
                eprintln!("Recovered answer saved; browser cleanup is incomplete: {error}");
            }
            return Ok(0);
        }
        let snapshot = if flag(m, "live") {
            let tail_owned = options.reference.is_none();
            if tail_owned && let Some(answer) = completed_answer(store, id)? {
                println!("Session {id} already completed; showing its saved answer.");
                print_inspected_answer(store, id, root, m, &answer, &cancel)?;
                return Ok(0);
            }
            match crate::browser_inspection::live(store, id, &options, env, &cancel, |line| {
                println!("{line}")
            })
            .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let completed = if tail_owned && !cancel.is_cancelled() {
                        completed_answer(store, id)?
                    } else {
                        None
                    };
                    let Some(answer) = completed else {
                        return Err(error);
                    };
                    println!(
                        "Session {id} completed during the live tail; showing its saved answer."
                    );
                    print_inspected_answer(store, id, root, m, &answer, &cancel)?;
                    return Ok(0);
                }
            }
        } else {
            crate::browser_inspection::harvest(store, id, &options, env, &cancel).await?
        };
        if cancel.is_cancelled() {
            bail!("Browser inspection cancelled");
        }
        print!(
            "{}",
            crate::browser_sessions::harvest_summary(id, &snapshot)
        );
        let tab = &snapshot.tab;
        let answer = tab["lastAssistantMarkdown"]
            .as_str()
            .or_else(|| tab["lastAssistantText"].as_str())
            .unwrap_or("");
        if !flag(m, "live") || tab["state"] == "completed" {
            print_inspected_answer(store, id, root, m, answer, &cancel)?;
        }
        if cancel.is_cancelled() {
            bail!("Browser inspection cancelled");
        }
        if let Some(completion) =
            crate::browser_historical::complete_owned(store, id, &snapshot, &cancel).await?
            && let Some(error) = completion.cleanup_error
        {
            eprintln!("Recovered answer saved; browser cleanup is incomplete: {error}");
        }
        Ok::<_, anyhow::Error>(0)
    }
    .await;
    signal.abort();
    if cancel.is_cancelled() {
        eprintln!("{}", cancelled_inspection(result));
        return Ok(130);
    }
    result
}
/// Reference sessionCommand: a cancelled inspection reports the cancellation; another failure
/// keeps its own diagnostics.
fn cancelled_inspection(result: Result<i32>) -> String {
    match result {
        Err(error)
            if !error
                .chain()
                .any(|cause| cause.to_string() == "Browser inspection cancelled") =>
        {
            format!("{error:#}")
        }
        _ => "Browser inspection cancelled.".to_owned(),
    }
}
async fn attach(
    store: &SessionStore,
    id: &str,
    view: &crate::session_attach::AttachView<'_>,
    environment: &Environment,
) -> Result<i32> {
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = crate::session_attach::attach(
        store,
        id,
        view,
        environment,
        &cancel,
        &mut std::io::stdout(),
    )
    .await;
    signal.abort();
    let _ = signal.await;
    result.map_err(|error| anyhow::anyhow!("{error:#}"))
}
/// Reference --route/--preflight: one plan per requested model; any not-ready plan exits 1.
fn route(options: &Value, env: &Environment, preflight: bool) -> Result<i32> {
    let models = options["models"]
        .as_array()
        .filter(|models| !models.is_empty())
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            vec![
                options["model"]
                    .as_str()
                    .unwrap_or(crate::policy::DEFAULT_API_MODEL)
                    .to_owned(),
            ]
        });
    let plans = models
        .iter()
        .map(|model| crate::cli_format::route_plan(model, options, env))
        .collect::<Result<Vec<_>>>()?;
    let title = if preflight {
        "Provider preflight"
    } else {
        "Route plan"
    };
    println!("{}", crate::cli_format::provider_plans(&plans, title));
    Ok(i32::from(plans.iter().any(|plan| plan["ok"] != true)))
}
/// Reference runProviderDoctor.
fn doctor(
    store: &SessionStore,
    root: &ArgMatches,
    m: &ArgMatches,
    env: &Environment,
    cwd: &Path,
) -> Result<i32> {
    require_flags(
        m,
        &[
            "providers",
            "models",
            "model",
            "provider",
            "no-azure",
            "azure-endpoint",
            "azure-deployment",
            "azure-api-version",
            "base-url",
            "json",
        ],
    )?;
    // Reference optsWithGlobals: program model, provider, Azure and base URL options apply.
    let scoped = |id| option_scope(root, &[m], id);
    if !flag(m, "providers") {
        println!("Run `industrious doctor --providers` to inspect API provider readiness.");
        return Ok(0);
    }
    let config = load_config(store, cwd, env, true).config;
    anyhow::ensure!(
        !(text(scoped("provider"), "provider") == Some("azure")
            && flag(scoped("no-azure"), "no-azure")),
        "--provider azure cannot be combined with --no-azure."
    );
    let mut options = json!({});
    if let Some(base) = text(scoped("base-url"), "base-url").or(config["apiBaseUrl"].as_str()) {
        options["baseUrl"] = json!(base);
    }
    if let Some(provider) = text(scoped("provider"), "provider") {
        options["provider"] = json!(provider);
    }
    if flag(scoped("no-azure"), "no-azure") {
        options["provider"] = json!("openai");
    }
    let mut explicit = json!({});
    for (key, target) in [
        ("azure-endpoint", "endpoint"),
        ("azure-deployment", "deployment"),
        ("azure-api-version", "apiVersion"),
    ] {
        if let Some(value) = text(scoped(key), key) {
            explicit[target] = json!(value);
        }
    }
    if let Some(azure) = providers::resolve_azure(&explicit, &config["azure"], env, true) {
        options["azure"] = azure;
    }
    let listed: Vec<String> = text(scoped("models"), "models")
        .map(|models| {
            models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let requested = if listed.is_empty() {
        vec![
            text(scoped("model"), "model")
                .or(config["model"].as_str())
                .unwrap_or(crate::policy::DEFAULT_API_MODEL)
                .to_owned(),
        ]
    } else {
        listed
    };
    let mut models: Vec<String> = vec![];
    for model in requested {
        let model = execution::api_model(&model)?;
        if !models.contains(&model) {
            models.push(model);
        }
    }
    let plans = models
        .iter()
        .map(|model| crate::cli_format::route_plan(model, &options, env))
        .collect::<Result<Vec<_>>>()?;
    if flag(m, "json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "providers": plans }))?
        );
    } else {
        println!(
            "{}",
            crate::cli_format::provider_plans(&plans, "Provider readiness")
        );
    }
    Ok(i32::from(plans.iter().any(|plan| plan["ok"] != true)))
}
/// Reference render/copy estimate: the API request body without background or storage.
fn render_tokens(request: &execution::ResolvedRequest) -> usize {
    let mut options = request.options.clone();
    options["background"] = json!(false);
    if let Some(options) = options.as_object_mut() {
        options.remove("previousResponseId");
        options.remove("reasoningEffort");
    }
    let system = request.options["system"]
        .as_str()
        .map(crate::policy::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(prompts::DEFAULT_SYSTEM);
    providers::build_request(
        crate::policy::DEFAULT_API_MODEL,
        &options,
        &request.user_prompt,
        system,
    )
    .and_then(|body| crate::tokenizer::estimate_request(&body))
    .or_else(|_| crate::tokenizer::estimate_chat(system, &request.user_prompt))
    .unwrap_or(0)
}
fn copy_report(result: &Result<()>, tokens: usize, files: usize) -> String {
    match result {
        Ok(()) => format!(
            "Copied markdown to clipboard (~{} tokens{}).",
            crate::cli_format::compact_number(tokens as f64),
            if files > 0 {
                format!("; {files} files")
            } else {
                String::new()
            }
        ),
        Err(error) => format!(
            "Copy failed ({error}); markdown not printed. Re-run with --render-markdown if you need the content."
        ),
    }
}
fn copy(text: &str) -> Result<()> {
    #[cfg(windows)]
    let commands: &[(&str, &[&str])] = &[("clip.exe", &[])];
    #[cfg(not(windows))]
    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    copy_with(commands, text)
}
fn copy_with(commands: &[(&str, &[&str])], text: &str) -> Result<()> {
    for (command, args) in commands.iter().copied() {
        if let Ok(mut child) = std::process::Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take()
                && stdin.write_all(text.as_bytes()).is_err()
            {
                let _ = child.kill();
            }
            if child.wait()?.success() {
                return Ok(());
            }
        }
    }
    bail!("no usable native clipboard command")
}

#[cfg(test)]
mod browser_options_tests {
    use super::*;
    #[test]
    fn every_captured_command_is_dispatched_and_keeps_its_own_help() {
        let captured: Vec<_> = parser()
            .get_subcommands()
            .map(|command| command.get_name().to_owned())
            .collect();
        assert_eq!(captured, COMMANDS);
    }
    #[test]
    fn attach_running_refuses_cli_launch_and_profile_flags() {
        for configured in [false, true] {
            for option in [
                "--browser-headless",
                "--browser-hide-window",
                "--browser-keep-browser",
                "--browser-manual-login",
                "--browser-manual-login-profile-dir=PUBLIC-profile",
                "--browser-port=9333",
                "--browser-debug-port=9333",
                "--browser-chrome-profile=PUBLIC",
                "--browser-cookie-path=PUBLIC-cookies",
                "--browser-cookie-sync",
                "--browser-no-cookie-sync",
                "--browser-inline-cookies=PUBLIC",
                "--browser-inline-cookies-file=PUBLIC.json",
            ] {
                let mut arguments = vec!["industrious", option];
                if !configured {
                    arguments.push("--browser-attach-running");
                }
                let matches = parser().try_get_matches_from(arguments).unwrap();
                let error = browser_runtime_overrides(
                    &matches,
                    &json!({"attachRunning":configured}),
                    &Environment::new(),
                )
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("--browser-attach-running cannot be combined"),
                    "{option}: {error:#}"
                );
            }
        }
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--browser-attach-running",
                "--browser-cookie-sync",
                "--browser-port=9333",
                "--browser-debug-port=9333",
                "--browser-headless",
            ])
            .unwrap();
        assert_eq!(
            browser_runtime_overrides(&matches, &json!({}), &Environment::new())
                .unwrap_err()
                .to_string(),
            "--browser-attach-running cannot be combined with --browser-cookie-sync, --browser-headless, --browser-port/--browser-debug-port because attach mode reuses an already-running browser instead of launching and configuring its own Chrome instance."
        );
        let matches = parser()
            .try_get_matches_from(["industrious", "--browser-attach-running"])
            .unwrap();
        assert_eq!(
            browser_runtime_overrides(
                &matches,
                &json!({"manualLogin":true,"manualLoginProfileDir":"PUBLIC-admission"}),
                &Environment::new()
            )
            .unwrap()["attachRunning"],
            true
        );
    }
    #[test]
    fn supported_runtime_flags_override_config_with_native_numeric_values() {
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--remote-chrome",
                "[::1]:9333",
                "--browser-chrome-path",
                "PUBLIC Chrome.exe",
                "--browser-timeout",
                "1h30m",
                "--browser-input-timeout",
                "0",
                "--browser-max-concurrent-tabs",
                "+7tabs",
                "--browser-attach-running",
            ])
            .unwrap();
        refuse_unimplemented_flags(&matches).unwrap();
        let config = json!({"chromePath":"PUBLIC configured.exe","timeoutMs":100,"inputTimeoutMs":100,"maxConcurrentTabs":2,"remoteChromeBrowserWSEndpoint":"ws://PUBLIC-previous/devtools/browser/PUBLIC"});
        let env = Environment::from([("ORACLE_BROWSER_MAX_CONCURRENT_TABS".into(), "5".into())]);
        let overrides = browser_runtime_overrides(&matches, &config, &env).unwrap();
        let config = config::deep_merge(&config, &overrides);
        assert_eq!(config["remoteChrome"], json!({"host":"::1","port":9333}));
        assert!(config["remoteChromeBrowserWSEndpoint"].is_null());
        assert_eq!(config["chromePath"], "PUBLIC Chrome.exe");
        assert_eq!(config["timeoutMs"].as_u64(), Some(5_400_000));
        assert_eq!(config["inputTimeoutMs"].as_u64(), Some(0));
        assert_eq!(config["maxConcurrentTabs"], 7);
        assert_eq!(config["attachRunning"], true);
    }
    #[test]
    fn owned_launch_flags_preserve_port_alias_precedence_and_override_both_urls() {
        for (port, url) in [
            ("--browser-port", "--chatgpt-url"),
            ("--browser-debug-port", "--browser-url"),
        ] {
            let matches = parser()
                .try_get_matches_from([
                    "industrious",
                    port,
                    "9333",
                    url,
                    " chatgpt.com/g/g-p-public/project?tab=sources ",
                ])
                .unwrap();
            refuse_unimplemented_flags(&matches).unwrap();
            let configured = json!({"debugPort":9222,"chatgptUrl":"https://chatgpt.com/c/public-configured","url":"https://chatgpt.com/"});
            let actual = config::deep_merge(
                &configured,
                &browser_runtime_overrides(&matches, &configured, &Environment::new()).unwrap(),
            );
            assert_eq!(actual["debugPort"], 9333);
            for key in ["url", "chatgptUrl"] {
                assert_eq!(
                    actual[key],
                    "https://chatgpt.com/g/g-p-public/project?tab=sources"
                );
            }
        }
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--browser-debug-port",
                "9334",
                "--browser-port",
                "9333",
                "--browser-url",
                "https://chatgpt.com/c/public-alias",
                "--chatgpt-url",
                "https://chatgpt.com/c/public-preferred",
            ])
            .unwrap();
        let actual = browser_runtime_overrides(&matches, &json!({}), &Environment::new()).unwrap();
        assert_eq!(actual["debugPort"], 9333);
        assert_eq!(actual["url"], "https://chatgpt.com/c/public-preferred");
        assert_eq!(actual["chatgptUrl"], actual["url"]);
    }
    #[test]
    fn explicit_launch_ports_and_urls_refuse_unsafe_inputs() {
        for (flag, raw) in [
            ("--browser-port", "0"),
            ("--browser-debug-port", "65536"),
            ("--browser-port", "9333tail"),
            ("--browser-port", "1.5"),
            ("--browser-url", "https://example.invalid/"),
            ("--chatgpt-url", "http://chatgpt.com/"),
            ("--browser-url", "https://chatgpt.com:9333/"),
            ("--chatgpt-url", "https://PUBLIC@chatgpt.com/"),
        ] {
            let matches = parser()
                .try_get_matches_from(["industrious", &format!("{flag}={raw}")])
                .unwrap();
            assert!(
                browser_runtime_overrides(&matches, &json!({}), &Environment::new()).is_err(),
                "{flag}={raw}"
            );
        }
    }
    #[test]
    fn configured_capacity_precedes_environment_and_invalid_flags_refuse() {
        let matches = parser().try_get_matches_from(["industrious"]).unwrap();
        let env = Environment::from([("ORACLE_BROWSER_MAX_CONCURRENT_TABS".into(), "8".into())]);
        assert_eq!(
            browser_runtime_overrides(&matches, &json!({}), &env).unwrap()["maxConcurrentTabs"],
            8
        );
        assert!(
            browser_runtime_overrides(&matches, &json!({"maxConcurrentTabs":5}), &env)
                .unwrap()
                .get("maxConcurrentTabs")
                .is_none()
        );
        for value in ["8tabs", "+8", "8.0", "0", "-1", ""] {
            let env =
                Environment::from([("ORACLE_BROWSER_MAX_CONCURRENT_TABS".into(), value.into())]);
            assert!(
                browser_runtime_overrides(&matches, &json!({}), &env)
                    .unwrap()
                    .get("maxConcurrentTabs")
                    .is_none(),
                "{value}"
            );
        }
        for (flag, value) in [
            ("--remote-chrome", ""),
            ("--remote-chrome", "PUBLIC-no-port"),
            ("--browser-max-concurrent-tabs", "0"),
            ("--browser-max-concurrent-tabs", "-1"),
            ("--browser-max-concurrent-tabs", "no"),
        ] {
            let matches = parser()
                .try_get_matches_from(["industrious", &format!("{flag}={value}")])
                .unwrap();
            assert!(
                browser_runtime_overrides(&matches, &json!({}), &env).is_err(),
                "{flag}={value}"
            );
        }
    }
    #[test]
    fn invalid_duration_flags_use_the_maintained_fallbacks() {
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--browser-timeout",
                "1.5s",
                "--browser-input-timeout",
                "PUBLIC invalid",
            ])
            .unwrap();
        let overrides = browser_runtime_overrides(
            &matches,
            &json!({"timeoutMs":1,"inputTimeoutMs":2}),
            &Environment::new(),
        )
        .unwrap();
        assert_eq!(overrides["timeoutMs"], 1_200_000);
        assert_eq!(overrides["inputTimeoutMs"], 60_000);
    }
    #[test]
    fn sources_copy_refuses_explicit_persistent_profile_intent_before_defaulting_login() {
        let matches = parser()
            .try_get_matches_from(["industrious", "--copy-profile", "PUBLIC-source"])
            .unwrap();
        for current in [
            json!({"manualLogin":true}),
            json!({"manualLoginProfileDir":"PUBLIC-profile"}),
        ] {
            assert!(
                browser_runtime_overrides_at_scope(&matches, &[], &current, &Environment::new())
                    .is_err()
            );
        }
        let env =
            Environment::from([("ORACLE_BROWSER_PROFILE_DIR".into(), "PUBLIC-profile".into())]);
        assert!(browser_runtime_overrides_at_scope(&matches, &[], &json!({}), &env).is_err());
        let overrides =
            browser_runtime_overrides_at_scope(&matches, &[], &json!({}), &Environment::new())
                .unwrap();
        assert_eq!(overrides["manualLogin"], false);
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--copy-profile",
                "PUBLIC-source",
                "--browser-manual-login-profile-dir",
                "PUBLIC-profile",
            ])
            .unwrap();
        assert!(
            browser_runtime_overrides_at_scope(&matches, &[], &json!({}), &Environment::new())
                .is_err()
        );
    }
    #[test]
    fn every_root_option_is_read_or_refused_on_a_root_run() {
        let options: Value = serde_json::from_str(include_str!("cli_spec.json")).unwrap();
        for option in options.as_array().unwrap() {
            let flags = option["flags"].as_str().unwrap();
            let id = arg(flags).get_id().to_string();
            assert!(
                ROOT_RUN_FLAGS.contains(&id.as_str())
                    || excluded_flag(&id).is_some()
                    || matches!(id.as_str(), "perf-trace" | "perf-trace-path"),
                "{flags}"
            );
        }
    }
    #[test]
    fn program_cookie_names_and_ports_reach_project_sources() {
        let matches = parser()
            .try_get_matches_from([
                "industrious",
                "--browser-port",
                "9333",
                "--browser-cookie-names",
                " PUBLIC-a ,, PUBLIC-b ",
                "project-sources",
                "list",
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let (_, child) = sub.subcommand().unwrap();
        require_flags(&matches, PROJECT_SOURCE_FLAGS).unwrap();
        let mut overrides = browser_runtime_overrides_at_scope(
            &matches,
            &[child, sub],
            &json!({}),
            &Environment::new(),
        )
        .unwrap();
        let mut input = json!({});
        // Reference parseCookieNames(option ?? ORACLE_BROWSER_COOKIE_NAMES): the option wins.
        let env = Environment::from([(
            "ORACLE_BROWSER_COOKIE_NAMES".into(),
            " PUBLIC-env ,, PUBLIC-env-b ".into(),
        )]);
        sources_cookie_options(&matches, &[child, sub], &mut input, &mut overrides, &env);
        assert_eq!(overrides["debugPort"], 9333);
        assert_eq!(overrides["cookieNames"], json!(["PUBLIC-a", "PUBLIC-b"]));
        let bare = parser()
            .try_get_matches_from(["industrious", "project-sources", "list"])
            .unwrap();
        let mut overrides = json!({});
        sources_cookie_options(&bare, &[], &mut input, &mut overrides, &env);
        assert_eq!(
            overrides["cookieNames"],
            json!(["PUBLIC-env", "PUBLIC-env-b"])
        );
        let mut overrides = json!({});
        sources_cookie_options(&bare, &[], &mut input, &mut overrides, &Environment::new());
        assert!(overrides.get("cookieNames").is_none());
        let blank = parser()
            .try_get_matches_from([
                "industrious",
                "--browser-cookie-names",
                " , ",
                "project-sources",
                "list",
            ])
            .unwrap();
        // A given blank option keeps the configured names and never reads the environment.
        let mut overrides = json!({});
        sources_cookie_options(&blank, &[], &mut input, &mut overrides, &env);
        assert!(overrides.get("cookieNames").is_none());
        let blank_env = Environment::from([("ORACLE_BROWSER_COOKIE_NAMES".into(), " , ".into())]);
        let mut overrides = json!({});
        sources_cookie_options(&bare, &[], &mut input, &mut overrides, &blank_env);
        assert!(overrides.get("cookieNames").is_none());
        // The MCP tool has no option, so the environment applies.
        assert_eq!(
            crate::project_sources::cookie_names(None, &env),
            Some(json!(["PUBLIC-env", "PUBLIC-env-b"]))
        );
    }
}
#[cfg(test)]
mod copy_tests {
    use super::*;
    #[test]
    fn copy_reports_follow_the_reference_summary_and_soft_failure() {
        let failed = copy_with(&[("PUBLIC-missing-clipboard-command", &[])], "PUBLIC");
        assert_eq!(
            copy_report(&failed, 12, 0),
            "Copy failed (no usable native clipboard command); markdown not printed. Re-run with --render-markdown if you need the content."
        );
        assert_eq!(
            copy_report(&Ok(()), 1500, 2),
            "Copied markdown to clipboard (~1.5k tokens; 2 files)."
        );
        assert_eq!(
            copy_report(&Ok(()), 999, 0),
            "Copied markdown to clipboard (~999 tokens)."
        );
    }
}
#[cfg(test)]
mod notification_flag_tests {
    use super::*;
    #[test]
    fn later_notification_flags_win_as_in_the_reference() {
        for (args, expected) in [
            (vec![], json!({})),
            (vec!["--notify"], json!({"notify":true})),
            (vec!["--no-notify"], json!({"notify":false})),
            (vec!["--notify", "--no-notify"], json!({"notify":false})),
            (vec!["--no-notify", "--notify"], json!({"notify":true})),
            (
                vec!["--notify-sound", "--no-notify", "--no-notify-sound"],
                json!({"notify":false,"notifySound":false}),
            ),
        ] {
            let mut argv = vec!["industrious", "-p", "PUBLIC notify flags"];
            argv.extend(args.iter().copied());
            let matches = parser().try_get_matches_from(&argv).unwrap();
            let mut input = json!({});
            notification_flags(&matches, &mut input);
            assert_eq!(input, expected, "{args:?}");
        }
    }
}
#[cfg(test)]
mod heartbeat_option_tests {
    use super::{Command, cancelled_inspection, heartbeat_seconds, parser};
    #[test]
    fn heartbeat_option_follows_reference_parse_heartbeat_option() {
        for (value, seconds) in [
            ("", 30.0),
            (" ", 30.0),
            ("off", 0.0),
            ("FALSE", 0.0),
            ("0", 0.0),
            ("1.5", 1.5),
            ("12abc", 12.0),
            ("2e1", 20.0),
        ] {
            assert_eq!(heartbeat_seconds(value).unwrap(), seconds, "{value:?}");
        }
        for value in ["-1", "abc", "infinity", "NaN"] {
            assert_eq!(
                heartbeat_seconds(value).unwrap_err().to_string(),
                "Heartbeat interval must be zero or a positive number.",
                "{value:?}"
            );
        }
    }
    #[test]
    fn cancelled_inspections_report_the_reference_notice_and_keep_other_failures() {
        assert_eq!(
            cancelled_inspection(Err(anyhow::anyhow!("Browser inspection cancelled"))),
            "Browser inspection cancelled."
        );
        assert_eq!(cancelled_inspection(Ok(0)), "Browser inspection cancelled.");
        assert_eq!(
            cancelled_inspection(Err(
                anyhow::anyhow!("PUBLIC cleanup failed").context("PUBLIC inspection")
            )),
            "PUBLIC inspection: PUBLIC cleanup failed"
        );
    }
    #[test]
    fn subcommand_help_describes_every_visible_option_and_matches_root_help() {
        let root_help = include_str!("../fixtures/cli-help.txt")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        fn walk(command: &Command, path: &str, root_help: &str) {
            let about = command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(!about.is_empty(), "{path} has no about");
            if !path.contains(' ') {
                assert!(
                    root_help.contains(&about),
                    "{path} about differs from root help"
                );
            }
            for argument in command.get_arguments() {
                if !argument.is_hide_set()
                    && argument.get_id() != "help"
                    && argument.get_long().is_some()
                {
                    assert!(
                        argument
                            .get_help()
                            .is_some_and(|help| !help.to_string().is_empty()),
                        "{path} --{} has no help",
                        argument.get_id()
                    );
                }
            }
            for child in command.get_subcommands() {
                walk(child, &format!("{path} {}", child.get_name()), root_help);
            }
        }
        for command in parser().get_subcommands() {
            walk(command, command.get_name(), &root_help);
        }
        let mut serve = parser().find_subcommand("serve").unwrap().clone();
        let help = serve.render_long_help().to_string();
        assert!(
            help.contains("--host <address>") && !help.contains("--remote-chrome"),
            "{help}"
        );
    }
}
