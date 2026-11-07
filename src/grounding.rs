//! `alcuin ground`: checks a saved answer's `path:line` references and quoted spans against the
//! files its session sent, so a reader can see which evidence the model could not have read.
//! Proposed code in fenced blocks is not checked. Ported from Ryan Duguid's TypeScript fork.
use crate::sessions::SessionStore;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::{fs, path::PathBuf, sync::LazyLock};

/// Quotes shorter than this, after whitespace is collapsed, are too common to be evidence.
const MIN_QUOTE_LENGTH: usize = 8;
/// What the session's prompt is called among the sent texts. It has no extension, so no
/// reference can name it, but the answer may quote it.
const PROMPT: &str = "prompt";

// A path with an extension, then :line or :start-end. `references` checks that the path does
// not continue a longer token, such as a URL host before its port.
const REFERENCE: &str = r"((?:[A-Za-z0-9_.-]+[\\/])*[A-Za-z0-9_-][A-Za-z0-9_.-]*\.[A-Za-z][A-Za-z0-9]{0,7}):([0-9]+)(?:-([0-9]+))?";
static REFERENCE_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{REFERENCE}")).expect("reference pattern"));
static REFERENCE_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{REFERENCE}$")).expect("reference pattern"));
static FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})").expect("fence pattern"));
static DIFF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^@@ -[0-9]+(?:,[0-9]+)? \+[0-9]+(?:,[0-9]+)? @@").expect("diff pattern")
});
static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -[0-9]+(?:,[0-9]+)? \+([0-9]+)(?:,([0-9]+))? @@").expect("hunk pattern")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}]$").expect("word pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Grounded,
    OutOfRange,
    NotSent,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceCheck {
    pub answer_line: usize,
    pub reference: String,
    pub status: Status,
    /// Line count of the longest sent file the reference matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_lines: Option<usize>,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteCheck {
    pub answer_line: usize,
    pub quote: String,
    pub found: bool,
}
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub references: Vec<ReferenceCheck>,
    pub quotes: Vec<QuoteCheck>,
    /// Sent files modified after the session was created, checked against their current text.
    pub changed_files: Vec<String>,
    /// Sent files that are not UTF-8 text, so nothing was checked against them.
    pub skipped_files: Vec<String>,
}
impl Report {
    /// True when a reference is out of range or names a file not sent, or a quote is not found.
    pub fn flagged(&self) -> bool {
        self.references
            .iter()
            .any(|reference| reference.status != Status::Grounded)
            || self.quotes.iter().any(|quote| !quote.found)
    }
}
pub struct SentFile {
    pub path: String,
    pub text: String,
}

fn normalise(text: &str) -> String {
    text.replace(['‘', '’'], "'")
        .replace(['“', '”'], "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.split('\n').count() - usize::from(text.ends_with('\n'))
}
fn number(digits: &str) -> u64 {
    digits.parse().unwrap_or(u64::MAX)
}
fn path_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '/' | '\\' | '.' | '-')
}
fn word(character: char) -> bool {
    WORD.is_match(character.encode_utf8(&mut [0; 4]))
}

/// Each `path:line` reference that starts a token: one not preceded by a path character.
fn references(line: &str) -> Vec<regex::Captures<'_>> {
    let mut found = vec![];
    let mut next = 0;
    let mut previous = None;
    for (index, character) in line.char_indices() {
        let starts_token = !previous.is_some_and(path_char);
        previous = Some(character);
        if index < next || !starts_token {
            continue;
        }
        if let Some(captures) = REFERENCE_AT.captures(&line[index..]) {
            next = index + captures[0].len();
            found.push(captures);
        }
    }
    found
}

