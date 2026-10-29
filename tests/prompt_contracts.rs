use alcuin::{
    config::{self, Environment},
    execution, project_sources, prompts, providers,
};
use serde_json::json;
use std::fs;

#[test]
fn path_aliases_keep_matched_brace_groups_and_reject_excessive_nesting_before_io() {
    assert_eq!(
        prompts::collect_paths(&["src/a,{a,{b,c}}/*.ts, !{x,y}/**,docs".into()]),
        vec!["src/a", "{a,{b,c}}/*.ts", "!{x,y}/**", "docs"]
    );
    assert_eq!(
        prompts::collect_paths(&["{unmatched,a,b".into()]),
        vec!["{unmatched", "a", "b"]
    );
    let root = tempfile::tempdir().unwrap();
    for prefix in ["", "!"] {
        let pattern = format!("{prefix}{}public{}", "{".repeat(33), "}".repeat(33));
        assert_eq!(
            prompts::resolve_files(&[pattern], root.path(), 1024, true)
                .unwrap_err()
                .to_string(),
            "Glob pattern nests braces or parentheses 33 levels deep; the limit is 32."
        );
    }
}
#[test]
fn brace_depth_boundary_selects_real_files_and_keeps_ordinary_excludes_and_gitignore() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["src", "a", "b", "c", "docs"] {
        fs::create_dir(root.path().join(directory)).unwrap();
    }
    for file in [
        "src/main.ts",
        "src/main.test.ts",
        "a/x.ts",
        "b/y.ts",
        "c/z.ts",
        "docs/guide.md",
    ] {
        fs::write(root.path().join(file), file).unwrap();
    }
    fs::write(
        root.path().join(".gitignore"),
        format!("*.log\n{}x{}\n", "{".repeat(40), "}".repeat(40)),
    )
    .unwrap();
    fs::write(root.path().join("src/ignored.log"), "PUBLIC ignored").unwrap();
    let nested = |depth: usize| {
        format!(
            "{}{{a,b}}{}/*.ts",
            "{a,".repeat(depth - 1),
            "}".repeat(depth - 1)
        )
    };
    let selected = |patterns: Vec<String>| {
        prompts::resolve_files(&patterns, root.path(), 1024, true)
            .unwrap()
            .into_iter()
            .map(|file| file.display_path)
            .collect::<Vec<_>>()
    };
    assert_eq!(selected(vec![nested(32)]), vec!["a/x.ts", "b/y.ts"]);
    assert_eq!(
        selected(vec!["{a,b,c}/*.ts".into(), format!("!{}", nested(32))]),
        vec!["c/z.ts"]
    );
    assert_eq!(
        selected(vec!["src/**".into(), "!src/**/*.test.ts".into()]),
        vec!["src/main.ts"]
    );
    assert_eq!(
        selected(vec!["{a,b}/*.ts".into()]),
        vec!["a/x.ts", "b/y.ts"]
    );
    assert_eq!(
        selected(vec!["src/main.ts".into(), "docs".into()]),
        vec!["docs/guide.md", "src/main.ts"]
    );
    for prefix in ["", "!"] {
        let error = prompts::resolve_files(
            &["src/**".into(), format!("{prefix}{}", nested(33))],
            root.path(),
            1024,
            true,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Glob pattern nests braces or parentheses 33 levels deep; the limit is 32."
        );
    }
    let escaped = format!("{}x{}", "\\{".repeat(33), "\\}".repeat(33));
    assert_eq!(
        prompts::resolve_files(&[escaped], root.path(), 1024, true)
            .unwrap_err()
            .to_string(),
        "Glob pattern nests braces or parentheses 33 levels deep; the limit is 32."
    );
    let once = prompts::collect_paths(&["a/x.ts,{a,{b,c}}/*.ts,docs".into(), "src/main.ts".into()]);
    assert_eq!(prompts::collect_paths(&once), once);
}

