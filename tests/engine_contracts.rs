use alcuin::config::{Environment, resolve_engine};
use serde_json::{Value, json};
use std::process::Command;
#[test]
fn browser_preview_uses_the_cli_choice_before_runtime_profile_defaults() {
    let home = tempfile::tempdir().unwrap();
    let mut env = Environment::new();
    env.insert(
        "ORACLE_BROWSER_PROFILE_DIR".into(),
        home.path()
            .join("PUBLIC-profile")
            .to_string_lossy()
            .into_owned(),
    );
    let options = json!({"engine":"browser","prompt":"PUBLIC preview defaults"});
    let request = alcuin::execution::resolve(&options, &json!({}), &env, home.path()).unwrap();
    assert_eq!(request.browser["manualLogin"], cfg!(windows));
    assert_eq!(request.browser_preview["manualLogin"], false);
    let request = alcuin::execution::resolve(
        &options,
        &json!({"browser":{"manualLogin":true}}),
        &env,
        home.path(),
    )
    .unwrap();
    assert_eq!(request.browser_preview["manualLogin"], true);
}

#[test]
fn browser_lifecycle_choices_override_configuration() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("PUBLIC-selected-profile");
    let mut environment = Environment::new();
    environment.insert(
        "ORACLE_BROWSER_PROFILE_DIR".into(),
        home.path()
            .join("PUBLIC-environment-profile")
            .to_string_lossy()
            .into_owned(),
    );
    let request=alcuin::execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC native lifecycle choices","browserManualLogin":true,"browserManualLoginProfileDir":profile,"browserHeadless":true,"browserHideWindow":true}),
        &json!({"browser":{"manualLogin":false,"headless":false,"hideWindow":false,"manualLoginProfileDir":"PUBLIC-unused"}}),
        &environment,home.path()).unwrap();
    assert_eq!(request.browser["manualLogin"], true);
    assert_eq!(
        request.browser["manualLoginProfileDir"],
        profile.to_string_lossy().as_ref()
    );
    assert_eq!(request.browser["headless"], true);
    assert_eq!(request.browser["hideWindow"], true);
    assert_eq!(request.options["browserConfig"], request.browser);
    let fallback=alcuin::execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC blank profile path","browserManualLogin":true,"browserManualLoginProfileDir":" \t "}),
        &json!({"browser":{"manualLoginProfileDir":"PUBLIC-shadowed"}}),
        &environment,home.path()).unwrap();
    assert_eq!(
        fallback.browser["manualLoginProfileDir"],
        environment["ORACLE_BROWSER_PROFILE_DIR"]
    );
}