struct QuoteStyle {
    open: char,
    close: [char; 2],
    /// The opening mark must not follow a letter or digit, so the apostrophes in "Ryan's" or
    /// "don't" do not open a quote.
    open_boundary: bool,
    /// The closing mark must not precede a letter or digit.
    close_boundary: bool,
}
const QUOTE_STYLES: [QuoteStyle; 4] = [
    QuoteStyle {
        open: '\'',
        close: ['\'', '’'],
        open_boundary: true,
        close_boundary: true,
    },
    QuoteStyle {
        open: '"',
        close: ['"', '”'],
        open_boundary: true,
        close_boundary: true,
    },
    QuoteStyle {
        open: '‘',
        close: ['’', '\''],
        open_boundary: false,
        close_boundary: true,
    },
    QuoteStyle {
        open: '“',
        close: ['”', '"'],
        open_boundary: false,
        close_boundary: false,
    },
];
/// The index of the mark that closes a quote opened at `open`, as the fork's
/// `\S(?:[^\n]*?\S)?` finds it: content of 2 or more characters, shortest first, then 1.
fn closing(chars: &[char], open: usize, style: &QuoteStyle) -> Option<usize> {
    if chars[open] != style.open || style.open_boundary && open > 0 && word(chars[open - 1]) {
        return None;
    }
    let first = open + 1;
    if chars.get(first).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let closes = |at: usize| {
        chars.get(at).is_some_and(|c| style.close.contains(c))
            && !(style.close_boundary && chars.get(at + 1).is_some_and(|c| word(*c)))
    };
    (first + 2..chars.len())
        .find(|&at| !chars[at - 1].is_whitespace() && closes(at))
        .or_else(|| closes(first + 1).then_some(first + 1))
}
/// The quoted spans in one line, by style and then position.
fn quotes(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let span = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    let mut found = vec![];
    for style in &QUOTE_STYLES {
        let mut at = 0;
        while at < chars.len() {
            match closing(&chars, at, style) {
                Some(close) => {
                    found.push(span(at + 1, close));
                    at = close + 1;
                }
                None => at += 1,
            }
        }
    }
    let mut at = 0;
    while at < chars.len() {
        let content = if chars[at] == '`' {
            chars[at + 1..].iter().position(|&c| c == '`')
        } else {
            None
        };
        match content.filter(|&length| length > 0) {
            Some(length) => {
                found.push(span(at + 1, at + 1 + length));
                at += length + 2;
            }
            None => at += 1,
        }
    }
    found
}

struct DiffSection {
    /// Lower-case path from the `+++` header, without its `b/` prefix.
    path: String,
    /// New-side line ranges the hunks show, inclusive.
    ranges: Vec<(u64, u64)>,
}
/// A unified diff shows each side of a file with a marker column, which would split any quote
/// that wraps a line. Read the new and old sides back as text, and record which new-side lines
/// each file's hunks show, so `path:line` can be checked against them.
fn read_diff(text: &str) -> Option<(Vec<DiffSection>, [String; 2])> {
    if !DIFF.is_match(text) {
        return None;
    }
    let mut sections: Vec<DiffSection> = vec![];
    let (mut new_side, mut old_side) = (vec![], vec![]);
    for line in text.split('\n').map(|line| line.trim_end_matches('\r')) {
        if let Some(header) = line.strip_prefix("+++ ") {
            let target = header.split('\t').next().unwrap_or_default();
            sections.push(DiffSection {
                path: target.strip_prefix("b/").unwrap_or(target).to_lowercase(),
                ranges: vec![],
            });
        } else if line.starts_with("--- ") {
            continue;
        } else if let Some(hunk) = HUNK.captures(line) {
            let start = number(&hunk[1]);
            let size = hunk.get(2).map_or(1, |count| number(count.as_str()));
            if size > 0
                && let Some(section) = sections.last_mut()
            {
                section.ranges.push((start, start.saturating_add(size - 1)));
            }
        } else if let Some(added) = line.strip_prefix('+') {
            new_side.push(added);
        } else if let Some(removed) = line.strip_prefix('-') {
            old_side.push(removed);
        } else if let Some(context) = line.strip_prefix(' ') {
            new_side.push(context);
            old_side.push(context);
        }
    }
    Some((
        sections,
        [
            normalise(&new_side.join("\n")),
            normalise(&old_side.join("\n")),
        ],
    ))
}

