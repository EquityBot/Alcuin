//! `alcuin ground`: checks a saved answer's `path:line` references and quoted spans against the
//! files its session sent, so a reader can see which evidence the model could not have read.
//! Proposed code in fenced blocks is not checked. Ported from Ryan Duguid's TypeScript fork.
use crate::sessions::SessionStore;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::{fs, path::Path, sync::LazyLock};

/// Quotes shorter than this, after whitespace is collapsed, are too common to be evidence.
const MIN_QUOTE_LENGTH: usize = 8;
/// What the session's prompt is called among the sent texts. It has no extension, so no
/// reference can name it, but the answer may quote it.
const PROMPT: &str = "prompt";
// shortcut: a name without a folder that ends in one of these common top-level domains reads as
// a host and port (`example.com:8080`), so it is not flagged unless a sent file has that name.
// The public suffix list is the upgrade path.
const HOST_SUFFIXES: &[&str] = &[
    "com", "net", "org", "io", "ai", "dev", "app", "co", "au", "uk", "local", "internal",
];

// An optional drive, home or root prefix, folders, then a file name with an extension of up to
// 16 characters, a dotfile or a conventional name without an extension, then :line or
// :start-end. `references` checks that the path does not continue a longer token, such as a URL
// host.
// shortcut: other names without an extension (`bin/run:4`) are not read as references, since
// `and/or:3` reads the same; matching them against the sent files' names is the upgrade path.
const REFERENCE: &str = r"((?:[A-Za-z]:[\\/]|~?/)?(?:[\p{L}\p{N}\p{M}_.-]+[\\/])*(?:[\p{L}\p{N}\p{M}_-][\p{L}\p{N}\p{M}_.-]*\.[A-Za-z][A-Za-z0-9]{0,15}|\.[A-Za-z][\p{L}\p{N}\p{M}_.-]*|Dockerfile|Containerfile|Makefile|GNUmakefile|Justfile|Rakefile|Gemfile|Procfile|Jenkinsfile|Vagrantfile|LICENSE|CODEOWNERS)):([0-9]+)(?:-([0-9]+))?";
static REFERENCE_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{REFERENCE}")).expect("reference pattern"));
static REFERENCE_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{REFERENCE}$")).expect("reference pattern"));
static FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})").expect("fence pattern"));
static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@").expect("hunk pattern")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}]$").expect("word pattern"));
static PATH_CHAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}\p{M}_./\\-]$").expect("path pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Grounded,
    OutOfRange,
    NotSent,
    /// A name such as `example.com:8080` that no sent file has: read as a host and port, and
    /// not flagged.
    Host,
}
/// Where the sent files' text came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SentFiles {
    /// The bytes the session saved when it sent them.
    #[default]
    Saved,
    /// No saved copy remains, so the session's file selection was read as it is now.
    Current,
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
    pub sent_files: SentFiles,
    pub references: Vec<ReferenceCheck>,
    pub quotes: Vec<QuoteCheck>,
    /// With current files only: those modified after the session was created.
    pub changed_files: Vec<String>,
    /// Sent files that are not UTF-8 text, so nothing was checked against them.
    pub skipped_files: Vec<String>,
}
impl Report {
    /// True when a reference is out of range or names a file not sent, or a quote is not found.
    pub fn flagged(&self) -> bool {
        self.references
            .iter()
            .any(|reference| matches!(reference.status, Status::OutOfRange | Status::NotSent))
            || self.quotes.iter().any(|quote| !quote.found)
    }
}
pub struct SentFile {
    /// Absolute, or relative to the session's working directory.
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
    PATH_CHAR.is_match(character.encode_utf8(&mut [0; 4]))
}
fn word(character: char) -> bool {
    WORD.is_match(character.encode_utf8(&mut [0; 4]))
}
fn absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/') || bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}
/// A lower-case path with forward slashes, joined to `cwd` unless absolute, without `.` or `..`.
fn key(cwd: &str, path: &str) -> String {
    let path = path.replace('\\', "/");
    let joined = if absolute(&path) {
        path
    } else {
        format!("{}/{path}", cwd.replace('\\', "/"))
    };
    let mut parts = vec![];
    for part in joined.split('/') {
        match part {
            "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/").to_lowercase()
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
/// The quoted spans in one line, by style and then position. A quote closes as the fork's
/// `\S(?:[^\n]*?\S)?` finds it: at the nearest mark after 2 or more characters, else after 1.
fn quotes(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let span = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
    let mut found = vec![];
    for style in &QUOTE_STYLES {
        let closes = |at: usize| {
            chars.get(at).is_some_and(|c| style.close.contains(c))
                && !(style.close_boundary && chars.get(at + 1).is_some_and(|c| word(*c)))
        };
        // The nearest index at or after each position that closes content ending in a
        // non-space, found once per line so that many unclosed quotes stay linear.
        let mut next = vec![None; chars.len() + 1];
        for at in (1..chars.len()).rev() {
            next[at] = if !chars[at - 1].is_whitespace() && closes(at) {
                Some(at)
            } else {
                next[at + 1]
            };
        }
        let mut at = 0;
        while at < chars.len() {
            let first = at + 1;
            let opens = chars[at] == style.open
                && !(style.open_boundary && at > 0 && word(chars[at - 1]))
                && chars.get(first).is_some_and(|c| !c.is_whitespace());
            let close = opens
                .then(|| {
                    next.get(first + 2)
                        .copied()
                        .flatten()
                        .or_else(|| closes(first + 1).then_some(first + 1))
                })
                .flatten();
            match close {
                Some(close) => {
                    found.push(span(first, close));
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
    /// Keys of the old and new paths from the `---` and `+++` headers.
    keys: Vec<String>,
    /// Line ranges the hunks show on either side, inclusive.
    ranges: Vec<(u64, u64)>,
}
/// A unified diff shows each side of a file with a marker column, which would split any quote
/// that wraps a line. Read each hunk's new and old sides back as text of their own, since lines
/// of different hunks or files are not adjacent, and record which lines each file's hunks show,
/// so `path:line` can be checked against them. Hunk line counts mark where each hunk ends, so a
/// changed line that looks like a header stays content.
fn read_diff(text: &str, cwd: &str) -> Option<(Vec<DiffSection>, Vec<String>)> {
    if !text.split('\n').any(|line| HUNK.is_match(line)) {
        return None;
    }
    let header = |line: &str, side: &str| {
        let path = line.split('\t').next().unwrap_or_default();
        key(cwd, path.strip_prefix(side).unwrap_or(path))
    };
    let mut sections: Vec<DiffSection> = vec![];
    let mut old_key = None;
    let (mut new_side, mut old_side) = (vec![], vec![]);
    let mut sides = vec![];
    fn flush(sides: &mut Vec<String>, hunk: [&mut Vec<&str>; 2]) {
        for side in hunk.into_iter().filter(|side| !side.is_empty()) {
            sides.push(normalise(&side.join("\n")));
            side.clear();
        }
    }
    let (mut old_left, mut new_left) = (0u64, 0u64);
    let mut in_hunk = false;
    for line in text.split('\n').map(|line| line.trim_end_matches('\r')) {
        let starts_header = ["--- ", "+++ ", "diff "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            || HUNK.is_match(line);
        // Within its stated count a hunk line is content, even one that looks like a header.
        // Past it, as in a hand-written diff that undercounts, content runs to the next header.
        if old_left > 0 || new_left > 0 || in_hunk && !starts_header {
            match line.chars().next() {
                // Some tools strip the space from an empty context line.
                None | Some(' ') => {
                    let context = line.get(1..).unwrap_or_default();
                    new_side.push(context);
                    old_side.push(context);
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some('-') => {
                    old_side.push(&line[1..]);
                    old_left = old_left.saturating_sub(1);
                    continue;
                }
                Some('+') => {
                    new_side.push(&line[1..]);
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some('\\') => continue,
                // A hunk shorter than its header says: read the line as a header.
                Some(_) => (old_left, new_left, in_hunk) = (0, 0, false),
            }
        }
        if starts_header {
            in_hunk = false;
            flush(&mut sides, [&mut new_side, &mut old_side]);
        }
        if let Some(path) = line.strip_prefix("--- ") {
            old_key = Some(header(path, "a/"));
        } else if let Some(path) = line.strip_prefix("+++ ") {
            sections.push(DiffSection {
                keys: old_key
                    .take()
                    .into_iter()
                    .chain([header(path, "b/")])
                    .collect(),
                ranges: vec![],
            });
        } else if let Some(hunk) = HUNK.captures(line) {
            let side = |start: usize, count: usize| {
                let first = number(&hunk[start]);
                let size = hunk.get(count).map_or(1, |count| number(count.as_str()));
                (first, size)
            };
            let (old, new) = (side(1, 2), side(3, 4));
            (old_left, new_left, in_hunk) = (old.1, new.1, true);
            if let Some(section) = sections.last_mut() {
                for (first, size) in [old, new].into_iter().filter(|&(_, size)| size > 0) {
                    section.ranges.push((first, first.saturating_add(size - 1)));
                }
            }
        }
    }
    flush(&mut sides, [&mut new_side, &mut old_side]);
    Some((sections, sides))
}

/// Checks the answer's references and quotes against the sent files. Relative paths are taken
/// from `cwd`, the session's working directory. Paths compare case-insensitively; a relative or
/// home (`~/`) reference names a file whose path ends with it, an absolute one only that file.
pub fn check(answer: &str, files: &[SentFile], cwd: &str) -> Report {
    struct Sent {
        key: String,
        lines: usize,
    }
    let sent: Vec<Sent> = files
        .iter()
        .map(|file| Sent {
            key: key(cwd, &file.path),
            lines: line_count(&file.text),
        })
        .collect();
    let mut sections = vec![];
    let mut texts: Vec<String> = files.iter().map(|file| normalise(&file.text)).collect();
    for (diff_sections, sides) in files.iter().filter_map(|file| read_diff(&file.text, cwd)) {
        sections.extend(diff_sections);
        texts.extend(sides);
    }
    let mut report = Report::default();
    let mut fence: Option<String> = None;
    for (index, line) in answer.split('\n').enumerate() {
        let answer_line = index + 1;
        match &fence {
            // Only a bare run of the opening character, at least as long, closes a fence.
            Some(open) => {
                let marker = line.trim();
                let character = open.chars().next();
                let closes =
                    marker.len() >= open.len() && marker.chars().all(|c| Some(c) == character);
                if closes {
                    fence = None;
                }
                continue;
            }
            None => {
                if let Some(marker) = FENCE.captures(line) {
                    fence = Some(marker[1].to_owned());
                    continue;
                }
            }
        }
        for captures in references(line) {
            let raw = captures[1].replace('\\', "/");
            let raw = raw.strip_prefix("./").unwrap_or(&raw);
            let (want, exact) = match raw.strip_prefix("~/") {
                Some(home) => (home.to_lowercase(), false),
                None if absolute(raw) => (key("", raw), true),
                None => (raw.to_lowercase(), false),
            };
            let suffix = format!("/{want}");
            let names = |path: &str| path == want || !exact && path.ends_with(&suffix);
            let start = number(&captures[2]);
            let last = captures
                .get(3)
                .map_or(start, |end| start.max(number(end.as_str())));
            let file_lines = sent
                .iter()
                .filter(|file| names(&file.key))
                .map(|file| file.lines)
                .max();
            // A diff can show lines that a sent copy of the same file does not have.
            let in_diff = || {
                sections.iter().any(|section| {
                    section.keys.iter().any(|key| names(key))
                        && section
                            .ranges
                            .iter()
                            .any(|&(first, end)| start >= first && last <= end)
                })
            };
            let status = match file_lines {
                Some(lines) if start >= 1 && last <= lines as u64 => Status::Grounded,
                _ if in_diff() => Status::Grounded,
                Some(_) => Status::OutOfRange,
                None if !want.contains('/')
                    && want
                        .rsplit_once('.')
                        .is_some_and(|(_, suffix)| HOST_SUFFIXES.contains(&suffix)) =>
                {
                    Status::Host
                }
                None => Status::NotSent,
            };
            report.references.push(ReferenceCheck {
                answer_line,
                reference: captures[0].to_owned(),
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

fn sections(text: &str) -> impl Iterator<Item = SentFile> {
    // `file_sections` drops each file's final line ending; restore it so a last blank line counts.
    crate::prompts::parse_file_sections(text)
        .into_iter()
        .map(|(path, mut text)| {
            if !text.is_empty() {
                text.push('\n');
            }
            SentFile { path, text }
        })
}
/// The texts a session sent, from its saved request: an API request body, or a browser prompt
/// and its staged uploads, with the uploads that are not text. None when no saved copy remains.
fn saved_files(
    store: &SessionStore,
    id: &str,
    meta: &Value,
) -> Result<Option<(Vec<SentFile>, Vec<String>)>> {
    let directory = store.directory(id)?;
    if directory.join("dispatch-request.json").is_file() {
        let body = crate::execution::saved_request(store, id)?.body;
        let user = body["input"][0]["content"][0]["text"]
            .as_str()
            .context("The saved API request has no prompt.")?;
        let mut files = vec![
            SentFile {
                path: PROMPT.into(),
                text: body["instructions"].as_str().unwrap_or_default().into(),
            },
            SentFile {
                path: PROMPT.into(),
                text: user.into(),
            },
        ];
        files.extend(sections(user));
        return Ok(Some((files, vec![])));
    }
    if !directory.join("browser-request.json").is_file() {
        return Ok(None);
    }
    let request = crate::browser_runner::saved_request(store, id)?;
    let mut files = vec![SentFile {
        path: PROMPT.into(),
        text: request.prompt.clone(),
    }];
    let mut skipped = vec![];
    let Some(staged) = &request.staged_uploads else {
        // A local run stages every file it sends.
        let none = meta["options"]["file"].as_array().is_none_or(Vec::is_empty);
        return Ok(none.then_some((files, skipped)));
    };
    // A run that closes its tab cleanly removes its staged uploads.
    if !staged.directory.is_dir() {
        return Ok(None);
    }
    let text_bundle = request
        .extra
        .get("bundled")
        .is_some_and(|bundled| bundled["format"] == "text");
    for (index, saved) in staged.files.iter().enumerate() {
        let text = crate::prompts::decode(staged.open(index)?.contents()?);
        match text {
            // ZIP bundles are not read.
            _ if saved.generated_bundle && !text_bundle => return Ok(None),
            Some(text) if saved.generated_bundle => files.extend(sections(&text)),
            Some(text) => files.push(SentFile {
                path: saved.display_path.clone(),
                text,
            }),
            None => skipped.push(saved.display_path.clone()),
        }
    }
    Ok(Some((files, skipped)))
}
/// The session's prompt and file selection as they are now, with the files modified since the
/// session was created and those that are not text.
fn current_files(meta: &Value, cwd: &str, id: &str) -> Result<(Vec<SentFile>, [Vec<String>; 2])> {
    let created = meta["createdAt"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .with_context(|| format!("Session \"{id}\" has no valid creation time."))?;
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
    for file in crate::prompts::resolve_files(&inputs, Path::new(cwd), 0, false)? {
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
                path: file.display_path,
                text,
            }),
            None => skipped.push(file.display_path),
        }
    }
    Ok((files, [changed, skipped]))
}
/// Checks a session's saved answer against what it sent: the bytes its saved request kept, or,
/// when none remain, its prompt and file selection as they are now.
pub fn session_report(store: &SessionStore, id: &str) -> Result<Report> {
    let meta = store.require(id)?;
    anyhow::ensure!(
        meta["browser"]["capturedAnswer"].is_object() || meta["capturedAnswer"].is_object(),
        "Session \"{id}\" has no saved answer to check."
    );
    let answer = String::from_utf8(crate::browser_runner::captured(store, id)?)
        .context("The saved answer is not UTF-8")?;
    let cwd = meta["cwd"]
        .as_str()
        .with_context(|| format!("Session \"{id}\" has no working directory."))?;
    let (files, sent_files, [changed, skipped]) = match saved_files(store, id, &meta)? {
        Some((files, skipped)) => (files, SentFiles::Saved, [vec![], skipped]),
        None => {
            let (files, lists) = current_files(&meta, cwd, id)?;
            (files, SentFiles::Current, lists)
        }
    };
    let mut report = check(&answer, &files, cwd);
    report.sent_files = sent_files;
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
    let hosts = match count(Status::Host) {
        0 => String::new(),
        n => format!(", {n} read as {}", if n == 1 { "a host" } else { "hosts" }),
    };
    let mut lines = vec![format!(
        "Grounding: {} ({} grounded, {} out of range, {} not in the files sent{hosts}); {} ({found} found, {} not found).",
        plural(report.references.len(), "reference"),
        count(Status::Grounded),
        count(Status::OutOfRange),
        count(Status::NotSent),
        plural(report.quotes.len(), "quote"),
        report.quotes.len() - found,
    )];
    if report.sent_files == SentFiles::Current {
        lines.push(
            "The session kept no copy of the files it sent, so they were read as they are now."
                .into(),
        );
    }
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
    fn checked(answer: &str, files: &[SentFile]) -> Report {
        check(answer, files, "/work/repo")
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
        let report = checked(
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
        let report = checked(
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
        assert!(checked("Compare docs/scope.md:134.", &[engine()]).flagged());
    }
    #[test]
    fn ignores_url_hosts_with_ports_and_line_0_is_out_of_range() {
        let report = checked(
            "Open https://example.com:443/x then engine.rs:0.",
            &[engine()],
        );
        assert_eq!(statuses(&report), [("engine.rs:0", Status::OutOfRange)]);
        // A reference starts its token, as with the fork's lookbehind, so the dotfiles here are
        // read whole and never as the sent engine.rs.
        assert_eq!(
            statuses(&checked(
                "See .engine.rs:2 and x/.engine.rs:2 or ..engine.rs:2",
                &[engine()]
            )),
            [
                (".engine.rs:2", Status::NotSent),
                ("x/.engine.rs:2", Status::NotSent)
            ]
        );
    }
    #[test]
    fn finds_quotes_across_wrapped_lines_and_curly_quotes_and_flags_invented_ones() {
        let report = checked(
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
        let report = checked(
            "Ryan's note doesn't say 'whilst'; see `engine.rs:2` and `let status`.",
            &[engine()],
        );
        assert_eq!(found(&report), [("let status", true)]);
        // 7 characters is under the minimum; 8 is checked.
        let report = checked("`1234567` and `12345678`", &[engine()]);
        assert_eq!(found(&report), [("12345678", false)]);
    }
    #[test]
    fn skips_lines_inside_fenced_code_blocks() {
        let report = checked(
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
        let report = checked(
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
        let report = checked(
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
        let mut report = checked(
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

    #[test]
    fn absolute_and_home_references_are_checked() {
        let report = checked(
            r"See /work/packages/close/src/engine.rs:2, C:\work\src\engine.rs:99, /work/src/engine.rs:99, ~/repo/nope.rs:5 and ~/close/src/engine.rs:9.",
            &[engine()],
        );
        assert_eq!(
            statuses(&report),
            [
                ("/work/packages/close/src/engine.rs:2", Status::Grounded),
                (r"C:\work\src\engine.rs:99", Status::NotSent),
                ("/work/src/engine.rs:99", Status::NotSent),
                ("~/repo/nope.rs:5", Status::NotSent),
                ("~/close/src/engine.rs:9", Status::OutOfRange)
            ]
        );
        // An absolute path names that file only, and a drive letter compares like any segment.
        let windows = SentFile {
            path: r"C:\Work\src\engine.rs".into(),
            text: "one\n".into(),
        };
        let report = checked("/src/engine.rs:1 and c:/work/src/engine.rs:1", &[windows]);
        assert_eq!(
            statuses(&report),
            [
                ("/src/engine.rs:1", Status::NotSent),
                ("c:/work/src/engine.rs:1", Status::Grounded)
            ]
        );
    }
    fn lib_diff() -> SentFile {
        SentFile {
            path: "/work/frozen/lib.diff".into(),
            text: [
                "--- a/src/lib.c",
                "+++ b/src/lib.c",
                "@@ -100,4 +100,2 @@",
                "-first removed",
                "-second removed",
                "-third removed",
                "+one added",
                " kept",
            ]
            .join("\n"),
        }
    }
    #[test]
    fn a_diff_grounds_citations_of_either_side() {
        let report = checked(
            "Removed at src/lib.c:101-103; kept at src/lib.c:101; src/lib.c:104 was never shown.",
            &[lib_diff()],
        );
        assert_eq!(
            statuses(&report),
            [
                ("src/lib.c:101-103", Status::Grounded),
                ("src/lib.c:101", Status::Grounded),
                ("src/lib.c:104", Status::NotSent)
            ]
        );
    }
    #[test]
    fn long_extensions_dotfiles_and_conventional_names_are_references() {
        let ignore = SentFile {
            path: "/work/repo/.gitignore".into(),
            text: "target\n".into(),
        };
        let report = checked(
            "See gradle.properties:900, .gitignore:900, Dockerfile:900, src/Makefile:2, \
             .gitignore:1 and a/b/c.properties:3.",
            &[engine(), ignore],
        );
        assert_eq!(
            statuses(&report),
            [
                ("gradle.properties:900", Status::NotSent),
                (".gitignore:900", Status::OutOfRange),
                ("Dockerfile:900", Status::NotSent),
                ("src/Makefile:2", Status::NotSent),
                (".gitignore:1", Status::Grounded),
                ("a/b/c.properties:3", Status::NotSent)
            ]
        );
        assert!(report.flagged());
        // Words joined by a slash still read as prose.
        assert!(
            checked("Use and/or:3 here.", &[engine()])
                .references
                .is_empty()
        );
    }
    #[test]
    fn a_diff_grounds_lines_a_sent_copy_of_the_file_lacks() {
        let snapshot = SentFile {
            path: "/work/repo/src/lib.c".into(),
            text: "one added\n".into(),
        };
        let report = checked(
            "Removed at src/lib.c:101; src/lib.c:104 was never shown.",
            &[snapshot, lib_diff()],
        );
        assert_eq!(
            statuses(&report),
            [
                ("src/lib.c:101", Status::Grounded),
                ("src/lib.c:104", Status::OutOfRange)
            ]
        );
    }
    #[test]
    fn a_quote_does_not_join_lines_of_different_files_or_hunks() {
        let diff = SentFile {
            path: "/work/frozen/two.diff".into(),
            text: [
                "--- a/a.txt",
                "+++ b/a.txt",
                "@@ -1,0 +1 @@",
                "+alpha one",
                "@@ -9,0 +9 @@",
                "+charlie three",
                "--- a/b.txt",
                "+++ b/b.txt",
                "@@ -1,0 +1 @@",
                "+bravo two",
            ]
            .join("\n"),
        };
        let report = checked(
            "'charlie three bravo two' and 'alpha one charlie three' but 'charlie three'",
            &[diff],
        );
        assert_eq!(
            found(&report),
            [
                ("charlie three bravo two", false),
                ("alpha one charlie three", false),
                ("charlie three", true)
            ]
        );
    }
    #[test]
    fn a_diff_path_takes_only_the_session_folders_as_an_added_prefix() {
        let report = checked(
            "other/src/lib.c:100 and repo/src/lib.c:100 and /work/repo/src/lib.c:100",
            &[lib_diff()],
        );
        assert_eq!(
            statuses(&report),
            [
                ("other/src/lib.c:100", Status::NotSent),
                ("repo/src/lib.c:100", Status::Grounded),
                ("/work/repo/src/lib.c:100", Status::Grounded)
            ]
        );
    }
    #[test]
    fn changed_lines_that_look_like_diff_headers_stay_content() {
        let diff = SentFile {
            path: "/work/frozen/headers.diff".into(),
            text: [
                "--- a/src/a.c",
                "+++ b/src/a.c",
                "@@ -1,2 +1,2 @@",
                " keep",
                "--- old comment",
                "+++ new comment",
                "@@ -10 +10 @@",
                "-old line here",
                "+new line here",
            ]
            .join("\n"),
        };
        // Read as headers, those lines would leave each side without them and give the second
        // hunk to a file called "new comment".
        let report = checked(
            "`keep -- old comment` became `keep ++ new comment` at src/a.c:2, and src/a.c:10 changed.",
            &[diff],
        );
        assert_eq!(
            found(&report),
            [("keep -- old comment", true), ("keep ++ new comment", true)]
        );
        assert_eq!(
            statuses(&report),
            [
                ("src/a.c:2", Status::Grounded),
                ("src/a.c:10", Status::Grounded)
            ]
        );
    }
    #[test]
    fn a_fence_with_an_info_string_does_not_close_an_open_fence() {
        let report = checked(
            "```\nengine.rs:99\n```rust\nengine.rs:98\n```\nengine.rs:97",
            &[engine()],
        );
        assert_eq!(statuses(&report), [("engine.rs:97", Status::OutOfRange)]);
        assert_eq!(report.references[0].answer_line, 6);
    }
    #[test]
    fn hosts_with_ports_are_reported_unflagged_and_non_ascii_paths_are_whole() {
        let sent = SentFile {
            path: "données/engine.rs".into(),
            text: "one\ntwo\n".into(),
        };
        let report = checked(
            "Call api.openai.com:443 or example.com:8080; see données/engine.rs:2 and 数据/nope.rs:3.",
            &[sent],
        );
        assert_eq!(
            statuses(&report),
            [
                ("api.openai.com:443", Status::Host),
                ("example.com:8080", Status::Host),
                ("données/engine.rs:2", Status::Grounded),
                ("数据/nope.rs:3", Status::NotSent)
            ]
        );
        let hosts = checked("Call api.openai.com:443 or example.com:8080.", &[engine()]);
        assert!(!hosts.flagged());
        assert_eq!(
            format_report(&hosts),
            "Grounding: 2 references (0 grounded, 0 out of range, 0 not in the files sent, 2 read as hosts); 0 quotes (0 found, 0 not found)."
        );
        // A sent file with a host-like name is still checked.
        let notes = SentFile {
            path: "notes.com".into(),
            text: "one\n".into(),
        };
        assert_eq!(
            statuses(&checked("notes.com:3", &[notes])),
            [("notes.com:3", Status::OutOfRange)]
        );
    }
    #[test]
    fn many_unclosed_quotes_on_one_line_stay_fast() {
        let line = "'a ".repeat(40_000);
        let started = std::time::Instant::now();
        assert!(checked(&line, &[engine()]).quotes.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
    #[test]
    fn saved_sections_keep_each_files_line_count() {
        let file = |content: &str| crate::prompts::PromptFile {
            path: "a.txt".into(),
            display_path: "a.txt".into(),
            content: content.into(),
            size_bytes: 0,
            scanned_sha256: None,
        };
        for content in ["one\n\n", "one", "a\nb\n", "x\r\n", ""] {
            for indexed in [false, true] {
                for numbers in [false, true] {
                    let written = crate::prompts::file_sections(&[file(content)], indexed, numbers);
                    let sent: Vec<SentFile> = sections(&written).collect();
                    assert_eq!(sent.len(), 1);
                    assert_eq!(
                        line_count(&sent[0].text),
                        line_count(content),
                        "{content:?}, indexed {indexed}, numbers {numbers}"
                    );
                }
            }
        }
        let prompt = crate::prompts::build_user_prompt("Review.", &[file("one\n\n")]);
        let sent: Vec<SentFile> = sections(&prompt).collect();
        assert_eq!(
            statuses(&checked("a.txt:2", &sent)),
            [("a.txt:2", Status::Grounded)]
        );
    }
    #[test]
    fn lines_past_an_undercounted_hunk_stay_content_until_the_next_header() {
        let diff = SentFile {
            path: "/work/review.diff".into(),
            text: [
                "--- a/src/lib.c",
                "+++ b/src/lib.c",
                "@@ -10,1 +10,1 @@",
                "-int b;",
                "+int c;",
                "+the second added line",
                "+and a third one here",
                "--- a/src/other.c",
                "+++ b/src/other.c",
                "@@ -1 +1 @@",
                "-before",
                "+after",
                "",
            ]
            .join("\n"),
        };
        let report = checked(
            "Added 'the second added line and a third one here'; src/other.c:1 changed.",
            &[diff],
        );
        assert_eq!(
            found(&report),
            [("the second added line and a third one here", true)]
        );
        assert_eq!(statuses(&report), [("src/other.c:1", Status::Grounded)]);
    }
}
