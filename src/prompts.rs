use anyhow::{Context, Result, bail, ensure};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub const DEFAULT_SYSTEM: &str = "You are Oracle, a focused one-shot problem solver. Emphasize direct answers and cite referenced files as path:line or path:line-line when line numbers are available.";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const IGNORED: &[&str] = &[
    "node_modules",
    "dist",
    "coverage",
    ".git",
    ".turbo",
    ".next",
    "build",
    "tmp",
];
/// Credential and private-key file names that are never bundled, compared case-insensitively.
const SENSITIVE_NAMES: &[&str] = &[
    ".npmrc",
    ".pypirc",
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".htpasswd",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];
const SENSITIVE_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks", "keystore", "kdbx"];
const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template", ".env.dist"];
fn sensitive_name(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_lowercase();
    if ENV_TEMPLATES.contains(&name.as_str()) {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || SENSITIVE_NAMES.contains(&name.as_str())
        || Path::new(&name)
            .extension()
            .is_some_and(|extension| SENSITIVE_EXTENSIONS.contains(&&*extension.to_string_lossy()))
}
/// A link is judged by its target too, so an innocuous name cannot alias a credential file.
fn sensitive(path: &Path) -> bool {
    sensitive_name(path) || fs::canonicalize(path).is_ok_and(|target| sensitive_name(&target))
}
/// Git rules apply from the enclosing repository root, or from the working directory outside one.
fn rules_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|directory| fs::symlink_metadata(directory.join(".git")).is_ok())
        .unwrap_or(cwd)
        .to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFile {
    pub path: PathBuf,
    pub display_path: String,
    pub content: String,
    pub size_bytes: u64,
}
fn dynamic(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{', '('])
}
fn matcher(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(&pattern.replace('\\', "/"))
        .literal_separator(true)
        .build()?
        .compile_matcher())
}
pub fn collect_paths(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|s| split_top_level_commas(s))
        .map(crate::policy::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut open = vec![];
    let mut closes = std::collections::HashMap::new();
    for (index, byte) in value.bytes().enumerate() {
        if byte == b'{' {
            open.push(index);
        } else if byte == b'}'
            && let Some(start) = open.pop()
        {
            closes.insert(start, index);
        }
    }
    let mut parts = vec![];
    let mut start = 0;
    let mut group_end = None;
    for (index, byte) in value.bytes().enumerate() {
        if group_end.is_some_and(|end| index <= end) {
            continue;
        }
        if let Some(&close) = closes.get(&index) {
            group_end = Some(close);
        } else if byte == b',' {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    parts
}
/// Reference assertBraceNesting: every opener counts, and only a matching closer outside a
/// bracket expression lowers the depth, so the scan over-counts rather than parses.
fn assert_brace_nesting(patterns: &[String]) -> Result<()> {
    for pattern in patterns {
        let mut open = vec![];
        let mut maximum = 0;
        let mut in_bracket = false;
        for character in pattern.chars() {
            if matches!(character, '{' | '(') {
                open.push(character);
                maximum = maximum.max(open.len());
            } else if in_bracket {
                in_bracket = character != ']';
            } else if character == '[' {
                in_bracket = true;
            } else if (character == '}' && open.last() == Some(&'{'))
                || (character == ')' && open.last() == Some(&'('))
            {
                open.pop();
            }
        }
        anyhow::ensure!(
            maximum <= 32,
            "Glob pattern nests braces or parentheses {maximum} levels deep; the limit is 32."
        );
    }
    Ok(())
}

fn relative(path: &Path, cwd: &Path) -> String {
    let p: Vec<_> = path.components().collect();
    let c: Vec<_> = cwd.components().collect();
    let shared = p.iter().zip(&c).take_while(|(a, b)| a == b).count();
    if shared == 0 {
        return path.to_string_lossy().replace('\\', "/");
    }
    let mut output = PathBuf::new();
    for _ in shared..c.len() {
        output.push("..");
    }
    for part in &p[shared..] {
        output.push(part.as_os_str());
    }
    output.to_string_lossy().replace('\\', "/")
}
pub(crate) fn absolute(cwd: &Path, value: &str) -> PathBuf {
    let raw = Path::new(value);
    let joined = if raw.is_absolute() {
        raw.to_owned()
    } else {
        cwd.join(raw)
    };
    let mut output = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                output.pop();
            }
            _ => output.push(component),
        }
    }
    output
}
struct FileWalker<'a> {
    cwd: &'a Path,
    root: &'a Path,
    literal: &'a [PathBuf],
    directories: &'a [PathBuf],
    excludes: &'a [GlobMatcher],
    prunes: &'a [GlobMatcher],
}
impl FileWalker<'_> {
    fn allowed(&self, path: &Path) -> bool {
        self.literal.iter().any(|p| p == path)
            || self
                .directories
                .iter()
                .filter(|d| {
                    d.file_name()
                        .is_some_and(|n| IGNORED.contains(&n.to_string_lossy().as_ref()))
                })
                .any(|d| path.starts_with(d))
    }
    fn default_ignored(&self, path: &Path) -> bool {
        if self.allowed(path) {
            return false;
        }
        let rel = relative(path, self.cwd);
        let mut ancestor = self.cwd.to_owned();
        for part in Path::new(&rel).components() {
            ancestor.push(part);
            if IGNORED.contains(&part.as_os_str().to_string_lossy().as_ref())
                && !ancestor.join(".gitignore").is_file()
            {
                return true;
            }
        }
        false
    }
    /// A rule file that cannot be applied fails the selection rather than widening it.
    fn rules_at(directory: &Path) -> Result<Option<ignore::gitignore::Gitignore>> {
        let path = directory.join(".gitignore");
        // Read only rule files on requested or reached paths. Never discover them with a whole-tree pass.
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", path.display()));
            }
        };
        ensure!(
            metadata.is_file() && !metadata.is_symlink(),
            "Cannot apply {}: it is not a regular file",
            path.display()
        );
        let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
        if let Some(error) = builder.add(&path) {
            return Err(error).with_context(|| format!("Cannot apply {}", path.display()));
        }
        Ok(Some(builder.build().with_context(|| {
            format!("Cannot apply {}", path.display())
        })?))
    }
    fn ancestor_rules(&self, path: &Path) -> Result<Vec<ignore::gitignore::Gitignore>> {
        let mut directories = vec![];
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if !directory.starts_with(self.root) {
                break;
            }
            directories.push(directory);
            if directory == self.root {
                break;
            }
            parent = directory.parent();
        }
        let mut rules = vec![];
        for directory in directories.into_iter().rev() {
            rules.extend(Self::rules_at(directory)?);
        }
        Ok(rules)
    }
    fn git_ignored(path: &Path, directory: bool, rules: &[ignore::gitignore::Gitignore]) -> bool {
        for rules in rules.iter().rev() {
            let matched = rules.matched(path, directory);
            if matched.is_whitelist() {
                return false;
            }
            if matched.is_ignore() {
                return true;
            }
        }
        false
    }
    fn excluded(&self, path: &Path, directory: bool) -> bool {
        self.excludes.iter().any(|m| m.is_match(path))
            || directory && self.prunes.iter().any(|m| m.is_match(path))
    }
    fn walk(
        &self,
        directory: &Path,
        mut rules: Vec<ignore::gitignore::Gitignore>,
        files: &mut BTreeSet<PathBuf>,
        visit: &mut impl FnMut(&Path),
        depth: Option<usize>,
    ) -> Result<()> {
        if self.default_ignored(directory)
            || self.excluded(directory, true)
            || Self::git_ignored(directory, true, &rules)
        {
            return Ok(());
        }
        visit(directory);
        if let Some(own) = Self::rules_at(directory)? {
            rules.push(own);
        }
        let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink()
                || self.excluded(&path, kind.is_dir())
                || self.default_ignored(&path)
                || Self::git_ignored(&path, kind.is_dir(), &rules)
            {
                continue;
            }
            if kind.is_dir() {
                if depth.is_none_or(|depth| depth > 1) {
                    self.walk(&path, rules.clone(), files, visit, depth.map(|d| d - 1))?;
                }
            } else if kind.is_file() {
                files.insert(path);
            }
        }
        Ok(())
    }
    fn literal_ignored(&self, path: &Path) -> Result<bool> {
        let rules = self.ancestor_rules(path)?;
        let mut ancestor = path.parent();
        while let Some(directory) = ancestor {
            if !directory.starts_with(self.root) {
                break;
            }
            if Self::git_ignored(directory, true, &rules) {
                return Ok(true);
            }
            if directory == self.root {
                break;
            }
            ancestor = directory.parent();
        }
        Ok(Self::git_ignored(path, false, &rules))
    }
}
pub fn resolve_files(
    inputs: &[String],
    cwd: &Path,
    max_bytes: u64,
    read_contents: bool,
) -> Result<Vec<PromptFile>> {
    resolve_files_with(inputs, cwd, max_bytes, read_contents, &mut |_| {})
}
fn resolve_files_with(
    inputs: &[String],
    cwd: &Path,
    max_bytes: u64,
    read_contents: bool,
    visit: &mut impl FnMut(&Path),
) -> Result<Vec<PromptFile>> {
    assert_brace_nesting(inputs)?;
    if inputs.is_empty() {
        return Ok(vec![]);
    }
    let mut literal = vec![];
    let mut directories = vec![];
    let mut patterns = vec![];
    let mut excludes = vec![];
    let mut prunes = vec![];
    for raw in inputs
        .iter()
        .map(|s| crate::policy::trim(s))
        .filter(|s| !s.is_empty())
    {
        if let Some(exclude) = raw.strip_prefix('!') {
            if exclude.is_empty() {
                continue;
            }
            excludes.push(matcher(&absolute(cwd, exclude).to_string_lossy())?);
            if let Some(prefix) = exclude
                .strip_suffix("/**/*")
                .or_else(|| exclude.strip_suffix("/**"))
            {
                prunes.push(matcher(&absolute(cwd, prefix).to_string_lossy())?);
            }
            continue;
        }
        if dynamic(raw) {
            patterns.push(absolute(cwd, raw));
            continue;
        }
        let path = absolute(cwd, raw);
        let metadata =
            fs::metadata(&path).with_context(|| format!("Missing file or directory: {raw}"))?;
        if metadata.is_dir() {
            directories.push(path);
        } else if metadata.is_file() {
            literal.push(path);
        } else {
            bail!("Not a file or directory: {raw}");
        }
    }
    let expanded = !directories.is_empty() || !patterns.is_empty() || !excludes.is_empty();
    let dot_opt_in = patterns
        .iter()
        .chain(&literal)
        .chain(&directories)
        .any(|p| {
            relative(p, cwd)
                .split('/')
                .any(|s| s.starts_with('.') && s.len() > 1)
        });
    let root = rules_root(cwd);
    let walker = FileWalker {
        cwd,
        root: &root,
        literal: &literal,
        directories: &directories,
        excludes: &excludes,
        prunes: &prunes,
    };
    let mut candidates = BTreeSet::new();
    candidates.extend(literal.iter().cloned());
    for dir in &directories {
        walker.walk(
            dir,
            walker.ancestor_rules(dir)?,
            &mut candidates,
            visit,
            None,
        )?;
    }
    for pattern in &patterns {
        let matching = matcher(&pattern.to_string_lossy())?;
        let mut root = PathBuf::new();
        for part in pattern.components() {
            if dynamic(&part.as_os_str().to_string_lossy()) {
                break;
            }
            root.push(part);
        }
        let mut possible = BTreeSet::new();
        if root.is_dir() {
            let remainder = pattern.strip_prefix(&root)?;
            let depth = if remainder
                .components()
                .any(|c| c.as_os_str().to_string_lossy().contains("**"))
            {
                None
            } else {
                Some(remainder.components().count())
            };
            walker.walk(
                &root,
                walker.ancestor_rules(&root)?,
                &mut possible,
                visit,
                depth,
            )?;
        }
        candidates.extend(possible.into_iter().filter(|path| matching.is_match(path)));
    }
    let ordered: Vec<_> = if expanded {
        candidates.into_iter().collect()
    } else {
        let mut seen = BTreeSet::new();
        literal
            .iter()
            .filter(|path| seen.insert((*path).clone()))
            .cloned()
            .collect()
    };
    let mut accepted = vec![];
    let mut oversized = vec![];
    let mut refused = vec![];
    let mut skipped = vec![];
    for path in ordered {
        if excludes.iter().any(|e| e.is_match(&path)) {
            continue;
        }
        if expanded
            && !dot_opt_in
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if expanded && literal.contains(&path) && walker.literal_ignored(&path)? {
            continue;
        }
        if walker.default_ignored(&path) {
            continue;
        }
        if sensitive(&path) {
            if literal.contains(&path) {
                refused.push(relative(&path, cwd));
            } else {
                skipped.push(relative(&path, cwd));
            }
            continue;
        }
        // Reference validation: a missing path fails at once, every oversized file is listed.
        let metadata = fs::metadata(&path)
            .with_context(|| format!("Missing file or directory: {}", relative(&path, cwd)))?;
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        if max_bytes != 0 && size > max_bytes {
            oversized.push(format!("{} ({})", relative(&path, cwd), format_bytes(size)));
        } else {
            accepted.push((path, size));
        }
    }
    if !refused.is_empty() {
        bail!(
            "Refusing files that look like credentials or private keys:\n- {}\nCopy any content that is safe to share into a file with another name.",
            refused.join("\n- ")
        );
    }
    if !skipped.is_empty() {
        eprintln!(
            "Skipped files that look like credentials or private keys: {}",
            skipped.join(", ")
        );
    }
    if !oversized.is_empty() {
        bail!(
            "The following files exceed the {} limit:\n- {}",
            format_bytes(max_bytes),
            oversized.join("\n- ")
        );
    }
    let mut output = vec![];
    let mut undecodable = vec![];
    for (path, size) in accepted {
        let content = if read_contents {
            match read_text(&path)? {
                Some(content) => content,
                None => {
                    undecodable.push(relative(&path, cwd));
                    continue;
                }
            }
        } else {
            String::new()
        };
        output.push(PromptFile {
            display_path: relative(&path, cwd),
            path,
            content,
            size_bytes: size,
        });
    }
    if !undecodable.is_empty() {
        bail!(
            "The following files are not UTF-8 text:\n- {}",
            undecodable.join("\n- ")
        );
    }
    if output.is_empty() {
        bail!("No files matched the provided --file patterns.");
    }
    Ok(output)
}
/// Reads a file as text; `None` when it is not valid UTF-8, so binary bytes are never garbled into a prompt.
pub fn read_text(path: &Path) -> Result<Option<String>> {
    Ok(String::from_utf8(fs::read(path)?).ok())
}
fn language(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "ts" => "ts",
        "tsx" => "tsx",
        "js" => "js",
        "jsx" => "jsx",
        "json" => "json",
        "swift" => "swift",
        "md" => "md",
        "sh" | "bash" | "zsh" => "bash",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "css" => "css",
        "scss" => "scss",
        "sql" => "sql",
        "yaml" | "yml" => "yaml",
        _ => "",
    }
}
pub fn file_sections(files: &[PromptFile], indexed: bool, numbers: bool) -> String {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let header = if indexed {
                format!("### File {}: {}", index + 1, file.display_path)
            } else {
                format!("### File: {}", file.display_path)
            };
            let ticks = file
                .content
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(3.max(ticks + 1));
            let content = crate::policy::trim_end(&file.content);
            let mut lines = vec![header];
            let numbered = if numbers {
                let split: Vec<_> = if content.is_empty() {
                    vec![]
                } else {
                    content.split('\n').collect()
                };
                let count = split.len();
                lines.push(if count == 0 {
                    "Lines: 0".into()
                } else {
                    format!("Lines: 1-{count}")
                });
                let width = count.to_string().len();
                split
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{:>width$} | {line}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                content.to_owned()
            };
            lines.extend([
                format!("{fence}{}", language(&file.display_path)),
                numbered,
                fence,
            ]);
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
pub fn render(system: &str, prompt: &str, files: &[PromptFile]) -> String {
    let raw = format!(
        "[SYSTEM]\n{system}\n\n[USER]\n{}\n\n{}",
        crate::policy::trim(prompt),
        file_sections(files, false, true)
    );
    let mut output = String::new();
    let mut newlines = 0;
    for c in raw.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                output.push(c);
            }
        } else {
            newlines = 0;
            output.push(c);
        }
    }
    crate::policy::trim_end(&output).to_owned()
}
pub fn build_user_prompt(prompt: &str, files: &[PromptFile]) -> String {
    if files.is_empty() {
        prompt.to_owned()
    } else {
        format!(
            "{}\n\n{}",
            crate::policy::trim(prompt),
            file_sections(files, true, true)
        )
    }
}
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", scaled(bytes as f64 / (1024.0 * 1024.0)))
    } else if bytes >= 1024 {
        format!("{} KB", scaled(bytes as f64 / 1024.0))
    } else {
        format!("{bytes} B")
    }
}
fn scaled(value: f64) -> String {
    format!("{value:.1}").trim_end_matches(".0").to_owned()
}

