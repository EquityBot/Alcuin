//! Root CLI wording and option contracts matched to the reference Commander CLI.
use alcuin::{sessions::SessionStore, storage};
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_alcuin"));
    command
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ORACLE_HOME_DIR", home)
        .current_dir(home);
    command
}
fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    command(home).args(args).output().unwrap()
}
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}
fn help() -> String {
    include_str!("../fixtures/cli-help.txt").replace("\r\n", "\n")
}

#[test]
fn version_prints_the_bare_package_version() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["--version", "-V"] {
        let output = cli(home.path(), &[flag]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(stdout(&output), format!("{}\n", env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn legacy_negatable_spellings_map_to_the_positive_flags() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["--[no-]notify", "--[no-]notify-sound", "--[no-]background"] {
        let output = cli(
            home.path(),
            &["-p", "PUBLIC legacy alias", flag, "--dry-run", "summary"],
        );
        assert_eq!(output.status.code(), Some(0), "{flag}: {}", stderr(&output));
    }
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn disabling_provider_native_capture_is_accepted() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(
        home.path(),
        &[
            "-p",
            "PUBLIC no capture",
            "--no-browser-capture-provider-native",
            "--dry-run",
            "summary",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn browser_health_help_describes_the_command_not_the_root() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["browser-health", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout(&output).contains("browser-health [OPTIONS]"),
        "{}",
        stdout(&output)
    );
    assert_ne!(stdout(&output), help());
}

#[test]
fn zero_arguments_print_the_hint_and_help_and_missing_prompts_fail_with_help() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        format!(
            "No prompt or subcommand supplied. Run `alcuin --help` for usage.\n{}",
            help()
        )
    );
    for args in [
        vec!["--engine", "api"],
        vec!["--route", "--dry-run"],
        vec!["--preflight", "--render-markdown"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(
            stdout(&output),
            "Prompt is required. Provide it via --prompt \"<text>\" or positional [prompt].\n",
            "{args:?}"
        );
        assert_eq!(stderr(&output), help(), "{args:?}");
    }
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn excluded_features_refuse_with_the_unsupported_reason() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["tui"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("alcuin tui is not supported in this build"),
        "{}",
        stderr(&output)
    );
    let output = command(home.path())
        .env("ORACLE_FORCE_TUI", "1")
        .args(["-p", "PUBLIC forced terminal UI", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("ORACLE_FORCE_TUI=1 requests the terminal UI"));
    for (args, flag) in [
        (
            vec!["-p", "PUBLIC pruning", "--retain-hours", "1"],
            "--retain-hours",
        ),
        (vec!["session", "--clear"], "--clear"),
        (vec!["status", "--clean"], "--clean"),
        (
            vec!["-p", "PUBLIC recheck", "--browser-recheck-delay", "1s"],
            "--browser-recheck-delay",
        ),
        (
            vec![
                "-p",
                "PUBLIC reattach",
                "--browser-auto-reattach-timeout",
                "1s",
            ],
            "--browser-auto-reattach-timeout",
        ),
        (
            vec!["project-sources", "list", "--browser-reuse-wait", "1s"],
            "--browser-reuse-wait",
        ),
        (
            vec!["-p", "PUBLIC approval", "--browser-approval-wait", "30s"],
            "--browser-approval-wait",
        ),
        (
            vec![
                "-p",
                "PUBLIC capture",
                "--browser-capture-provider-native",
                "--dry-run",
            ],
            "--browser-capture-provider-native",
        ),
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let text = stderr(&output);
        assert!(
            text.contains(&format!("{flag} is not supported in this build: ")),
            "{args:?}: {text}"
        );
        assert!(!text.contains("gated"), "{args:?}: {text}");
    }
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn duration_options_use_the_reference_parser_and_labelled_errors() {
    let home = tempfile::tempdir().unwrap();
    for (flag, value, label) in [
        ("--http-timeout", "1.5s", "HTTP timeout"),
        ("--http-timeout", "+5s", "HTTP timeout"),
        ("--zombie-timeout", "1e3", "Zombie timeout"),
    ] {
        let output = cli(home.path(), &["-p", "PUBLIC duration", flag, value]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            format!(
                "error: option '{flag} <ms|s|m|h>' argument '{value}' is invalid. {label} must be a positive duration like 30m, 10s, 500ms, or 2h.\n(use --help for usage)\n"
            )
        );
    }
    let output = cli(home.path(), &["-p", "PUBLIC duration", "--timeout", "1.5m"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Timeout must be a positive number of seconds"));
    for args in [
        ["--http-timeout", "1h30m"],
        ["--zombie-timeout", "2m 30s"],
        ["--timeout", "1h30m"],
        ["--timeout", "2.5"],
    ] {
        let mut argv = vec!["-p", "PUBLIC duration", "--dry-run", "summary"];
        argv.extend(args);
        let output = cli(home.path(), &argv);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn parse_failures_use_commander_wording() {
    let home = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (
            vec!["--promt", "PUBLIC"],
            "error: unknown option '--promt'\n(Did you mean --prompt?)\n(use --help for usage)\n",
        ),
        (
            vec!["status", "--hourz", "2"],
            "error: unknown option '--hourz'\n(Did you mean --hours?)\n(use --help for usage)\n",
        ),
        (
            vec!["-p", "PUBLIC", "--model"],
            "error: option '-m, --model <model>' argument missing\n(use --help for usage)\n",
        ),
        (
            vec!["restart"],
            "error: missing required argument 'id'\n(use --help for usage)\n",
        ),
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }
}

#[test]
fn profile_lock_and_environment_attachment_timeouts_reach_the_saved_browser_config() {
    let home = tempfile::tempdir().unwrap();
    storage::atomic_json(&home.path().join("config.json"),&json!({"engine":"browser","browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("PUBLIC empty profile"),"cookieSync":false,"chromePath":"PUBLIC configured.exe"}})).unwrap();
    let output = command(home.path())
        .env("ORACLE_BROWSER_ATTACHMENT_TIMEOUT", " 2m ")
        .args([
            "--engine",
            "browser",
            "--prompt",
            "PUBLIC lock timeout",
            "--wait",
            "--browser-profile-lock-timeout",
            "1m30s",
            "--write-output",
            "   ",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!stderr(&output).contains("gated"), "{}", stderr(&output));
    let store = SessionStore::new(home.path());
    let saved: Vec<Value> = store
        .list(false)
        .unwrap()
        .into_iter()
        .filter(|entry| entry["options"]["providerAttempt"].is_null())
        .collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0]["browser"]["config"]["profileLockTimeoutMs"],
        90_000
    );
    assert_eq!(
        saved[0]["browser"]["config"]["attachmentTimeoutMs"],
        120_000
    );
    // Reference resolveOutputPath ignores a blank --write-output.
    assert!(saved[0]["options"]["writeOutputPath"].is_null());
}

#[test]
fn render_prints_only_the_bundle_and_warns_when_it_is_oversized() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(home.path().join("b.rs"), "pub fn b() {}\n").unwrap();
    let output = cli(
        home.path(),
        &[
            "-p",
            "PUBLIC render",
            "--include",
            "b.rs",
            "--file",
            "a.rs",
            "--render",
            "--write-output",
            "PUBLIC-render.md",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("Ignoring duplicate --file inputs: b.rs\n"),
        "{text}"
    );
    let a = text.find("### File: a.rs").unwrap();
    let b = text.find("### File: b.rs").unwrap();
    assert!(a < b, "{text}");
    assert!(!text.contains("Warning: bundle"));
    assert!(!home.path().join("PUBLIC-render.md").exists());
    std::fs::write(home.path().join("large.txt"), " a".repeat(250_000)).unwrap();
    let output = cli(
        home.path(),
        &[
            "-p",
            "PUBLIC large render",
            "--file",
            "large.txt",
            "--max-file-size-bytes",
            "4000000",
            "--render",
            "--write-output",
            " ",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let warning = text.lines().next().unwrap();
    assert!(
        warning.starts_with("Warning: bundle is ~")
            && warning.ends_with(" tokens (>196,000); may exceed model limits."),
        "{warning}"
    );
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn root_validations_use_the_reference_messages() {
    let home = tempfile::tempdir().unwrap();
    for (args, message) in [
        (
            vec![
                "-p",
                "PUBLIC",
                "--models",
                "gpt-6-astra",
                "--model",
                "gpt-6-astra",
                "--dry-run",
            ],
            "--models cannot be combined with --model.",
        ),
        (
            vec![
                "-p",
                "PUBLIC",
                "--provider",
                "azure",
                "--no-azure",
                "--dry-run",
            ],
            "--provider azure cannot be combined with --no-azure.",
        ),
        (
            vec![
                "-p",
                "PUBLIC",
                "--engine",
                "api",
                "--browser-follow-up",
                "next",
                "--dry-run",
            ],
            "--browser-follow-up requires --engine browser.",
        ),
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&output), format!("✖ {message}\n"), "{args:?}");
    }
    let output = cli(home.path(), &["-p", "PUBLIC", "--browser", "--render"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("`--browser` is deprecated; use `--engine browser` instead.\n")
    );
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn route_and_preflight_print_reference_plans_and_exit_on_readiness() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["-p", "PUBLIC route", "--route"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Route plan\n\ngpt-6-astra: not ready\n  provider: OpenAI\n  base: api.openai.com\n  key: missing\n  error: Missing OPENAI_API_KEY.\n\n"
    );
    let output = command(home.path())
        .env("OPENAI_API_KEY", "sk-PUBLIC-synthetic-1234")
        .args(["--preflight"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Provider preflight\n\ngpt-6-astra: ok\n  provider: OpenAI\n  base: api.openai.com\n  key: OPENAI_API_KEY=sk-P****1234\n\n"
    );
    let output = command(home.path())
        .env("AZURE_OPENAI_ENDPOINT", "https://public-azure.invalid")
        .env("AZURE_OPENAI_API_KEY", "PUBLIC-SYNTHETIC-KEY")
        .args(["--route", "--provider", "azure"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with(
            "Route plan\n\ngpt-6-astra: not ready\n  provider: Azure OpenAI\n  base: public-azure.invalid\n  key: AZURE_OPENAI_API_KEY=PUBL****-KEY\n  azure deployment: none\n  azure: active because Azure endpoint is configured\n  error: Azure mode requires --azure-deployment"
        ),
        "{text}"
    );
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn doctor_requires_providers_and_prints_reference_readiness() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(root.path().join(".oracle")).unwrap();
    std::fs::write(
        root.path().join(".oracle/config.json"),
        r#"{"apiBaseUrl":"https://PUBLIC-parent.invalid/v1"}"#,
    )
    .unwrap();
    let output = cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        "Run `alcuin doctor --providers` to inspect API provider readiness.\n"
    );
    let output = cli(&home, &["doctor", "--providers", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let expected = json!({"providers":[{"model":"gpt-6-astra","ok":false,"provider":"openai","providerLabel":"OpenAI","base":"api.openai.com","keySource":"OPENAI_API_KEY","keyPreview":"missing","keyPresent":false,"isAzureOpenAI":false,"azureConfigured":false,"error":"Missing OPENAI_API_KEY."}]});
    assert_eq!(
        stdout(&output),
        format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())
    );
    storage::atomic_json(
        &home.join("config.json"),
        &json!({"apiBaseUrl":"https://PUBLIC-gateway.invalid/v1/chat"}),
    )
    .unwrap();
    let output = command(&home)
        .env("OPENAI_API_KEY", "sk-PUBLIC-synthetic-1234")
        .args(["doctor", "--providers", "--models", "gpt-6-astra, Latest"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Provider readiness\n\ngpt-6-astra: ok\n  provider: OpenAI-compatible\n  base: public-gateway.invalid/v1/...\n  key: OPENAI_API_KEY=sk-P****1234\n\n"
    );
    let output = cli(
        &home,
        &["doctor", "--providers", "--provider", "azure", "--no-azure"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "✖ --provider azure cannot be combined with --no-azure.\n"
    );
    let _: Value = json!(null);
}

fn finished_session(store: &SessionStore, home: &Path, slug: &str, patch: Value) -> String {
    let meta = store
        .create(
            json!({"prompt":format!("PUBLIC {slug} prompt"),"slug":slug,"model":"gpt-6-astra","mode":"api"}),
            home,
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    store.update(&id, |_| Ok(Some(patch))).unwrap();
    id
}

const STATUS_EXAMPLES: &str = "\nUsage Examples\n  alcuin status --hours 72 --limit 50\n    Show 72h of history capped at 50 entries.\n  alcuin session <session-id>\n    Attach to a specific running/completed session to stream its output.\n";

#[test]
fn session_views_list_and_attach_with_reference_lines() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path(), &["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("No stored sessions found.\n{STATUS_EXAMPLES}")
    );
    let output = cli(home.path(), &["status", "--hours", "5"]);
    assert_eq!(stdout(&output), "No stored sessions found.\n");

    let store = SessionStore::new(home.path());
    let parent = finished_session(
        &store,
        home.path(),
        "public-parent-run",
        json!({"status":"completed","elapsedMs":1500,
            "usage":{"inputTokens":2200,"outputTokens":1200,"reasoningTokens":0,"totalTokens":3400,"cost":0.0822},
            "response":{"responseId":"resp_PUBLIC_parent_response_1234","status":"completed"}}),
    );
    store
        .append_log(
            &parent,
            Some("gpt-6-astra"),
            "PUBLIC preamble\nAnswer:\nPUBLIC answer\n",
        )
        .unwrap();
    let child = finished_session(
        &store,
        home.path(),
        "public-child-run",
        json!({"status":"error","errorMessage":"PUBLIC failure",
            "options":{"prompt":"PUBLIC child prompt","previousResponseId":"resp_PUBLIC_parent_response_1234","file":[]}}),
    );

    let output = cli(home.path(), &["status", "--limit", "5"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0], "Recent Sessions");
    assert_eq!(
        lines[1],
        "Status    Model         Mode    Timestamp           Chars    Cost  Slug"
    );
    assert!(
        lines[2].starts_with("completed gpt-6-astra   api     "),
        "{text}"
    );
    assert!(
        lines[2].ends_with("   31  $0.082  public-parent-run"),
        "{text}"
    );
    assert!(
        lines[3].starts_with("error     gpt-6-astra   api     "),
        "{text}"
    );
    assert!(
        lines[3].ends_with("   19       -  └─ public-child-run"),
        "{text}"
    );

    let output = cli(home.path(), &["status", "--limit", "1"]);
    let text = stdout(&output);
    assert!(
        text.contains("  public-child-run <- public-parent-run (resp_PUBLIC...1234)\n"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "Showing 1 of 2 sessions from the requested range. Rerun with --limit <count> or --all to see more.\n"
        ),
        "{text}"
    );

    let output = cli(home.path(), &["session", &parent, "--render"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let created = store.require(&parent).unwrap()["createdAt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        text,
        format!(
            "Chain: root -> {parent}\nCreated: {created}\nStatus: completed\nModels:\n- gpt-6-astra — pending\nResponse: response=resp_PUBLIC_parent_response_1234 | status=completed\nPrompt:\nPUBLIC public-parent-run prompt\n---\nRender requested but stdout is not a TTY; showing raw text.\nAnswer:\nPUBLIC answer\n1.5s · $0.0822 · gpt-6-astra · ↑2.2k ↓1.2k ↻0 Δ3.4k | slug={parent}\n"
        )
    );

    let output = cli(
        home.path(),
        &[
            "--engine",
            "api",
            "--wait",
            "session",
            &child,
            "--hide-prompt",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with(&format!(
            "Ignoring flags on session attach: engine, wait\nChain: {parent} (resp_PUBLIC...1234) -> {child}\n"
        )),
        "{text}"
    );
    assert!(!text.contains("Prompt:"), "{text}");

    let output = cli(home.path(), &["--status", "--session", &parent]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).starts_with(&format!("Chain: root -> {parent}\n")));
    let output = cli(home.path(), &["--status", "--file", "public.rs"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("Recent Sessions\n"));
    assert!(stdout(&output).ends_with(STATUS_EXAMPLES));
    let output = cli(home.path(), &["--retain-hours", "5", "status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--retain-hours is not supported in this build"));

    for flag in ["--harvest", "--live"] {
        let output = cli(home.path(), &["session", flag]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            format!("✖ The {flag} flag requires a session ID.\n")
        );
    }
}

#[test]
fn file_size_limit_reads_the_environment_like_the_reference() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("public.txt"), "PUBLIC ".repeat(8)).unwrap();
    let run = |limit: &str, extra: &[&str]| {
        let mut args = vec![
            "-p",
            "PUBLIC size",
            "--file",
            "public.txt",
            "--render-markdown",
        ];
        args.extend_from_slice(extra);
        command(home.path())
            .env("ORACLE_MAX_FILE_SIZE_BYTES", limit)
            .args(args)
            .output()
            .unwrap()
    };
    for limit in ["  ", " 5000 ", "5000kb"] {
        let output = run(limit, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{limit:?}: {}",
            stderr(&output)
        );
    }
    let output = run("20", &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("public.txt"),
        "{}",
        stderr(&output)
    );
    let output = run("20", &["--max-file-size-bytes", "5000"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    for limit in ["-5", "PUBLIC"] {
        let output = run(limit, &[]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "✖ ORACLE_MAX_FILE_SIZE_BYTES must be a positive integer number of bytes.\n"
        );
    }
}

#[test]
fn duplicate_prompts_compare_trimmed_text_and_print_the_reference_notice() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let running = store
        .create(
            json!({"prompt":"PUBLIC duplicate guard prompt\n","slug":"public-duplicate-guard","model":"gpt-6-astra","mode":"api"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = running["id"].as_str().unwrap().to_owned();
    store
        .update(&id, |_| Ok(Some(json!({"status":"running"}))))
        .unwrap();
    let output = command(home.path())
        .env("OPENAI_API_KEY", "sk-PUBLIC-synthetic-1234")
        .args(["-p", "  PUBLIC duplicate guard prompt  ", "--engine", "api"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    assert_eq!(
        stdout(&output),
        format!(
            "A session with the same prompt is already running ({id}). Reattach with \"alcuin session {id}\" or rerun with --force to start another run.\n"
        )
    );
    assert_eq!(store.list(false).unwrap().len(), 1);
}

#[test]
fn excluded_feature_config_keys_refuse_with_the_unsupported_reason() {
    let home = tempfile::tempdir().unwrap();
    for (config, message) in [
        (
            json!({"sessionRetentionHours":24}),
            "config key sessionRetentionHours is not supported in this build: session pruning is not included; stored sessions are kept for recovery.",
        ),
        (
            json!({"browser":{"assistantRecheckDelayMs":1000}}),
            "config key browser.assistantRecheckDelayMs is not supported in this build: delayed assistant rechecks are not included; recover with `alcuin session <id>`.",
        ),
        (
            json!({"browser":{"autoReattachIntervalMs":1000}}),
            "config key browser.autoReattachIntervalMs is not supported in this build: automatic reattach is not included; recover with `alcuin session <id>`.",
        ),
        (
            json!({"browser":{"reuseChromeWaitMs":1000}}),
            "config key browser.reuseChromeWaitMs is not supported in this build: waiting to reuse a starting Chrome is not included.",
        ),
    ] {
        storage::atomic_json(&home.path().join("config.json"), &config).unwrap();
        let output = cli(home.path(), &["-p", "PUBLIC excluded config", "--dry-run"]);
        assert_eq!(output.status.code(), Some(1), "{config}");
        assert_eq!(stderr(&output), format!("✖ {message}\n"));
    }
    storage::atomic_json(
        &home.path().join("config.json"),
        &json!({"browser":{"assistantRecheckDelayMs":null}}),
    )
    .unwrap();
    let output = cli(home.path(), &["-p", "PUBLIC excluded config", "--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn config_loads_skip_directory_candidates_and_always_report_unreadable_files() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".oracle/config.json")).unwrap();
    let output = command(&home)
        .current_dir(&project)
        .args(["-p", "PUBLIC config directory", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    std::fs::write(home.join("config.json"), "{ PUBLIC").unwrap();
    let output = cli(&home, &["doctor", "--providers"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "Failed to read {}: invalid JSON5 configuration\n",
            home.join("config.json").display()
        )
    );
}

#[test]
fn project_sources_print_the_reference_summary_and_pretty_json() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("PUBLIC.rs"), b"PUBLIC source").unwrap();
    let args = [
        "project-sources",
        "add",
        "--chatgpt-url",
        "https://chatgpt.com/g/g-p-public/project",
        "--file",
        "PUBLIC.rs",
        "--dry-run",
    ];
    let output = cli(home.path(), &args);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Project Sources add dry run\nProject: https://chatgpt.com/g/g-p-public/project?tab=sources\nPlanned uploads: 1\n  batch 1: PUBLIC.rs\n"
    );
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let output = cli(home.path(), &json_args);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        text,
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
    );
    assert_eq!(value["status"], "dry-run");
    let mut lock_args = args.to_vec();
    lock_args.extend(["--browser-profile-lock-timeout", "5s"]);
    let output = cli(home.path(), &lock_args);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
// Commander's program parser takes program options wherever they are typed, so a program option
// after a command is applied or ignored exactly as it is before it; clap errors remain only
// where Commander errors, with its wording.
fn program_options_after_a_command_apply_or_ignore_like_commander() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("PUBLIC-a.txt"), b"PUBLIC a").unwrap();
    std::fs::write(home.path().join("PUBLIC-b.txt"), b"PUBLIC b").unwrap();
    let store = SessionStore::new(home.path());
    let id = finished_session(
        &store,
        home.path(),
        "public-trailing-flag-run",
        json!({"status":"completed","elapsedMs":1500}),
    );
    store
        .append_log(
            &id,
            Some("gpt-6-astra"),
            "PUBLIC trailing answer
",
        )
        .unwrap();
    let hint = "Run `alcuin doctor --providers` to inspect API provider readiness.
";
    let project = "https://chatgpt.com/g/g-p-public/project";
    let version = format!(
        "{}
",
        env!("CARGO_PKG_VERSION")
    );
    let not_tty = "Render requested but stdout is not a TTY";
    let cases: &[(&[&str], i32, &str)] = &[
        (&["doctor", "--wait", "--slug", "PUBLIC words"], 0, hint),
        (
            &[
                "doctor",
                "--providers",
                "-vm",
                "gpt-6-astra",
                "--base-url=http://127.0.0.1:9/v1",
            ],
            1,
            "gpt-6-astra: not ready
  provider: OpenAI-compatible
  base: 127.0.0.1:9/v1
",
        ),
        (&["session", "--version"], 0, &version),
        (
            &["bridge", "codex-config", "--background", "-v"],
            0,
            "[mcp_servers.alcuin]
",
        ),
        (
            &[
                "project-sources",
                "add",
                "--dry-run",
                "-f",
                "PUBLIC-a.txt",
                "--include",
                "PUBLIC-b.txt",
                "--chatgpt-url",
                project,
            ],
            0,
            "Planned uploads: 2
  batch 1: PUBLIC-a.txt
  batch 1: PUBLIC-b.txt
",
        ),
        (
            &["restart", "PUBLIC-missing", "--slug", "PUBLIC words"],
            1,
            "No session found with ID PUBLIC-missing",
        ),
        (
            &["perf-summary", "PUBLIC-missing.json", "--engine", "api"],
            1,
            "perf-summary input 1: unreadable file.",
        ),
        (
            &["docs", "check", "--retain-hours", "5"],
            1,
            "--retain-hours is not supported in this build: ",
        ),
        // The root --path takes a value everywhere except after session, and status ignores it;
        // a variadic program option takes the operands that follow it, as Commander does.
        (
            &["status", "--path", "PUBLIC-a.txt"],
            0,
            "public-trailing-flag-run",
        ),
        (
            &["session", "--file", "PUBLIC-a.txt", &id],
            0,
            STATUS_EXAMPLES,
        ),
        (&["session", &id, "--path"], 0, "Session dir: "),
        (
            &["session", &id, "--model", "gpt-6-astra"],
            0,
            "PUBLIC trailing answer",
        ),
        (&["session", &id, "--render"], 0, not_tty),
        (&["--render-markdown", "session", &id], 0, not_tty),
        (&["status", &id, "--render"], 0, not_tty),
        (
            &["--write-output", "PUBLIC-out.md", "session", &id],
            1,
            "The --write-output flag requires --harvest or --live.",
        ),
        (
            &["session", &id, "--model"],
            1,
            "error: option '-m, --model <model>' argument missing
(use --help for usage)
",
        ),
        (
            &["status", "--path"],
            1,
            "error: option '--path <paths...>' argument missing
(use --help for usage)
",
        ),
        (
            &["doctor", "--wiat"],
            1,
            "error: unknown option '--wiat'
(Did you mean --wait?)
(use --help for usage)
",
        ),
        (
            &["session", &id, "-vx"],
            1,
            "error: unknown option '-x'
(use --help for usage)
",
        ),
        (
            &["session", &id, "--path", "extra"],
            1,
            "error: too many arguments for 'session'. Expected 1 argument but got 2.
",
        ),
    ];
    for (args, code, expected) in cases {
        let output = cli(home.path(), args);
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert_eq!(output.status.code(), Some(*code), "{args:?}: {text}");
        assert!(text.contains(expected), "{args:?}: {text}");
    }
    assert!(!home.path().join("PUBLIC-out.md").exists());
}

#[test]
// Reference bridge doctor: the program parser takes --verbose and -v wherever they are typed, so
// the command's own --verbose is never set, and runBridgeDoctor ignores its options anyway.
fn bridge_doctor_accepts_and_ignores_verbose_like_commander() {
    let home = tempfile::tempdir().unwrap();
    let plain = cli(home.path(), &["bridge", "doctor"]);
    assert_eq!(plain.status.code(), Some(0), "{}", stderr(&plain));
    for args in [
        &["bridge", "doctor", "--verbose"][..],
        &["bridge", "doctor", "-v"],
        &["--verbose", "bridge", "doctor"],
    ] {
        let output = cli(home.path(), args);
        assert_eq!(output.status.code(), plain.status.code(), "{args:?}");
        assert_eq!(stdout(&output), stdout(&plain), "{args:?}");
        assert_eq!(stderr(&output), stderr(&plain), "{args:?}");
    }
}

#[test]
// Commander lists ignored options in parse order because neither option has a default.
fn root_options_after_a_session_view_reach_the_root_parser_like_commander() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let id = finished_session(
        &store,
        home.path(),
        "public-hoisted-flag-run",
        json!({"status":"completed","elapsedMs":1500}),
    );
    let output = cli(
        home.path(),
        &["session", &id, "--wait", "--engine", "api", "--hide-prompt"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("Ignoring flags on session attach: wait, engine\n"),
        "{text}"
    );
    assert!(!text.contains("Prompt:"), "{text}");
    for args in [
        vec!["status", "--verbose-render"],
        vec!["session", id.as_str(), "--verbose-render"],
        vec!["--verbose-render", "--status"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&output).contains("--verbose-render is not supported in this build: "),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
// Commander parses program options anywhere; each command reads only its own and the globals it
// takes through optsWithGlobals, and ignores the rest. No placement reaches a generic gate.
fn program_options_reach_every_command_like_commander() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("PUBLIC-a.txt"), b"PUBLIC a").unwrap();
    std::fs::write(home.path().join("PUBLIC-b.txt"), b"PUBLIC b").unwrap();
    let hint = "Run `alcuin doctor --providers` to inspect API provider readiness.\n";
    let project = "https://chatgpt.com/g/g-p-public/project";
    let cases: &[(&[&str], i32, &str)] = &[
        (&["--slug", "PUBLIC words", "doctor"], 0, hint),
        (
            &[
                "doctor",
                "--browser-timeout",
                "5s",
                "--remote-chrome",
                "127.0.0.1:9",
            ],
            0,
            hint,
        ),
        (
            &[
                "--model",
                "gpt-6-astra",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "doctor",
                "--providers",
            ],
            1,
            "gpt-6-astra: not ready\n  provider: OpenAI-compatible\n  base: 127.0.0.1:9/v1\n",
        ),
        (
            &[
                "--file=PUBLIC-a.txt",
                "--dry-run",
                "summary",
                "--slug",
                "PUBLIC words",
                "project-sources",
                "add",
                "-f",
                "PUBLIC-b.txt",
                "--chatgpt-url",
                project,
            ],
            0,
            "Planned uploads: 2\n  batch 1: PUBLIC-a.txt\n  batch 1: PUBLIC-b.txt\n",
        ),
        (
            &[
                "--background",
                "--slug",
                "PUBLIC words",
                "bridge",
                "codex-config",
            ],
            0,
            "[mcp_servers.alcuin]\n",
        ),
        (
            &[
                "status",
                "--browser-timeout",
                "5s",
                "--browser-thinking-time",
                "pro",
            ],
            0,
            "No stored sessions found.\n",
        ),
        (
            &["--slug", "PUBLIC words", "restart", "PUBLIC-missing"],
            1,
            "No session found with ID PUBLIC-missing",
        ),
        (
            &[
                "--write-output",
                "PUBLIC-out.md",
                "recover-native",
                "--request",
                "PUBLIC-missing.json",
            ],
            1,
            "Native reply request could not be read.",
        ),
        (
            &[
                "--slug",
                "PUBLIC words",
                "perf-summary",
                "PUBLIC-missing.json",
            ],
            1,
            "perf-summary input 1: unreadable file.",
        ),
        (
            &["--slug", "PUBLIC words", "tui"],
            1,
            "alcuin tui is not supported in this build: ",
        ),
        (
            &["--retain-hours", "5", "docs", "check"],
            1,
            "--retain-hours is not supported in this build: ",
        ),
        (
            &["--exec-session", "PUBLIC-id", "--slug", "PUBLIC words"],
            1,
            "--exec-session is an internal worker entry and takes no other options or commands.",
        ),
        (
            &["--exec-session", "PUBLIC-id", "docs", "check"],
            1,
            "--exec-session is an internal worker entry and takes no other options or commands.",
        ),
    ];
    for (args, code, expected) in cases {
        let output = cli(home.path(), args);
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert_eq!(output.status.code(), Some(*code), "{args:?}: {text}");
        assert!(text.contains(expected), "{args:?}: {text}");
        assert!(!text.contains("gated"), "{args:?}: {text}");
    }
    assert!(!home.path().join("PUBLIC-out.md").exists());
    let store = SessionStore::new(home.path());
    let id = finished_session(
        &store,
        home.path(),
        "public-global-flag-run",
        json!({"status":"completed","elapsedMs":1500}),
    );
    let output = cli(home.path(), &["session", &id, "--browser-timeout", "5s"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("Ignoring flags on session attach: browserTimeout\n"),
        "{}",
        stdout(&output)
    );
}