#[test]
fn configuration_and_environment_precedence_match_the_maintained_resolver() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/engine-reference.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let input = &case["input"];
        if input.get("browserFlag").is_some() || input.get("apiProviderRequested").is_some() {
            continue;
        }
        let env: Environment = serde_json::from_value(input["env"].clone()).unwrap();
        let config = json!({"engine":input["configEngine"]});
        assert_eq!(
            resolve_engine(input["engine"].as_str(), &config, &env).unwrap(),
            case["engine"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn legacy_browser_and_explicit_provider_flags_keep_their_cli_priority() {
    let home = tempfile::tempdir().unwrap();
    for (flags, expected) in [
        (vec!["--browser", "--engine", "api"], "browser"),
        (vec!["--provider", "openai"], "api"),
        (vec!["--browser", "--provider", "openai"], "browser"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .env("ORACLE_ENGINE", "browser")
            .current_dir(home.path())
            .args(["--prompt", "PUBLIC engine precedence", "--dry-run", "json"])
            .args(flags)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if expected == "api" {
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .starts_with("[dry-run] Alcuin (0.1.0) would call gpt-6-astra")
            );
        } else {
            let output = String::from_utf8(output.stdout).unwrap();
            // Reference: the --browser deprecation notice precedes the preview on stdout.
            assert!(
                output.starts_with(
                    "`--browser` is deprecated; use `--engine browser` instead.\n[preview] Alcuin"
                ),
                "{output}"
            );
            assert!(output.contains("\"engine\": \"browser\""));
        }
    }
}

#[test]
fn profile_environment_forces_manual_login_only_for_mcp_consultations() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("PUBLIC-env-profile");
    let env = Environment::from([(
        "ORACLE_BROWSER_PROFILE_DIR".into(),
        profile.to_string_lossy().into_owned(),
    )]);
    let options = json!({"engine":"browser","prompt":"PUBLIC profile origin"});
    let config = json!({"browser":{"manualLogin":false}});
    let cli = alcuin::execution::resolve(&options, &config, &env, home.path()).unwrap();
    assert_eq!(cli.browser["manualLogin"], false);
    assert!(cli.browser["manualLoginProfileDir"].is_null());
    let mcp = alcuin::execution::resolve_mcp(&options, &config, &env, home.path()).unwrap();
    assert_eq!(mcp.browser["manualLogin"], true);
    assert_eq!(
        mcp.browser["manualLoginProfileDir"],
        profile.to_string_lossy().as_ref()
    );
}

#[test]
fn browser_config_normalises_like_the_reference_resolver() {
    let home = tempfile::tempdir().unwrap();
    let resolve = |options: Value, config: Value, env: &Environment| {
        let mut input = json!({"engine":"browser","prompt":"PUBLIC normalised browser config"});
        input
            .as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        alcuin::execution::resolve(&input, &config, env, home.path())
    };
    let none = Environment::new();
    // shouldSyncBrowserCookies: a manual-login profile needs cookieSync and manualLoginCookieSync.
    let manual = resolve(
        json!({}),
        json!({"browser":{"manualLogin":true,"cookieSync":false,"manualLoginCookieSync":true}}),
        &none,
    )
    .unwrap();
    assert_eq!(manual.browser["cookieSync"], false);
    let both = resolve(
        json!({}),
        json!({"browser":{"manualLogin":true,"cookieSync":true,"manualLoginCookieSync":true}}),
        &none,
    )
    .unwrap();
    assert_eq!(both.browser["cookieSync"], true);
    let strategy = resolve(
        json!({"browserModelStrategy":" CURRENT "}),
        json!({}),
        &none,
    )
    .unwrap();
    assert_eq!(strategy.browser["modelStrategy"], "current");
    let invalid = resolve(json!({"browserModelStrategy":"foo"}), json!({}), &none)
        .err()
        .unwrap();
    assert_eq!(
        invalid.to_string(),
        r#"Invalid browser model strategy: "foo". Expected "select", "current", or "ignore"."#
    );
    let research = resolve(
        json!({}),
        json!({"browser":{"researchMode":"bogus"}}),
        &none,
    )
    .unwrap();
    assert_eq!(research.browser["researchMode"], "off");
    let env = Environment::from([
        ("ORACLE_BROWSER_ALLOW_COOKIE_ERRORS".into(), " TRUE ".into()),
        ("ORACLE_BROWSER_PORT".into(), "9333".into()),
        ("ORACLE_BROWSER_DEBUG_PORT".into(), "9444".into()),
    ]);
    let from_env = resolve(json!({}), json!({}), &env).unwrap();
    assert_eq!(from_env.browser["allowCookieErrors"], true);
    assert_eq!(from_env.browser["debugPort"], 9333);
    let configured = resolve(json!({}), json!({"browser":{"debugPort":9222}}), &env).unwrap();
    assert_eq!(configured.browser["debugPort"], 9222);
    for raw in ["abc", "0", "65536", "-1"] {
        let env = Environment::from([("ORACLE_BROWSER_DEBUG_PORT".into(), raw.into())]);
        assert!(
            resolve(json!({}), json!({}), &env).unwrap().browser["debugPort"].is_null(),
            "{raw}"
        );
    }
}