#[test]
fn render_uses_reference_sections_line_numbers_and_safe_fences() {
    let file = prompts::PromptFile {
        path: "example.rs".into(),
        display_path: "example.rs".into(),
        content: "fn main() {\n    // ``` nested\n}\n".into(),
        size_bytes: 38,
    };
    assert_eq!(
        prompts::render("System", "Prompt", &[file]),
        "[SYSTEM]\nSystem\n\n[USER]\nPrompt\n\n### File: example.rs\nLines: 1-3\n````rust\n1 | fn main() {\n2 |     // ``` nested\n3 | }\n````"
    );
    assert_eq!(
        prompts::render("System", " Prompt  ", &[]),
        "[SYSTEM]\nSystem\n\n[USER]\nPrompt"
    );
}
#[test]
fn file_sections_use_javascript_trailing_whitespace_and_preserve_internal_crlf() {
    let file = prompts::PromptFile {
        path: "public.txt".into(),
        display_path: "public.txt".into(),
        content: "PUBLIC first\r\nsecond\u{85}\u{feff}".into(),
        size_bytes: 0,
    };
    assert_eq!(
        prompts::file_sections(&[file], false, true),
        "### File: public.txt\nLines: 1-2\n```\n1 | PUBLIC first\r\n2 | second\u{85}\n```"
    );
}
#[test]
fn explicit_files_globs_exclusions_and_size_refusal_use_only_the_synthetic_tree() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir(root.join("build")).unwrap();
    fs::write(root.join("src/a.rs"), "PUBLIC a").unwrap();
    fs::write(root.join("src/a.test.rs"), "PUBLIC test").unwrap();
    fs::write(root.join("build/out.rs"), "PUBLIC build").unwrap();
    fs::write(root.join(".hidden.rs"), "PUBLIC hidden").unwrap();
    let files = prompts::resolve_files(
        &["**/*.rs".into(), "!**/*.test.rs".into()],
        root,
        1024,
        true,
    )
    .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].display_path, "src/a.rs");
    assert_eq!(
        prompts::resolve_files(&["build/out.rs".into()], root, 1024, true)
            .unwrap()
            .len(),
        1
    );
    assert!(prompts::resolve_files(&["src/a.rs".into()], root, 2, true).is_err());
    assert!(prompts::resolve_files(&["missing.rs".into()], root, 1024, true).is_err());
    assert!(prompts::resolve_files(&["*.txt".into()], root, 1024, true).is_err());
}
#[test]
fn project_config_cannot_redirect_credentials_routes_or_native_capture() {
    let user = json!({"apiBaseUrl":"https://trusted.example/v1","modelOverrides":{"gpt-6-astra":{"apiModel":"gpt-6-astra"}},"browser":{"remoteHost":"trusted:9473","remoteToken":"synthetic-secret","captureProviderNative":true,"manualLoginProfileDir":"/synthetic/trusted"}});
    let project = json!({"apiBaseUrl":"https://attacker.example/v1","azure":{"endpoint":"https://attacker.example"},"modelOverrides":{"gpt-6-astra":{"apiModel":"retired"}},"browser":{"remoteHost":"attacker:9473","remoteToken":"replacement","captureProviderNative":false,"manualLoginProfileDir":"/attacker","chatgptUrl":"https://attacker.example/project","timeoutMs":100}});
    let merged = config::deep_merge(&user, &config::sanitise_project(&project));
    assert_eq!(merged["apiBaseUrl"], user["apiBaseUrl"]);
    assert_eq!(merged["modelOverrides"], user["modelOverrides"]);
    assert_eq!(merged["browser"]["remoteToken"], "synthetic-secret");
    assert_eq!(merged["browser"]["captureProviderNative"], true);
    assert_eq!(merged["browser"]["timeoutMs"], 100);
    assert!(merged.get("azure").is_none());
    assert!(
        config::sanitise_project(
            &json!({"browser":{"url":"https://chatgpt.com/g/g-p-test/project"}})
        )["browser"]["url"]
            .is_string()
    );
}
#[test]
fn json5_config_merges_outer_projects_before_inner_projects() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let cwd = project.join("inner");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(project.join(".oracle")).unwrap();
    fs::create_dir_all(cwd.join(".oracle")).unwrap();
    fs::write(
        home.join("config.json"),
        "{engine:'api', browser:{remoteToken:'synthetic',timeoutMs:10}}",
    )
    .unwrap();
    fs::write(
        project.join(".oracle/config.json"),
        "{browser:{timeoutMs:20,remoteToken:'attacker'}}",
    )
    .unwrap();
    fs::write(cwd.join(".oracle/config.json"), "{browser:{timeoutMs:30}}").unwrap();
    let loaded = config::load(&home, &cwd, Some(root.path()), true);
    assert_eq!(loaded.paths.len(), 3);
    assert_eq!(loaded.config["browser"]["timeoutMs"], 30);
    assert_eq!(loaded.config["browser"]["remoteToken"], "synthetic");
}
#[test]
fn mcp_output_boundary_refuses_escape_and_symlink_redirection() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    assert!(
        config::mcp_output(
            &home,
            &home.join("generated/image.png"),
            "generateImage",
            false
        )
        .is_ok()
    );
    assert!(
        config::mcp_output(
            &home,
            &home.join("generated/../config.json"),
            "generateImage",
            false
        )
        .is_err()
    );
    assert!(
        config::mcp_output(
            &home,
            &root.path().join("outside.png"),
            "generateImage",
            false
        )
        .is_err()
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path(), home.join("generated")).unwrap();
        assert!(
            config::mcp_output(
                &home,
                &home.join("generated/image.png"),
                "generateImage",
                false
            )
            .is_err()
        );
    }
}
#[test]
fn azure_options_resolve_per_field_from_option_environment_then_user_config() {
    let config = json!({"endpoint":"https://config.invalid","deployment":"config-deployment","apiVersion":"config-version"});
    let env = Environment::from([("AZURE_OPENAI_DEPLOYMENT".into(), "env-deployment".into())]);
    let explicit = json!({"apiVersion":"2025-04-01-preview"});
    for skip_blank in [false, true] {
        assert_eq!(
            providers::resolve_azure(&explicit, &config, &env, skip_blank),
            Some(
                json!({"endpoint":"https://config.invalid","deployment":"env-deployment","apiVersion":"2025-04-01-preview"})
            )
        );
        assert_eq!(
            providers::resolve_azure(&explicit, &json!(null), &env, skip_blank),
            None
        );
    }
    // Runs take an explicit value as given and skip only empty fallbacks, as the reference CLI does.
    let blank = Environment::from([("AZURE_OPENAI_ENDPOINT".into(), "  ".into())]);
    assert_eq!(
        providers::resolve_azure(&json!({}), &config, &blank, false).unwrap()["endpoint"],
        "  "
    );
    assert_eq!(
        providers::resolve_azure(&json!({"endpoint":""}), &config, &env, false),
        None
    );
    // Route plans and the doctor skip blank values at every level.
    assert_eq!(
        providers::resolve_azure(&json!({"endpoint":" "}), &config, &blank, true).unwrap()["endpoint"],
        "https://config.invalid"
    );

    let root = tempfile::tempdir().unwrap();
    let user = json!({"azure":config});
    let input = json!({"prompt":"PUBLIC Azure version","engine":"api","azure":explicit});
    let request = execution::resolve(&input, &user, &env, root.path()).unwrap();
    assert_eq!(
        request.options["azure"],
        json!({"endpoint":"https://config.invalid","deployment":"env-deployment","apiVersion":"2025-04-01-preview"})
    );
    let mut openai = input.clone();
    openai["provider"] = json!("openai");
    let request = execution::resolve(&openai, &user, &env, root.path()).unwrap();
    assert!(request.options.get("azure").is_none());
}
#[test]
fn an_azure_endpoint_selects_the_api_only_when_the_engine_defaulted_to_browser() {
    let root = tempfile::tempdir().unwrap();
    let engine = |input: serde_json::Value, config: serde_json::Value, extra: &[(&str, &str)]| {
        let mut env = Environment::new();
        for (key, value) in extra {
            env.insert((*key).into(), (*value).into());
        }
        let mut input = input;
        input["prompt"] = json!("PUBLIC Azure engine");
        execution::resolve(&input, &config, &env, root.path())
            .unwrap()
            .engine
    };
    let endpoint = ("AZURE_OPENAI_ENDPOINT", "https://public-azure.invalid");
    let none = json!({});
    assert_eq!(engine(none.clone(), none.clone(), &[endpoint]), "api");
    assert_eq!(
        engine(
            none.clone(),
            json!({"azure":{"endpoint":"https://public-azure.invalid"}}),
            &[]
        ),
        "api"
    );
    assert_eq!(engine(none.clone(), none.clone(), &[]), "browser");
    assert_eq!(
        engine(
            none.clone(),
            none.clone(),
            &[("AZURE_OPENAI_ENDPOINT", " ")]
        ),
        "browser"
    );
    for (input, config, extra) in [
        (json!({"engine":"browser"}), none.clone(), vec![endpoint]),
        (json!({"remoteBrowser":true}), none.clone(), vec![endpoint]),
        (json!({"provider":"openai"}), none.clone(), vec![endpoint]),
        (none.clone(), json!({"engine":"browser"}), vec![endpoint]),
        (
            none.clone(),
            none.clone(),
            vec![endpoint, ("ORACLE_ENGINE", " Browser ")],
        ),
    ] {
        assert_eq!(engine(input.clone(), config, &extra), "browser", "{input}");
    }
}
#[test]
fn route_resolution_keeps_azure_and_gateway_authentication_separate() {
    let env = Environment::from([
        ("OPENAI_API_KEY".into(), "synthetic-openai-key".into()),
        ("AZURE_OPENAI_API_KEY".into(), "synthetic-azure-key".into()),
        ("OPENROUTER_API_KEY".into(), "synthetic-router-key".into()),
    ]);
    let options = json!({"azure":{"endpoint":"https://example.openai.azure.com","deployment":"astra-deployment"},"baseUrl":"https://openrouter.ai/api/v1"});
    let (route, key) = providers::resolve_route("gpt-6-astra", &options, &env).unwrap();
    assert_eq!(route.key_source, "AZURE_OPENAI_API_KEY");
    assert_eq!(key.unwrap().expose(), "synthetic-azure-key");
    assert!(route.base_url.ends_with("/openai/v1"));
    let (route, _) = providers::resolve_route("openai/gpt-6-astra", &json!({}), &env).unwrap();
    assert_eq!(route.key_source, "OPENROUTER_API_KEY");
    assert_eq!(route.transport, providers::ApiTransport::ChatCompletions);
    let (route, key) = providers::resolve_route(
        "gpt-6-astra",
        &json!({"baseUrl":"http://127.0.0.1:8080/v1"}),
        &env,
    )
    .unwrap();
    assert_eq!(route.key_source, "OPENAI_API_KEY");
    assert!(!format!("{key:?}").contains("synthetic-openai-key"));
    assert!(
        providers::resolve_route(
            "gpt-6-astra",
            &json!({"provider":"azure"}),
            &Environment::new()
        )
        .is_err()
    );
    assert!(
        providers::resolve_route(
            "gpt-6-astra",
            &json!({"azure":{"endpoint":"https://example.openai.azure.com"}}),
            &env
        )
        .is_err()
    );
    assert_eq!(
        providers::route_target(
            "https://user:synthetic-password@example.com/v1/private?key=synthetic"
        ),
        "example.com/v1/..."
    );
}
#[test]
fn api_request_and_model_defaults_preserve_current_admission() {
    let root = tempfile::tempdir().unwrap();
    let env = Environment::new();
    let browser = execution::resolve(
        &json!({"prompt":"PUBLIC consultation","engine":"browser"}),
        &json!({}),
        &env,
        root.path(),
    )
    .unwrap();
    assert_eq!(browser.options["model"], "gpt-6-pro");
    assert_eq!(browser.browser["thinkingTime"], "pro");
    let api = execution::resolve(
        &json!({"prompt":"PUBLIC consultation","engine":"api","model":"Latest"}),
        &json!({}),
        &env,
        root.path(),
    )
    .unwrap();
    assert_eq!(api.options["model"], "gpt-6-astra");
    assert_eq!(api.options["reasoningMode"], "pro");
    for model in ["gpt-5.5-pro", "claude", "gpt-6-pro"] {
        assert!(
            execution::resolve(
                &json!({"prompt":"PUBLIC consultation","engine":"api","model":model}),
                &json!({}),
                &env,
                root.path()
            )
            .is_err()
        );
    }
    let body = providers::build_request(
        "gpt-6-astra",
        &json!({"reasoningMode":"pro","previousResponseId":"resp_public","maxOutput":100}),
        "PUBLIC user",
        prompts::DEFAULT_SYSTEM,
    )
    .unwrap();
    assert_eq!(body["reasoning"], json!({"mode":"pro","effort":"xhigh"}));
    assert_eq!(body["tools"], json!([{"type":"web_search"}]));
    assert_eq!(body["previous_response_id"], "resp_public");
    assert_eq!(body["store"], true);
    assert!(
        providers::build_request(
            "gpt-6-astra",
            &json!({"reasoningMode":"standard"}),
            "PUBLIC",
            prompts::DEFAULT_SYSTEM
        )
        .is_err()
    );
}
#[test]
fn project_sources_plan_counts_duplicates_and_batches_ten() {
    assert_eq!(
        project_sources::normalise_url("https://chatgpt.com/g/g-p-123/project?tab=chats&foo=bar")
            .unwrap(),
        "https://chatgpt.com/g/g-p-123/project?tab=sources&foo=bar"
    );
    assert!(project_sources::normalise_url("https://example.com/g/g-p-123/project").is_err());
    assert!(project_sources::normalise_url("https://chatgpt.com/c/abc").is_err());
    let before = vec![json!({"name":"a","index":0}), json!({"name":"b","index":1})];
    let mut after = before.clone();
    after.push(json!({"name":"a","index":2}));
    assert_eq!(
        project_sources::added_sources(&before, &after),
        vec![json!({"name":"a","index":2})]
    );
    let files = (0..11)
        .map(|i| prompts::PromptFile {
            path: format!("source-{i}.md").into(),
            display_path: format!("source-{i}.md"),
            content: String::new(),
            size_bytes: 10,
        })
        .collect::<Vec<_>>();
    let plan = project_sources::upload_plan(&files);
    assert_eq!(plan[9]["batch"], 1);
    assert_eq!(plan[10]["batch"], 2);
}

