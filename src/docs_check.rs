//! Markdown flag validation against the captured CLI surface, including command scope.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
const DEFAULT_PATHS: [&str; 6] = [
    "README.md",
    "docs/index.md",
    "docs/agents.md",
    "docs/sessions.md",
    "docs/spec.md",
    "docs/cli-reference.md",
];
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub file: String,
    pub flag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocsCheck {
    pub checked_files: Vec<String>,
    pub checked_flags: Vec<String>,
    pub issues: Vec<Issue>,
}
struct Reference {
    flag: String,
    section: Option<String>,
    command: Option<String>,
}
fn long_flag(line: &str, start: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    if bytes.get(start..start + 2) != Some(b"--") || !bytes.get(start + 2)?.is_ascii_lowercase() {
        return None;
    }
    let mut end = start + 3;
    while bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        end += 1;
    }
    Some((line[start..end].into(), end))
}
fn slash_flags(line: &str) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    let mut start = 0;
    while start < line.len() {
        let Some((base, mut end)) = long_flag(line, start) else {
            start += 1;
            continue;
        };
        let prefix = &base[..base.rfind('-').unwrap() + 1];
        let mut additions = vec![];
        while line.as_bytes().get(end) == Some(&b'/') {
            let next = end + 1;
            if let Some((flag, tail)) = long_flag(line, next) {
                additions.push(flag);
                end = tail;
                continue;
            }
            let bytes = line.as_bytes();
            if bytes.get(next) == Some(&b'-')
                && bytes.get(next + 1).is_some_and(u8::is_ascii_lowercase)
            {
                let mut tail = next + 2;
                while bytes
                    .get(tail)
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
                {
                    tail += 1;
                }
                additions.push(format!("{prefix}{}", &line[next + 1..tail]));
                end = tail;
            } else {
                break;
            }
        }
        if !additions.is_empty() {
            flags.insert(base);
            flags.extend(additions);
        }
        start = end;
    }
    flags
}
fn command_path(line: &str, name: &str) -> Option<String> {
    let trimmed = line.trim();
    let trimmed = trimmed
        .strip_prefix(['$', '>'])
        .filter(|s| s.starts_with(char::is_whitespace))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let tokens: Vec<_> = trimmed.split_whitespace().collect();
    if !(trimmed.starts_with(&format!("{name} "))
        || trimmed.starts_with("oracle ")
        || matches!(
            tokens.first(),
            Some(&"npx" | &"npm" | &"pnpm" | &"corepack")
        ))
    {
        return None;
    }
    let index = tokens
        .iter()
        .position(|token| *token == name || matches!(*token, "oracle" | "@steipete/oracle"))?;
    let mut path = vec![name];
    for token in &tokens[index + 1..] {
        let bytes = token.as_bytes();
        if !bytes.first().is_some_and(u8::is_ascii_lowercase)
            || !bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            break;
        }
        path.push(token);
    }
    Some(path.join(" "))
}
fn references(markdown: &str, name: &str) -> Vec<Reference> {
    let mut references = vec![];
    let mut section = None;
    for line in markdown.lines() {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        if hashes >= 2
            && line[hashes..].starts_with(char::is_whitespace)
            && !line[hashes..].trim().is_empty()
        {
            section = Some(line[hashes..].trim().to_owned());
        }
        let command = command_path(line, name);
        let trimmed = line.trim().trim_start_matches(['$', '>']).trim_start();
        if matches!(
            trimmed.split_whitespace().next(),
            Some("corepack" | "pnpm" | "npm" | "cargo")
        ) && command.is_none()
        {
            continue;
        }
        let mut flags = slash_flags(line);
        for (start, c) in line.char_indices() {
            if c != '-' {
                continue;
            }
            let Some((flag, end)) = long_flag(line, start) else {
                continue;
            };
            let before = line[..start].chars().next_back();
            let after = line[end..].chars().next();
            let left = before.is_none_or(|c| {
                c.is_whitespace() || matches!(c, '`' | '(' | '[' | '{' | '|' | ',')
            });
            let right = after.is_none_or(|c| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '`' | ')' | '[' | ']' | '.' | ',' | ';' | ':' | '|' | '=' | '<' | '>' | '}'
                    )
            });
            if left && right && !flag.ends_with('-') {
                flags.insert(flag);
            }
        }
        for flag in flags {
            references.push(Reference {
                flag,
                section: section.clone(),
                command: command.clone(),
            });
        }
    }
    references.sort_by(|a, b| {
        locale_compare(&a.flag, &b.flag).then_with(|| {
            locale_compare(
                a.section.as_deref().unwrap_or(""),
                b.section.as_deref().unwrap_or(""),
            )
        })
    });
    references
}
/// ICU root collation order for printable ASCII punctuation and symbols, which all sort before
/// digits and letters.
const PUNCTUATION_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
/// JavaScript String.prototype.localeCompare under the root locale for ASCII text: whitespace,
/// then punctuation in ICU order, digits and letters ignoring case; lowercase breaks ties first.
/// shortcut: non-ASCII characters fall back to code point order after every ASCII character.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    fn primary(c: char) -> (u8, u32) {
        if c.is_ascii_whitespace() || c.is_ascii_control() {
            (0, c as u32)
        } else if let Some(index) = PUNCTUATION_ORDER.find(c) {
            (1, index as u32)
        } else if c.is_ascii_digit() {
            (2, c as u32)
        } else if c.is_ascii_alphabetic() {
            (3, c.to_ascii_lowercase() as u32)
        } else {
            (4, c as u32)
        }
    }
    left.chars()
        .map(primary)
        .cmp(right.chars().map(primary))
        .then_with(|| {
            left.chars()
                .map(|c| c.is_ascii_uppercase())
                .cmp(right.chars().map(|c| c.is_ascii_uppercase()))
        })
}
pub fn extract_flags(markdown: &str) -> Vec<String> {
    references(markdown, "oracle")
        .into_iter()
        .map(|r| r.flag)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn option_flags(options: &[Value]) -> BTreeSet<String> {
    let mut flags = BTreeSet::from(["--help".into(), "--version".into()]);
    for option in options {
        let text = option["flags"].as_str().unwrap_or("");
        for (i, _) in text.char_indices() {
            if let Some((flag, _)) = long_flag(text, i) {
                flags.insert(flag);
            }
        }
    }
    flags
}
fn surface() -> (
    BTreeSet<String>,
    BTreeSet<String>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let root: Vec<Value> =
        serde_json::from_str(include_str!("cli_spec.json")).expect("Captured CLI options");
    let commands: Vec<Value> =
        serde_json::from_str(include_str!("cli_commands.json")).expect("Captured CLI commands");
    let root = option_flags(&root);
    let mut all = root.clone();
    let mut scoped = BTreeMap::from([("industrious".into(), root.clone())]);
    fn add(
        spec: &Value,
        parent: &str,
        all: &mut BTreeSet<String>,
        scoped: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        let command = spec["command"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        let path = format!("{parent} {command}");
        let flags = option_flags(spec["options"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        all.extend(flags.iter().cloned());
        scoped.insert(path.clone(), flags);
        for child in spec["children"].as_array().into_iter().flatten() {
            add(child, &path, all, scoped);
        }
    }
    for spec in commands {
        add(&spec, "industrious", &mut all, &mut scoped);
    }
    (root, all, scoped)
}
pub fn check(cwd: &Path, paths: &[String]) -> Result<DocsCheck> {
    let candidates = if paths.is_empty() {
        DEFAULT_PATHS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    } else {
        paths.to_vec()
    };
    let mut checked_files = vec![];
    for candidate in candidates {
        match fs::metadata(cwd.join(&candidate)) {
            Ok(metadata) if metadata.is_file() => checked_files.push(candidate),
            Err(_) if !paths.is_empty() => bail!("Docs check path not found: {candidate}"),
            _ => {}
        }
    }
    if checked_files.is_empty() {
        bail!("No docs found to check. Run from the repo root or pass --docs-path <file>.");
    }
    let (root, all, scoped) = surface();
    let mut checked_flags = BTreeSet::new();
    let mut issues = vec![];
    for file in &checked_files {
        let body = fs::read_to_string(cwd.join(file))?;
        for reference in references(&body, "industrious") {
            checked_flags.insert(reference.flag.clone());
            let available = if let Some(command) = &reference.command {
                scoped.get(command).unwrap_or(&all)
            } else if reference.section.as_deref() == Some("Core consult flags") {
                &root
            } else {
                &all
            };
            if !available.contains(&reference.flag) {
                issues.push(Issue {
                    file: file.clone(),
                    flag: reference.flag,
                    section: reference.section,
                    command: reference.command,
                });
            }
        }
    }
    issues.sort_by(|a, b| {
        locale_compare(&a.file, &b.file).then_with(|| locale_compare(&a.flag, &b.flag))
    });
    Ok(DocsCheck {
        checked_files,
        checked_flags: checked_flags.into_iter().collect(),
        issues,
    })
}
pub fn format_result(result: &DocsCheck) -> String {
    if result.issues.is_empty() {
        return format!(
            "Docs/help check: ok ({} flags, {} files)",
            result.checked_flags.len(),
            result.checked_files.len()
        );
    }
    let mut lines = vec!["Docs/help drift:".to_owned()];
    for issue in &result.issues {
        let scopes = [issue.section.as_deref(), issue.command.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let scope = if scopes.is_empty() {
            String::new()
        } else {
            format!(" ({})", scopes.join(", "))
        };
        lines.push(format!(
            "- {}{scope} mentions {}, but CLI help does not expose {}",
            issue.file, issue.flag, issue.flag
        ));
    }
    lines.join("\n")
}
#[cfg(test)]
mod tests {
    #[test]
    fn locale_compare_matches_root_collation_for_ascii_paths() {
        let mut names = vec![
            "README.md",
            "docs/b.md",
            "Docs/a.md",
            "docs/a.md",
            "--a_b",
            "--a-b",
            "--ab",
            "--a9",
        ];
        names.sort_by(|a, b| super::locale_compare(a, b));
        assert_eq!(
            names,
            [
                "--a_b",
                "--a-b",
                "--a9",
                "--ab",
                "docs/a.md",
                "Docs/a.md",
                "docs/b.md",
                "README.md"
            ]
        );
    }
}