#[cfg(test)]
mod traversal_tests {
    use super::*;
    #[test]
    fn requested_glob_prunes_unrelated_and_excluded_caches_before_loading_rules() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        for directory in [
            "src",
            "src/cache/deep",
            "node_modules/deep",
            "unrelated/deep",
        ] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::write(root.join("src/public.rs"), "PUBLIC").unwrap();
        fs::write(root.join("src/cache/deep/.gitignore"), "!").unwrap();
        fs::write(root.join("node_modules/deep/hidden.rs"), "PUBLIC hidden").unwrap();
        let mut visited = vec![];
        let files = resolve_files_with(
            &["src/**/*.rs".into(), "!src/cache/**".into()],
            root,
            1024,
            true,
            &mut |p| visited.push(p.to_owned()),
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(visited, vec![root.join("src")]);
        visited.clear();
        resolve_files_with(
            &[
                "**/*.rs".into(),
                "!src/cache/**".into(),
                "!unrelated/**".into(),
            ],
            root,
            1024,
            true,
            &mut |p| visited.push(p.to_owned()),
        )
        .unwrap();
        assert_eq!(visited, vec![root.to_owned(), root.join("src")]);
    }
}

#[cfg(test)]
mod bounded_depth_tests {
    use super::*;
    #[test]
    fn a_non_recursive_glob_does_not_visit_deeper_unmatched_directories() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        fs::create_dir_all(root.join("src/unrelated/deep/cache")).unwrap();
        fs::write(root.join("src/public.rs"), "PUBLIC").unwrap();
        let mut visited = vec![];
        let files = resolve_files_with(&["src/*.rs".into()], root, 1024, true, &mut |p| {
            visited.push(p.to_owned())
        })
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(visited, vec![root.join("src")]);
    }
}