#[test]
fn nested_gitignore_negation_and_explicit_default_ignore_whitelists_are_preserved() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    for dir in ["src/nested", "build", "node_modules"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(
        root.join(".gitignore"),
        "*.tmp\n!keep.tmp\n/src/parent.rs\n",
    )
    .unwrap();
    fs::write(
        root.join("src/.gitignore"),
        "!parent.rs\nnested/*\n!nested/keep.rs\n",
    )
    .unwrap();
    for file in [
        "src/public.rs",
        "src/parent.rs",
        "src/drop.tmp",
        "src/keep.tmp",
        "src/nested/keep.rs",
        "src/nested/drop.rs",
        "build/default.rs",
        "node_modules/public.rs",
    ] {
        fs::write(root.join(file), "PUBLIC").unwrap();
    }
    let names = prompts::resolve_files(&["src/**/*".into()], root, 1024, true)
        .unwrap()
        .into_iter()
        .map(|f| f.display_path)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "src/keep.tmp",
            "src/nested/keep.rs",
            "src/parent.rs",
            "src/public.rs"
        ]
    );
    assert_eq!(
        prompts::resolve_files(&["build".into()], root, 1024, true).unwrap()[0].display_path,
        "build/default.rs"
    );
    fs::write(root.join("node_modules/.gitignore"), "*.private\n").unwrap();
    assert!(
        prompts::resolve_files(&["**/*.rs".into()], root, 1024, true)
            .unwrap()
            .iter()
            .any(|f| f.display_path == "node_modules/public.rs")
    );
    // A literal-only request overrides Git ignores as in the maintained readFiles fast path.
    assert_eq!(
        prompts::resolve_files(&["src/drop.tmp".into()], root, 1024, true)
            .unwrap()
            .len(),
        1
    );
    assert!(
        prompts::resolve_files(
            &["src/drop.tmp".into(), "!missing".into()],
            root,
            1024,
            true
        )
        .is_err()
    );
}

