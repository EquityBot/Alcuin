use industrious::docs_check;
use serde_json::Value;
use std::{fs, process::Command};
#[test]
fn flags_are_complete_and_slash_shorthand_does_not_create_prefix_fragments() {
    assert_eq!(
        docs_check::extract_flags(
            "Use `--no-azure`, `--http-timeout <ms>`, --browser-inline-cookies[(-file)], --remote-host/--remote-token, --browser-auto-reattach-delay/-interval/-timeout, and --browser-auto-reattach-*"
        ),
        vec![
            "--browser-auto-reattach-delay",
            "--browser-auto-reattach-interval",
            "--browser-auto-reattach-timeout",
            "--browser-inline-cookies",
            "--http-timeout",
            "--no-azure",
            "--remote-host",
            "--remote-token"
        ]
    );
    assert_eq!(
        docs_check::extract_flags(
            "corepack pnpm@11.28.0 install --frozen-lockfile\nnpm install --ignore-scripts\npnpm run check --silent\ncargo install --locked --path .\nindustrious --stale-flag\nDocumented: `--known`."
        ),
        vec!["--known", "--stale-flag"]
    );
}
#[test]
fn inherited_command_aliases_and_core_sections_keep_their_own_flag_scope() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("flags.md"),"industrious status --json\noracle status --json\nnpx @steipete/oracle status --json\npnpm exec industrious status --json\ncorepack pnpm@11.28.0 exec industrious status --json\n## Core consult flags\n`--json`\n").unwrap();
    let result = docs_check::check(home.path(), &["flags.md".into()]).unwrap();
    assert_eq!(result.issues.len(), 6);
    assert_eq!(
        result
            .issues
            .iter()
            .filter(|i| i.command.as_deref() == Some("industrious status"))
            .count(),
        5
    );
    assert_eq!(
        result
            .issues
            .iter()
            .filter(|i| i.section.as_deref() == Some("Core consult flags"))
            .count(),
        1
    );
    assert_eq!(result.checked_flags, vec!["--json"]);
    assert!(docs_check::format_result(&result).contains("Docs/help drift:"));
}
#[test]
fn explicit_missing_paths_refuse_and_default_paths_do_not_visit_unrelated_documents() {
    let home = tempfile::tempdir().unwrap();
    assert!(docs_check::check(home.path(), &[]).is_err());
    assert_eq!(
        docs_check::check(home.path(), &["missing.md".into()])
            .unwrap_err()
            .to_string(),
        "Docs check path not found: missing.md"
    );
    fs::write(
        home.path().join("README.md"),
        "Use `--engine` and `--file`.\n",
    )
    .unwrap();
    fs::write(home.path().join("unrelated.md"), "`--stale-flag`\n").unwrap();
    let result = docs_check::check(home.path(), &[]).unwrap();
    assert!(result.issues.is_empty());
    assert_eq!(result.checked_files, vec!["README.md"]);
    assert_eq!(
        docs_check::format_result(&result),
        "Docs/help check: ok (2 flags, 1 files)"
    );
}
#[test]
fn actual_cli_reports_drift_as_exit_one_and_json_stays_parseable() {
    let home = tempfile::tempdir().unwrap();
    let run = |file: &str| {
        Command::new(env!("CARGO_BIN_EXE_industrious"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .current_dir(home.path())
            .args(["docs", "check", "--docs-path", file, "--json"])
            .output()
            .unwrap()
    };
    fs::write(home.path().join("good.md"), "industrious doctor --json\n").unwrap();
    let output = run("good.md");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["issues"], serde_json::json!([]));
    fs::write(home.path().join("bad.md"), "industrious status --json\n").unwrap();
    let output = run("bad.md");
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["issues"][0]["command"], "industrious status");
    let output = run("missing.md");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Docs check path not found: missing.md")
    );
}
#[test]
fn issues_sort_files_and_flags_like_locale_compare() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("docs")).unwrap();
    for name in ["README.md", "docs/a.md"] {
        fs::write(
            home.path().join(name),
            "industrious --public-zeta-stale --public-alpha-stale\n",
        )
        .unwrap();
    }
    let result = docs_check::check(home.path(), &["README.md".into(), "docs/a.md".into()]).unwrap();
    let order: Vec<_> = result
        .issues
        .iter()
        .map(|issue| format!("{} {}", issue.file, issue.flag))
        .collect();
    assert_eq!(
        order,
        [
            "docs/a.md --public-alpha-stale",
            "docs/a.md --public-zeta-stale",
            "README.md --public-alpha-stale",
            "README.md --public-zeta-stale"
        ]
    );
}