/// Checks the answer's references and quotes against the sent files. Paths compare
/// case-insensitively, and a reference names a file when the file's path ends with it.
pub fn check(answer: &str, files: &[SentFile]) -> Report {
    struct Sent {
        key: String,
        lines: usize,
    }
    let sent: Vec<Sent> = files
        .iter()
        .map(|file| Sent {
            key: file.path.replace('\\', "/").to_lowercase(),
            lines: line_count(&file.text),
        })
        .collect();
    let mut sections = vec![];
    let mut texts: Vec<String> = files.iter().map(|file| normalise(&file.text)).collect();
    for (diff_sections, sides) in files.iter().filter_map(|file| read_diff(&file.text)) {
        sections.extend(diff_sections);
        texts.extend(sides);
    }
    let mut report = Report::default();
    let mut fence: Option<String> = None;
    for (index, line) in answer.split('\n').enumerate() {
        let answer_line = index + 1;
        if let Some(marker) = FENCE.captures(line).map(|c| c[1].to_owned())
            && fence.as_deref().is_none_or(|open| marker.starts_with(open))
        {
            fence = if fence.is_some() { None } else { Some(marker) };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        for captures in references(line) {
            let want = captures[1].replace('\\', "/");
            let want = want.strip_prefix("./").unwrap_or(&want).to_lowercase();
            let suffix = format!("/{want}");
            let start = number(&captures[2]);
            let last = captures
                .get(3)
                .map_or(start, |end| start.max(number(end.as_str())));
            let reference = captures[0].to_owned();
            let file_lines = sent
                .iter()
                .filter(|file| file.key == want || file.key.ends_with(&suffix))
                .map(|file| file.lines)
                .max();
            let status = match file_lines {
                Some(lines) if start >= 1 && last <= lines as u64 => Status::Grounded,
                Some(_) => Status::OutOfRange,
                // A diff's paths are repository-relative, and the answer may add or drop a prefix.
                None if sections.iter().any(|section| {
                    (section.path == want
                        || section.path.ends_with(&suffix)
                        || want.ends_with(&format!("/{}", section.path)))
                        && section
                            .ranges
                            .iter()
                            .any(|&(first, end)| start >= first && last <= end)
                }) =>
                {
                    Status::Grounded
                }
                None => Status::NotSent,
            };
            report.references.push(ReferenceCheck {
                answer_line,
                reference,
                status,
                file_lines,
            });
        }
        for quote in quotes(line).iter().map(|quote| normalise(quote)) {
            if quote.encode_utf16().count() < MIN_QUOTE_LENGTH || REFERENCE_ONLY.is_match(&quote) {
                continue;
            }
            let found = texts.iter().any(|text| text.contains(&quote));
            report.quotes.push(QuoteCheck {
                answer_line,
                quote,
                found,
            });
        }
    }
    report
}

/// Checks a session's saved answer against its prompt and the files it sent. Sessions keep the
/// file selection, not the bytes sent, so the selection is resolved again from the session's
/// working directory and each file read as it is now; a file modified after the session was
/// created is listed in `changed_files`.
pub fn session_report(store: &SessionStore, id: &str) -> Result<Report> {
    let meta = store.require(id)?;
    anyhow::ensure!(
        meta["browser"]["capturedAnswer"].is_object() || meta["capturedAnswer"].is_object(),
        "Session \"{id}\" has no saved answer to check."
    );
    let answer = String::from_utf8(crate::browser_runner::captured(store, id)?)
        .context("The saved answer is not UTF-8")?;
    let created = meta["createdAt"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .with_context(|| format!("Session \"{id}\" has no valid creation time."))?;
    let cwd = meta["cwd"]
        .as_str()
        .map(PathBuf::from)
        .with_context(|| format!("Session \"{id}\" has no working directory."))?;
    let options = &meta["options"];
    let inputs: Vec<String> = options["file"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut files = vec![SentFile {
        path: PROMPT.into(),
        text: options["prompt"].as_str().unwrap_or_default().into(),
    }];
    let (mut changed, mut skipped) = (vec![], vec![]);
    for file in crate::prompts::resolve_files(&inputs, &cwd, 0, false)? {
        let modified = fs::metadata(&file.path)
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("Cannot read {}", file.display_path))?;
        // createdAt has millisecond precision.
        if chrono::DateTime::<chrono::Utc>::from(modified).timestamp_millis()
            > created.timestamp_millis()
        {
            changed.push(file.display_path.clone());
        }
        match crate::prompts::read_text(&file.path)
            .with_context(|| format!("Cannot read {}", file.display_path))?
        {
            Some(text) => files.push(SentFile {
                path: file.path.to_string_lossy().into_owned(),
                text,
            }),
            None => skipped.push(file.display_path),
        }
    }
    let mut report = check(&answer, &files);
    report.changed_files = changed;
    report.skipped_files = skipped;
    Ok(report)
}

pub fn format_report(report: &Report) -> String {
    let count = |status: Status| {
        report
            .references
            .iter()
            .filter(|reference| reference.status == status)
            .count()
    };
    let found = report.quotes.iter().filter(|quote| quote.found).count();
    let plural = |n: usize, noun: &str| format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
    let mut lines = vec![format!(
        "Grounding: {} ({} grounded, {} out of range, {} not in the files sent); {} ({found} found, {} not found).",
        plural(report.references.len(), "reference"),
        count(Status::Grounded),
        count(Status::OutOfRange),
        count(Status::NotSent),
        plural(report.quotes.len(), "quote"),
        report.quotes.len() - found,
    )];
    let mut section = |title: &str, items: Vec<String>| {
        if !items.is_empty() {
            lines.push(title.to_owned());
            lines.extend(items.into_iter().map(|item| format!("  {item}")));
        }
    };
    let references = |status: Status| {
        report
            .references
            .iter()
            .filter(move |reference| reference.status == status)
    };
    section(
        "Out of range:",
        references(Status::OutOfRange)
            .map(|reference| {
                format!(
                    "answer line {}: {} (file has {} lines)",
                    reference.answer_line,
                    reference.reference,
                    reference.file_lines.unwrap_or_default()
                )
            })
            .collect(),
    );
    section(
        "Quotes not found in the files sent:",
        report
            .quotes
            .iter()
            .filter(|quote| !quote.found)
            .map(|quote| format!("answer line {}: '{}'", quote.answer_line, quote.quote))
            .collect(),
    );
    section(
        "Not in the files sent:",
        references(Status::NotSent)
            .map(|reference| {
                format!(
                    "answer line {}: {}",
                    reference.answer_line, reference.reference
                )
            })
            .collect(),
    );
    section(
        "Changed since the session was created (checked against their current text):",
        report.changed_files.clone(),
    );
    section("Not text (not checked):", report.skipped_files.clone());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine() -> SentFile {
        SentFile {
            path: "/work/packages/close/src/engine.rs".into(),
            text: "fn run() {\n    let status = \"BLOCKED\";\n    // A blank ReviewGroup is refused\n}\n"
                .into(),
        }
    }
    fn prompt() -> SentFile {
        SentFile {
            path: "/work/frozen/prompt.txt".into(),
            text: "Success: every table claim matches the code.\n".into(),
        }
    }
    fn statuses(report: &Report) -> Vec<(&str, Status)> {
        report
            .references
            .iter()
            .map(|reference| (reference.reference.as_str(), reference.status))
            .collect()
    }
    fn found(report: &Report) -> Vec<(&str, bool)> {
        report
            .quotes
            .iter()
            .map(|quote| (quote.quote.as_str(), quote.found))
            .collect()
    }

    #[test]
    fn grounds_a_reference_by_path_suffix_and_line_range() {
        let report = check(
            "See src/engine.rs:2 and packages/close/src/engine.rs:1-4.",
            &[engine()],
        );
        assert_eq!(
            statuses(&report),
            [
                ("src/engine.rs:2", Status::Grounded),
                ("packages/close/src/engine.rs:1-4", Status::Grounded)
            ]
        );
        assert!(!report.flagged());
    }
    #[test]
    fn flags_a_line_past_the_end_and_a_file_not_sent() {
        let report = check(
            "Bug at engine.rs:9. Compare docs/scope.md:134.",
            &[engine()],
        );
        assert_eq!(
            report.references,
            [
                ReferenceCheck {
                    answer_line: 1,
                    reference: "engine.rs:9".into(),
                    status: Status::OutOfRange,
                    file_lines: Some(4)
                },
                ReferenceCheck {
                    answer_line: 1,
                    reference: "docs/scope.md:134".into(),
                    status: Status::NotSent,
                    file_lines: None
                }
            ]
        );
        assert!(report.flagged());
        // A file that was not sent is flagged on its own.
        assert!(check("Compare docs/scope.md:134.", &[engine()]).flagged());
    }
    #[test]
    fn ignores_url_hosts_with_ports_and_line_0_is_out_of_range() {
        let report = check(
            "Open https://example.com:443/x then engine.rs:0.",
            &[engine()],
        );
        assert_eq!(statuses(&report), [("engine.rs:0", Status::OutOfRange)]);
        // A path that continues a longer token is not a reference, as with the fork's lookbehind.
        assert!(
            check("See .engine.rs:2 and x/.engine.rs:2", &[engine()])
                .references
                .is_empty()
        );
    }
    #[test]
    fn finds_quotes_across_wrapped_lines_and_curly_quotes_and_flags_invented_ones() {
        let report = check(
            &[
                "The comment says ‘A blank   ReviewGroup is refused’.",
                "The brief asks that 'every table claim matches the code'.",
                "It also claims \"every account is mapped twice\".",
            ]
            .join("\n"),
            &[engine(), prompt()],
        );
        assert_eq!(
            found(&report),
            [
                ("A blank ReviewGroup is refused", true),
                ("every table claim matches the code", true),
                ("every account is mapped twice", false)
            ]
        );
        assert!(report.flagged());
    }
    #[test]
    fn does_not_open_quotes_at_apostrophes_and_skips_short_quotes_and_bare_references() {
        let report = check(
            "Ryan's note doesn't say 'whilst'; see `engine.rs:2` and `let status`.",
            &[engine()],
        );
        assert_eq!(found(&report), [("let status", true)]);
        // 7 characters is under the minimum; 8 is checked.
        let report = check("`1234567` and `12345678`", &[engine()]);
        assert_eq!(found(&report), [("12345678", false)]);
    }
    #[test]
    fn skips_lines_inside_fenced_code_blocks() {
        let report = check(
            &[
                "```rust",
                "let fixed = 'proposed new code';",
                "engine.rs:99",
                "```",
                "after",
            ]
            .join("\n"),
            &[engine()],
        );
        assert_eq!(report, Report::default());
        assert!(!report.flagged());
        // A shorter or different marker does not close the fence.
        let report = check(
            "````\n```\n~~~\nengine.rs:99\n````\nengine.rs:99",
            &[engine()],
        );
        assert_eq!(report.references.len(), 1);
        assert_eq!(report.references[0].answer_line, 6);
    }
    #[test]
    fn reads_both_sides_of_a_unified_diff_and_checks_references_against_its_hunks() {
        let diff = SentFile {
            path: "/work/frozen/source-1-review.diff".into(),
            text: [
                "diff --git a/packages/close/README.md b/packages/close/README.md",
                "--- a/packages/close/README.md",
                "+++ b/packages/close/README.md",
                "@@ -38,3 +38,4 @@ heading",
                " every review; the others run only when supplied. A control that",
                "-raises nothing has passed.",
                "+ran and raised nothing has passed. One whose input was",
                "+not supplied did not run.",
                "",
            ]
            .join("\n"),
        };
        let report = check(
            &[
                "Old text: 'A control that raises nothing has passed'.",
                "New text: 'A control that ran and raised nothing has passed'.",
                "See repo/packages/close/README.md:39-41, README.md:38 and packages/close/README.md:90.",
            ]
            .join("\n"),
            &[diff],
        );
        assert_eq!(
            found(&report),
            [
                ("A control that raises nothing has passed", true),
                ("A control that ran and raised nothing has passed", true)
            ]
        );
        assert_eq!(
            statuses(&report),
            [
                ("repo/packages/close/README.md:39-41", Status::Grounded),
                ("README.md:38", Status::Grounded),
                ("packages/close/README.md:90", Status::NotSent)
            ]
        );
    }
    #[test]
    fn formats_counts_and_each_item_that_needs_a_readers_check() {
        let mut report = check(
            "engine.rs:2 engine.rs:7 docs/x.md:1 'every account is mapped twice'",
            &[engine()],
        );
        report.changed_files = vec!["src/engine.rs".into()];
        assert_eq!(
            format_report(&report),
            [
                "Grounding: 3 references (1 grounded, 1 out of range, 1 not in the files sent); 1 quote (0 found, 1 not found).",
                "Out of range:",
                "  answer line 1: engine.rs:7 (file has 4 lines)",
                "Quotes not found in the files sent:",
                "  answer line 1: 'every account is mapped twice'",
                "Not in the files sent:",
                "  answer line 1: docs/x.md:1",
                "Changed since the session was created (checked against their current text):",
                "  src/engine.rs",
            ]
            .join("\n")
        );
    }
}