#[test]
fn astra_effort_range_and_explicit_browser_labels_follow_maintained_source() {
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            providers::build_request(
                "gpt-6-astra",
                &json!({"reasoningMode":"pro","reasoningEffort":effort}),
                "PUBLIC user",
                prompts::DEFAULT_SYSTEM
            )
            .unwrap()["reasoning"]["effort"],
            effort
        );
    }
    assert!(
        providers::build_request(
            "gpt-6-astra",
            &json!({"reasoningMode":"pro","reasoningEffort":"none"}),
            "PUBLIC user",
            prompts::DEFAULT_SYSTEM
        )
        .is_err()
    );
    let home = tempfile::tempdir().unwrap();
    let request = execution::resolve(
        &json!({"prompt":"PUBLIC browser preview","engine":"browser","browserModelLabel":"GPT-6"}),
        &json!({}),
        &Environment::new(),
        home.path(),
    )
    .unwrap();
    assert_eq!(request.browser["desiredModel"], "GPT-6");
    assert_eq!(request.browser["modelIsImplicitDefault"], false);
    assert!(
        execution::resolve(
            &json!({"prompt":"PUBLIC API request","engine":"api","browserThinkingTime":"pro"}),
            &json!({}),
            &Environment::new(),
            home.path()
        )
        .is_err()
    );
}
#[test]
fn legacy_gemini_options_are_saved_and_ignored_for_gpt6_on_both_engines() {
    let root = tempfile::tempdir().unwrap();
    let saved = json!({"youtube":"https://example.invalid/PUBLIC","editImage":"PUBLIC.png","aspectRatio":"16:9","geminiShowThoughts":true,"geminiAllowModelFallback":false});
    for engine in ["api", "browser"] {
        let mut input = saved.clone();
        input["prompt"] = json!("PUBLIC legacy Gemini options");
        input["engine"] = json!(engine);
        let request =
            execution::resolve(&input, &json!({}), &Environment::new(), root.path()).unwrap();
        for (key, value) in saved.as_object().unwrap() {
            assert_eq!(&request.options[key], value, "{engine} {key}");
        }
        assert_eq!(request.engine, engine);
    }
}
#[test]
fn zombie_options_are_saved_for_status_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let input = json!({"prompt":"PUBLIC zombie options","engine":"api","zombieTimeoutMs":300000.0,"zombieUseLastActivity":true});
    let request = execution::resolve(&input, &json!({}), &Environment::new(), root.path()).unwrap();
    assert_eq!(request.options["zombieTimeoutMs"], 300000.0);
    assert_eq!(request.options["zombieUseLastActivity"], true);
}
#[test]
fn partial_mode_is_saved_with_the_run_options() {
    let root = tempfile::tempdir().unwrap();
    let input = json!({"prompt":"PUBLIC partial mode","engine":"api","partialMode":"ok"});
    let request = execution::resolve(&input, &json!({}), &Environment::new(), root.path()).unwrap();
    assert_eq!(request.options["partialMode"], "ok");
}

