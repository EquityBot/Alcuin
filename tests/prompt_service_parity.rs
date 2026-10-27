//! File selection and Project Sources URL parity with the reference oracle/files and
//! projectSources/url modules.
use alcuin::{project_sources, prompts};

fn refusal(patterns: &[String], root: &std::path::Path, limit: u64) -> String {
    match prompts::resolve_files(patterns, root, limit, true) {
        Ok(_) => panic!("{patterns:?} was accepted"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn nesting_counts_parentheses_and_ignores_closers_inside_brackets() {
    let root = tempfile::tempdir().unwrap();
    let limit = "Glob pattern nests braces or parentheses 33 levels deep; the limit is 32.";
    for pattern in [
        format!("{}public{}", "(".repeat(33), ")".repeat(33)),
        "{[}]".repeat(33),
        format!("{}public{}", "{(".repeat(16), "x{"),
    ] {
        assert_eq!(
            refusal(std::slice::from_ref(&pattern), root.path(), 1024),
            limit,
            "{pattern}"
        );
    }
    let shallow = format!("{}public{}", "(".repeat(32), ")".repeat(32));
    assert_ne!(refusal(&[shallow], root.path(), 1024), limit);
}

#[test]
fn every_oversized_file_is_listed_and_a_missing_file_takes_precedence() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", "small.txt"] {
        let body = if name == "small.txt" { "1" } else { "PUBLIC" };
        std::fs::write(root.path().join(name), body).unwrap();
    }
    let paths = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        refusal(&paths(&["a.txt", "small.txt", "b.txt"]), root.path(), 4),
        "The following files exceed the 4 B limit:\n- a.txt (6 B)\n- b.txt (6 B)"
    );
    assert_eq!(
        refusal(&paths(&["a.txt", "missing.txt"]), root.path(), 4),
        "Missing file or directory: missing.txt"
    );
}

#[test]
fn unparseable_project_urls_carry_the_parser_reason() {
    assert_eq!(
        project_sources::normalise_url("PUBLIC not a url")
            .unwrap_err()
            .to_string(),
        "Invalid ChatGPT project URL: PUBLIC not a url (Invalid URL)"
    );
}