fn names(files: &[prompts::PromptFile]) -> Vec<&str> {
    files
        .iter()
        .map(|file| file.display_path.as_str())
        .collect()
}

#[test]
fn repository_root_ignore_rules_apply_from_a_subdirectory() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    let cwd = root.join("sub");
    fs::create_dir_all(cwd.join("secret")).unwrap();
    fs::write(root.join(".gitignore"), "secret/\n").unwrap();
    fs::write(cwd.join("secret/a.rs"), "PUBLIC ignored").unwrap();
    fs::write(cwd.join("ok.rs"), "PUBLIC kept").unwrap();
    let select = || prompts::resolve_files(&["**/*.rs".into()], &cwd, 1024, true).unwrap();
    // Outside a repository the working directory stays the boundary.
    assert_eq!(names(&select()), ["ok.rs", "secret/a.rs"]);
    // A .git directory or a worktree's .git file marks the repository root.
    fs::create_dir(root.join(".git")).unwrap();
    assert_eq!(names(&select()), ["ok.rs"]);
    fs::remove_dir(root.join(".git")).unwrap();
    fs::write(root.join(".git"), "gitdir: PUBLIC elsewhere").unwrap();
    assert_eq!(names(&select()), ["ok.rs"]);
    // A directory input is pruned by the same rule.
    assert!(
        prompts::resolve_files(&["secret".into(), "ok.rs".into()], &cwd, 1024, true)
            .unwrap()
            .iter()
            .all(|file| file.display_path == "ok.rs")
    );
}

#[test]
fn rule_files_that_cannot_be_applied_fail_the_selection() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "PUBLIC").unwrap();
    fs::write(root.join("src/.gitignore"), "[z-a]\n").unwrap();
    let error = prompts::resolve_files(&["src/**/*.rs".into()], root, 1024, true).unwrap_err();
    assert!(format!("{error:#}").contains(".gitignore"), "{error:#}");
    fs::remove_file(root.join("src/.gitignore")).unwrap();
    fs::create_dir(root.join("src/.gitignore")).unwrap();
    let error = prompts::resolve_files(&["src/**/*.rs".into()], root, 1024, true).unwrap_err();
    assert!(
        format!("{error:#}").contains("not a regular file"),
        "{error:#}"
    );
    #[cfg(unix)]
    {
        fs::remove_dir(root.join("src/.gitignore")).unwrap();
        fs::write(root.join("rules"), "*.rs\n").unwrap();
        std::os::unix::fs::symlink(root.join("rules"), root.join("src/.gitignore")).unwrap();
        let error = prompts::resolve_files(&["src/**/*.rs".into()], root, 1024, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
    }
}

#[test]
fn files_that_are_not_utf8_are_refused_and_all_listed() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::write(root.join("a.bin.txt"), b"PUBLIC \xff").unwrap();
    fs::write(root.join("b.txt"), b"\xfe\xff\x00P").unwrap();
    fs::write(root.join("ok.txt"), "PUBLIC café").unwrap();
    let error = prompts::resolve_files(&["*.txt".into()], root, 1024, true).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("a.bin.txt") && message.contains("b.txt"),
        "{message}"
    );
    assert!(!message.contains("ok.txt"), "{message}");
    // Listing files without reading them, as browser uploads do, is unchanged: browser uploads
    // and ZIP bundles send the original bytes (pinned in fixtures/browser-prompt-reference.json).
    assert_eq!(
        prompts::resolve_files(&["*.txt".into()], root, 1024, false)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn credential_files_are_skipped_in_expansion_and_refused_by_name() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    for name in [
        ".env",
        ".env.local",
        ".env.example",
        "ID_RSA",
        "id_rsa.pub",
        "id_ed25519_sk",
        "cert.PEM",
        "server.ppk",
        ".npmrc",
        "ok.rs",
    ] {
        fs::write(root.join(name), "PUBLIC").unwrap();
    }
    // A dot pattern opts dotfiles into expansion; credential names stay out.
    let files = prompts::resolve_files(&["*".into(), ".*".into()], root, 1024, true).unwrap();
    assert_eq!(names(&files), [".env.example", "id_rsa.pub", "ok.rs"]);
    for literal in [".env", "ID_RSA", "cert.PEM", "server.ppk", "id_ed25519_sk"] {
        let error = prompts::resolve_files(&[literal.into(), "ok.rs".into()], root, 1024, true)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("Refusing"),
            "{literal} {error:#}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join(".env"), root.join("public.txt")).unwrap();
        let error = prompts::resolve_files(&["public.txt".into()], root, 1024, true).unwrap_err();
        assert!(format!("{error:#}").contains("public.txt"), "{error:#}");
    }
}

#[test]
fn ignore_rules_come_from_the_repository_holding_each_selected_path() {
    let repository = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let root = repository.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("sub/secret")).unwrap();
    fs::write(root.join(".gitignore"), "secret/\n").unwrap();
    fs::write(root.join("sub/secret/a.rs"), "PUBLIC ignored").unwrap();
    fs::write(root.join("sub/ok.rs"), "PUBLIC kept").unwrap();
    let sub = root.join("sub");
    // Selected by absolute path from an unrelated working directory.
    for input in [
        root.to_string_lossy().into_owned(),
        sub.to_string_lossy().into_owned(),
        sub.join("**/*.rs").to_string_lossy().replace('\\', "/"),
    ] {
        let files =
            prompts::resolve_files(std::slice::from_ref(&input), elsewhere.path(), 1024, true)
                .unwrap();
        // Temporary folder names can start with a dot, which opts dotfiles in; only the ignore
        // rule is under test here.
        let selected: Vec<_> = files
            .iter()
            .filter(|file| file.path.extension().is_some_and(|e| e == "rs"))
            .collect();
        assert!(
            selected.len() == 1 && selected[0].path.ends_with("sub/ok.rs"),
            "{input}: {:?}",
            names(&files)
        );
    }
}

#[cfg(unix)]
#[test]
fn a_broken_git_link_does_not_hide_the_enclosing_repository_rules() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("sub/secret")).unwrap();
    fs::write(root.join(".gitignore"), "secret/\n").unwrap();
    std::os::unix::fs::symlink(root.join("PUBLIC-missing"), root.join("sub/.git")).unwrap();
    fs::write(root.join("sub/secret/a.rs"), "PUBLIC ignored").unwrap();
    fs::write(root.join("sub/ok.rs"), "PUBLIC kept").unwrap();
    let files = prompts::resolve_files(&["**/*.rs".into()], &root.join("sub"), 1024, true).unwrap();
    assert_eq!(names(&files), ["ok.rs"]);
}

#[test]
fn a_text_bundle_refuses_files_that_are_not_utf8_and_zip_keeps_their_bytes() {
    use alcuin::browser_prompt::{BrowserPrompt, discover};
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::write(root.join("public.txt"), b"PUBLIC\xff").unwrap();
    fs::write(root.join("ok.txt"), "PUBLIC").unwrap();
    let files = discover(&["public.txt".into(), "ok.txt".into()], root, None).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let assemble = |format: &str| {
        BrowserPrompt::assemble(
            &files,
            "PUBLIC prompt",
            None,
            &json!({"browserBundleFiles": true, "browserBundleFormat": format}),
            parent.path(),
        )
    };
    for format in ["text", "auto"] {
        let error = assemble(format).err().expect(format);
        assert!(
            format!("{error:#}").contains("public.txt") && !format!("{error:#}").contains("ok.txt"),
            "{format}: {error:#}"
        );
    }
    assert!(assemble("zip").is_ok());
}
