//! Browser prompt assembly, raw uploads and owned text/ZIP bundles.
use crate::{
    browser_bundle::{ZipEntry, stored_zip},
    native_fs::{NativeEntry, remove_owned_directory},
    policy,
    prompts::{self, PromptFile},
    storage::private_file,
    tokenizer,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MEDIA: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "webm", "m4v", "mp3", "wav", "aac", "flac", "ogg", "m4a", "jpg",
    "jpeg", "png", "gif", "webp", "bmp", "svg", "heic", "heif", "pdf",
];
const ARCHIVES: &[&str] = &[
    "7z", "aab", "apk", "br", "bz2", "cab", "crx", "deb", "dmg", "doc", "docx", "ear", "epub",
    "gz", "ipa", "iso", "jar", "lz", "lz4", "msi", "odp", "ods", "odt", "pkg", "ppt", "pptx",
    "rar", "rpm", "tar", "tgz", "war", "whl", "xls", "xlsx", "xz", "xpi", "zip", "zipx", "zst",
];
/// Reference describeBrowserControlPlan and formatBrowserControlPlan, for previews and runs.
pub(crate) fn control_plan_lines(config: &Value, label: &str) -> Vec<String> {
    let tab = config["browserTabRef"]
        .as_str()
        .map(policy::trim)
        .unwrap_or("");
    let mut guidance = vec![];
    let (summary, focus) = if config["attachRunning"] == true {
        guidance.push(if tab.is_empty(){"Alcuin opens a dedicated tab and leaves the existing browser process alone.".into()}else{format!("Alcuin reuses the matching ChatGPT tab ({tab}) and leaves the existing browser process alone.")});
        if config["keepBrowser"] == true {
            guidance.push("The browser stays open because Alcuin did not launch it.".into());
        }
        (
            if tab.is_empty() {
                "attach to an already-running local Chrome session"
            } else {
                "attach to an already-running local Chrome tab"
            },
            true,
        )
    } else if config["remoteChrome"].is_object()
        || config["remoteChrome"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    {
        guidance.push(if tab.is_empty(){"Alcuin opens a dedicated tab in the configured remote Chrome session.".into()}else{format!("Alcuin reuses the matching ChatGPT tab ({tab}) in the configured remote Chrome session.")});
        guidance
            .push("Local Chrome launch, cookie copy, and window hiding flags are skipped.".into());
        (
            if tab.is_empty() {
                "reuse an existing remote Chrome session"
            } else {
                "reuse an existing remote Chrome tab"
            },
            false,
        )
    } else if config["headless"] == true {
        guidance.push(
            "Headless mode avoids visible UI but may be blocked by ChatGPT or Cloudflare.".into(),
        );
        ("launch headless Chrome", false)
    } else if config["hideWindow"] == true {
        guidance.push(
            if cfg!(target_os = "macos") {
                "On macOS, Alcuin launches Chrome off-screen while keeping the page rendered."
            } else {
                "--browser-hide-window has no effect on Windows or Linux; Chrome stays visible."
            }
            .into(),
        );
        guidance.push("For the calmest shared-desktop flow, prefer --browser-attach-running or --remote-chrome.".into());
        (
            if cfg!(target_os = "macos") {
                "launch Chrome in hidden-window mode"
            } else {
                "launch visible Chrome"
            },
            true,
        )
    } else {
        guidance.push(if config["manualLogin"]==true {"Manual-login mode may show the persistent Alcuin Chrome profile for sign-in or automation."}else{"A visible automation Chrome window may take focus while Alcuin controls ChatGPT."}.into());
        guidance.push(
            "Use --browser-attach-running or --remote-chrome to reduce desktop disruption.".into(),
        );
        if config["keepBrowser"] == true {
            guidance.push(
                "Chrome will remain open after the run because --browser-keep-browser is enabled."
                    .into(),
            );
        }
        ("launch visible Chrome", true)
    };
    let risk = if focus {
        "may focus/control the browser UI"
    } else {
        "does not use a visible local browser window"
    };
    std::iter::once(format!("[{label}] Browser control: {summary}; {risk}."))
        .chain(
            guidance
                .into_iter()
                .map(|line: String| format!("[{label}] Browser guidance: {line}")),
        )
        .collect()
}
pub fn raw_upload(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_lowercase();
    MEDIA.contains(&extension.as_str()) || ARCHIVES.contains(&extension.as_str())
}
pub fn discover(inputs: &[String], cwd: &Path, maximum: Option<u64>) -> Result<Vec<PromptFile>> {
    let mut files = prompts::resolve_files(inputs, cwd, 0, false)?;
    for file in &mut files {
        let raw = raw_upload(&file.path);
        let limit = if raw {
            maximum.unwrap_or(0)
        } else {
            maximum.unwrap_or(prompts::DEFAULT_MAX_FILE_BYTES)
        };
        ensure!(
            limit == 0 || file.size_bytes <= limit,
            "The following file exceeds the {limit}-byte limit:\n- {} ({} bytes)",
            file.display_path,
            file.size_bytes
        );
        if !raw {
            file.content = String::from_utf8_lossy(&fs::read(&file.path)?).into_owned();
        }
    }
    Ok(files)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub path: PathBuf,
    pub display_path: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated_bundle: bool,
}
impl Attachment {
    fn file(file: &PromptFile) -> Self {
        Self {
            path: file.path.clone(),
            display_path: file.display_path.clone(),
            size_bytes: if raw_upload(&file.path) {
                file.size_bytes
            } else {
                file.content.len() as u64
            },
            generated_bundle: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BundleFormat {
    Text,
    Zip,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BundleScope {
    TextOnly,
    All,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleMetadata {
    pub original_count: usize,
    pub bundle_path: PathBuf,
    pub format: BundleFormat,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PendingBundle {
    pub format: BundleFormat,
    pub scope: BundleScope,
}
pub struct BrowserPrompt {
    pub markdown: String,
    pub composer_text: String,
    pub estimated_input_tokens: usize,
    pub attachments: Vec<Attachment>,
    pub inline_file_count: usize,
    pub token_estimate_includes_inline_files: bool,
    pub attachments_policy: String,
    pub attachment_mode: String,
    pub bundled: Option<BundleMetadata>,
    bundles: Vec<OwnedBundle>,
    text_files: Vec<PromptFile>,
    raw_files: Vec<PromptFile>,
}
struct OwnedBundle {
    path: PathBuf,
    parent: NativeEntry,
    directory: NativeEntry,
    file: NativeEntry,
    filename: String,
    cleaned: bool,
}
impl OwnedBundle {
    fn create(parent: &Path, name: &str, bytes: &[u8]) -> Result<Self> {
        let parent_entry = NativeEntry::open(parent, true)?;
        let path = parent.join(format!("oracle-browser-bundle-{}", uuid::Uuid::new_v4()));
        #[cfg_attr(windows, allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        let directory = NativeEntry::open(&path, true)?;
        parent_entry.require_matches(parent)?;
        let file = match private_file(&path.join(name), true)
            .and_then(|file| NativeEntry::from_handle(file, false))
        {
            Ok(file) => file,
            Err(error) => {
                let _ = remove_owned_directory(&path, &directory, &[]);
                return Err(error);
            }
        };
        let mut bundle = Self {
            path,
            parent: parent_entry,
            directory,
            file,
            filename: name.into(),
            cleaned: false,
        };
        bundle.file.handle.write_all(bytes)?;
        bundle.file.handle.sync_all()?;
        bundle.directory.require_matches(&bundle.path)?;
        bundle.file.require_matches(&bundle.path.join(name))?;
        ensure!(
            bundle.file.handle.metadata()?.len() == bytes.len() as u64,
            "Generated bundle size changed"
        );
        Ok(bundle)
    }
    fn cleanup(&mut self) -> Result<()> {
        if !self.cleaned {
            self.parent
                .require_matches(self.path.parent().context("Bundle has no parent")?)?;
            remove_owned_directory(&self.path, &self.directory, &[(&self.filename, &self.file)])?;
            self.cleaned = true;
        }
        Ok(())
    }
}
impl Drop for OwnedBundle {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
fn scope(format: BundleFormat, requested: bool, text: usize, raw: usize) -> Option<BundleScope> {
    if text + raw == 0 {
        None
    } else if format == BundleFormat::Text {
        (text > 0 && (requested || text > 1 || text + raw > 10)).then_some(BundleScope::TextOnly)
    } else if requested {
        Some(BundleScope::All)
    } else if text > 1 {
        Some(if raw < 10 {
            BundleScope::TextOnly
        } else {
            BundleScope::All
        })
    } else {
        (text + raw > 10).then_some(BundleScope::All)
    }
}
fn unique_basenames(attachments: &[Attachment]) -> Result<()> {
    let mut names = std::collections::BTreeMap::<String, Vec<&Attachment>>::new();
    for attachment in attachments {
        let name = attachment
            .path
            .file_name()
            .context("Attachment has no basename")?;
        names
            .entry(
                name.to_string_lossy()
                    .to_lowercase()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .or_default()
            .push(attachment);
    }
    let mut collisions: Vec<_> = names
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .collect();
    if collisions.is_empty() {
        return Ok(());
    }
    collisions.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
    for (_, files) in &mut collisions {
        files.sort_by(|left, right| {
            left.display_path
                .encode_utf16()
                .cmp(right.display_path.encode_utf16())
                .then_with(|| {
                    left.path
                        .as_os_str()
                        .to_string_lossy()
                        .encode_utf16()
                        .cmp(right.path.as_os_str().to_string_lossy().encode_utf16())
                })
        });
    }
    // Folded names collide here too; name each group by its first file's original basename.
    let shown = |files: &[&Attachment]| {
        files[0]
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut lines = vec![if collisions.len() == 1 {
        format!(
            "Browser upload cannot safely include multiple files named \"{}\":",
            shown(&collisions[0].1)
        )
    } else {
        "Browser upload cannot safely include files with colliding basenames:".into()
    }];
    let multiple = collisions.len() > 1;
    for (_, files) in &collisions {
        if multiple {
            lines.push(format!("Files named \"{}\":", shown(files)));
        }
        lines.extend(
            files
                .iter()
                .map(|file| format!("- {}", file.path.display())),
        );
    }
    lines.push("Use --browser-bundle-files (and --browser-bundle-format zip for raw or binary files), or choose unique names before retrying.".into());
    anyhow::bail!("{}", lines.join("\n"));
}
fn count(attachments: usize, format: BundleFormat) -> Result<()> {
    ensure!(
        attachments <= 10,
        "Browser upload has {attachments} attachments after applying bundle format \"{}\". Use --browser-bundle-format auto or zip to stay within the 10-attachment limit.",
        if format == BundleFormat::Zip {
            "zip"
        } else {
            "text"
        }
    );
    Ok(())
}
fn zip_instruction(prompt: &str, count: usize, path: &Path) -> String {
    let instruction = format!(
        "The attached `{}` contains {count} selected {} with relative paths preserved. Extract it into a temporary directory, then inspect the resulting file tree with filesystem and search tools before answering.",
        path.file_name().unwrap_or_default().to_string_lossy(),
        if count == 1 { "file" } else { "files" }
    );
    if policy::trim(prompt).is_empty() {
        instruction
    } else {
        format!("{}\n\n{instruction}", policy::trim(prompt))
    }
}
impl BrowserPrompt {
    pub fn assemble(
        files: &[PromptFile],
        prompt: &str,
        system: Option<&str>,
        options: &Value,
        parent: &Path,
    ) -> Result<Self> {
        let text_files: Vec<_> = files
            .iter()
            .filter(|file| !raw_upload(&file.path))
            .cloned()
            .collect();
        let raw_files: Vec<_> = files
            .iter()
            .filter(|file| raw_upload(&file.path))
            .cloned()
            .collect();
        let policy = options["browserAttachments"].as_str().unwrap_or("auto");
        ensure!(
            matches!(policy, "auto" | "never" | "always"),
            "Unknown browser attachment policy"
        );
        // Pasted file text cannot be verified as sent, so every file is uploaded.
        ensure!(
            policy != "never" || files.is_empty(),
            "Inline file delivery cannot preserve verified ChatGPT source. Use --browser-attachments auto or always."
        );
        let system = system.map(policy::trim).unwrap_or("");
        let prompt = policy::trim(prompt);
        let base = [system, prompt]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let requested = options["browserBundleFiles"] == true;
        let format = match options["browserBundleFormat"].as_str().unwrap_or("auto") {
            "auto" if raw_files.is_empty() => BundleFormat::Text,
            "auto" | "zip" => BundleFormat::Zip,
            "text" => BundleFormat::Text,
            _ => anyhow::bail!("Browser bundle format must be auto, text or zip."),
        };
        let selected_scope = scope(format, requested, text_files.len(), raw_files.len());
        let attachments = text_files
            .iter()
            .chain(&raw_files)
            .map(Attachment::file)
            .collect();
        let mut plan = Self {
            markdown: prompts::render(system, prompt, &text_files),
            composer_text: base,
            estimated_input_tokens: 0,
            attachments,
            inline_file_count: 0,
            token_estimate_includes_inline_files: false,
            attachments_policy: policy.into(),
            attachment_mode: if selected_scope.is_some() {
                "bundle"
            } else if files.is_empty() {
                "inline"
            } else {
                "upload"
            }
            .into(),
            bundled: None,
            bundles: vec![],
            text_files,
            raw_files,
        };
        let mut attachment_text = prompts::file_sections(&plan.text_files, false, false);
        if let Some(scope) = selected_scope {
            let (attachments, bundled, composer, text, bundle) =
                plan.bundle(&plan.composer_text, format, scope, parent)?;
            plan.attachments = attachments;
            plan.bundled = Some(bundled);
            plan.composer_text = composer;
            attachment_text = text;
            plan.bundles.push(bundle);
        }
        count(plan.attachments.len(), format)?;
        unique_basenames(&plan.attachments)?;
        let mut messages = vec![];
        if !system.is_empty() {
            messages.push(("system", system));
        }
        let mut user = prompt.to_owned();
        if let Some(bundle) = &plan.bundled
            && bundle.format == BundleFormat::Zip
        {
            user = zip_instruction(&user, bundle.original_count, &bundle.bundle_path);
        }
        if !user.is_empty() {
            messages.push(("user", user.as_str()));
        }
        if messages.is_empty() {
            messages.push(("user", ""));
        }
        plan.estimated_input_tokens = tokenizer::count_chat(&messages)?;
        if !plan.token_estimate_includes_inline_files && !plan.text_files.is_empty() {
            plan.estimated_input_tokens +=
                tokenizer::count_chat(&[("user", attachment_text.as_str())])?;
        }
        Ok(plan)
    }
    fn bundle(
        &self,
        prompt: &str,
        format: BundleFormat,
        scope: BundleScope,
        parent: &Path,
    ) -> Result<(Vec<Attachment>, BundleMetadata, String, String, OwnedBundle)> {
        let native: Vec<_> = if scope == BundleScope::TextOnly {
            self.raw_files.iter().map(Attachment::file).collect()
        } else {
            vec![]
        };
        count(1 + native.len(), format)?;
        unique_basenames(&native)?;
        let reserved: BTreeSet<_> = native
            .iter()
            .filter_map(|file| file.path.file_name())
            .map(|name| name.to_string_lossy().to_lowercase())
            .collect();
        let extension = if format == BundleFormat::Zip {
            "zip"
        } else {
            "txt"
        };
        let mut name = format!("attachments-bundle.{extension}");
        let mut suffix = 2;
        while reserved.contains(&name.to_lowercase()) {
            name = format!("attachments-bundle-{suffix}.{extension}");
            suffix += 1;
        }
        if format == BundleFormat::Text {
            // A text bundle carries decoded text, so bytes that are not UTF-8 would be garbled.
            let mut undecodable = vec![];
            for file in &self.text_files {
                if prompts::read_text(&file.path)?.is_none() {
                    undecodable.push(file.display_path.as_str());
                }
            }
            ensure!(
                undecodable.is_empty(),
                "A text bundle cannot carry files that are not UTF-8 text:\n- {}\nUse --browser-bundle-format zip to send their original bytes.",
                undecodable.join("\n- ")
            );
        }
        let text = prompts::file_sections(&self.text_files, false, format == BundleFormat::Text);
        let text = if text.is_empty() {
            text
        } else {
            format!("{text}\n")
        };
        let sources: Vec<_> = self
            .text_files
            .iter()
            .chain(if scope == BundleScope::All {
                self.raw_files.iter()
            } else {
                [].iter()
            })
            .collect();
        let bytes = if format == BundleFormat::Text {
            text.as_bytes().to_vec()
        } else {
            let size = sources.iter().try_fold(0u64, |total, file| {
                total
                    .checked_add(if raw_upload(&file.path) {
                        file.size_bytes
                    } else {
                        file.content.len() as u64
                    })
                    .context("Browser ZIP bundle size overflow")
            })?;
            ensure!(
                size <= 128 * 1024 * 1024,
                "Browser ZIP bundle inputs exceed the 134217728-byte in-memory limit."
            );
            let mut contents = vec![];
            for source in &sources {
                let mut file = NativeEntry::open(&source.path, false)?;
                let mut bytes = vec![];
                Read::by_ref(&mut file.handle)
                    .take(128 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                file.require_matches(&source.path)?;
                ensure!(
                    bytes.len() as u64 == source.size_bytes,
                    "Browser bundle source size changed"
                );
                contents.push(bytes);
            }
            let entries: Vec<_> = sources
                .iter()
                .zip(&contents)
                .map(|(file, bytes)| ZipEntry {
                    path: &file.display_path,
                    bytes,
                })
                .collect();
            stored_zip(&entries)?
        };
        let bundle = OwnedBundle::create(parent, &name, &bytes)?;
        let path = bundle.path.join(&name);
        let mut attachments = vec![Attachment {
            path: path.clone(),
            display_path: path.to_string_lossy().into_owned(),
            size_bytes: bytes.len() as u64,
            generated_bundle: true,
        }];
        attachments.extend(native);
        let count = if format == BundleFormat::Zip {
            sources.len()
        } else {
            self.text_files.len()
        };
        Ok((
            attachments,
            BundleMetadata {
                original_count: count,
                bundle_path: path.clone(),
                format,
            },
            if format == BundleFormat::Zip {
                zip_instruction(prompt, count, &path)
            } else {
                prompt.into()
            },
            text,
            bundle,
        ))
    }
    pub(crate) fn remote_fallback(
        files: &[PromptFile],
        composer: &str,
        pending: Option<PendingBundle>,
        parent: &Path,
    ) -> Result<Self> {
        let mut plan = Self::assemble(
            files,
            composer,
            None,
            &serde_json::json!({"browserAttachments":"always","browserBundleFiles":false}),
            parent,
        )?;
        plan.composer_text = composer.into();
        if let Some(pending) = pending {
            let (attachments, bundled, composer, _, bundle) =
                plan.bundle(composer, pending.format, pending.scope, parent)?;
            plan.attachments = attachments;
            plan.bundled = Some(bundled);
            plan.composer_text = composer;
            plan.bundles.push(bundle);
        }
        Ok(plan)
    }
    pub fn cleanup(&mut self) -> Result<()> {
        for bundle in &mut self.bundles {
            bundle.cleanup()?;
        }
        Ok(())
    }
    pub fn preview_lines(
        &self,
        config: &Value,
        model: &str,
        mode: &str,
        followups: &[Value],
    ) -> Result<Vec<String>> {
        let followup_count = followups
            .iter()
            .filter(|value| {
                value
                    .as_str()
                    .is_some_and(|text| !policy::trim(text).is_empty())
            })
            .count();
        ensure!(
            followup_count == 0 || config["researchMode"] != "deep",
            "Browser follow-ups are not supported with Deep Research mode. Put the full research plan into the initial prompt or run a normal browser consult for multi-turn review."
        );
        let strategy = config["modelStrategy"].as_str().unwrap_or("select");
        let target = if matches!(strategy, "current" | "ignore") {
            format!("picker={strategy}; requested={model}")
        } else if let Some(desired) = config["desiredModel"]
            .as_str()
            .filter(|label| !policy::trim(label).is_empty())
        {
            format!("target={}; requested={model}", policy::trim(desired))
        } else {
            model.into()
        };
        let suffix = if self.token_estimate_includes_inline_files && self.inline_file_count > 0 {
            format!(
                " (includes {} inline file{})",
                self.inline_file_count,
                if self.inline_file_count == 1 { "" } else { "s" }
            )
        } else if !self.attachments.is_empty() {
            format!(
                " (prompt only; {} attachment{} excluded)",
                self.attachments.len(),
                if self.attachments.len() == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let mut lines = vec![format!(
            "[preview] Alcuin ({}) browser mode ({target}) with ~{} tokens{suffix}.",
            env!("CARGO_PKG_VERSION"),
            grouped(self.estimated_input_tokens)
        )];
        lines.extend(control_plan_lines(config, "preview"));
        if followup_count > 0 {
            lines.push(format!(
                "[preview] Browser follow-ups: {followup_count} additional prompt(s)."
            ));
            lines.push("[preview] Multi-turn is explicit only: Alcuin will send these prompts in order, but it never invents follow-ups automatically.".into());
        }
        let cookies = if let Some(cookies) = config["inlineCookies"]
            .as_array()
            .filter(|cookies| !cookies.is_empty())
        {
            format!(
                "Cookies: inline payload ({}) via {}.",
                cookies.len(),
                config["inlineCookiesSource"].as_str().unwrap_or("inline")
            )
        } else if config["cookieSync"] != true {
            "Cookies: Chrome copy disabled (use --browser-manual-login, inline cookies, or --browser-cookie-sync).".into()
        } else {
            format!(
                "Cookies: copy from Chrome ({}).",
                config["cookieNames"]
                    .as_array()
                    .filter(|names| !names.is_empty())
                    .map(|names| names
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_else(|| "all from Chrome profile".into())
            )
        };
        lines.push(format!("[preview] {cookies}"));
        if !self.attachments.is_empty() {
            lines.push(
                if self.bundled.is_some() {
                    "[preview] Bundled upload:"
                } else {
                    "[preview] Attachments to upload:"
                }
                .into(),
            );
            for file in &self.attachments {
                let size = if file.size_bytes < 1024 {
                    format!("{} B", file.size_bytes)
                } else if file.size_bytes < 1024 * 1024 {
                    format!("{:.1} KB", file.size_bytes as f64 / 1024.0)
                } else {
                    format!("{:.1} MB", file.size_bytes as f64 / (1024.0 * 1024.0))
                };
                lines.push(format!("  • {} ({size})", file.display_path));
            }
            if let Some(bundle) = &self.bundled {
                lines.push(format!(
                    "  (bundled {} files into {})",
                    bundle.original_count,
                    bundle.bundle_path.display()
                ));
            }
        } else if self.inline_file_count > 0 {
            lines.push("[preview] Inline file content:".into());
            lines.push(format!(
                "  • {} file{} pasted directly into the composer.",
                self.inline_file_count,
                if self.inline_file_count == 1 { "" } else { "s" }
            ));
        } else {
            lines.push("[preview] No files attached.".into());
        }
        // Reference runBrowserDryRun: the same plan under the dry-run label and header.
        if mode == "dry-run" {
            lines[0] = format!(
                "[dry-run] Alcuin ({}) would launch browser mode ({target}) with ~{} tokens{suffix}.",
                env!("CARGO_PKG_VERSION"),
                grouped(self.estimated_input_tokens)
            );
            for line in lines.iter_mut().skip(1) {
                if let Some(rest) = line.strip_prefix("[preview] ") {
                    *line = format!("[dry-run] {rest}");
                }
            }
        }
        if matches!(mode, "json" | "full") {
            lines.extend([String::new(),"Preview JSON".into(),serde_json::to_string_pretty(&serde_json::json!({"model":model,"engine":"browser","composerText":self.composer_text,"attachments":self.attachments.iter().map(|file|serde_json::json!({"path":file.path,"displayPath":file.display_path,"sizeBytes":file.size_bytes})).collect::<Vec<_>>(),"inlineFileCount":self.inline_file_count,"bundled":self.bundled,"tokenEstimate":self.estimated_input_tokens,"browserFollowUps":followups}))?]);
        }
        if mode == "full" {
            lines.extend([
                String::new(),
                "Composer Text".into(),
                if self.composer_text.is_empty() {
                    "(empty prompt)".into()
                } else {
                    self.composer_text.clone()
                },
            ]);
        }
        Ok(lines)
    }
}
fn grouped(value: usize) -> String {
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

#[cfg(test)]
mod tests {
    #[test]
    fn remote_chrome_objects_use_the_remote_control_plan() {
        let lines = control_plan_lines(
            &serde_json::json!({"remoteChrome":{"host":"127.0.0.1","port":9222}}),
            "preview",
        );
        assert_eq!(
            lines[0],
            "[preview] Browser control: reuse an existing remote Chrome session; does not use a visible local browser window."
        );
    }
    use super::*;
    use serde_json::json;
    #[test]
    fn full_prompt_and_fallback_corpus_matches_the_pinned_reference() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        fn attachments(files: &[Attachment]) -> Value {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            json!(files.iter().map(|file| {
                let name=file.path.file_name().unwrap().to_string_lossy();
                json!({"name":name,"displayPath":if file.generated_bundle{name.to_string()}else{file.display_path.replace('\\',"/")},"sizeBytes":file.size_bytes,"generatedBundle":file.generated_bundle,"bytesBase64":STANDARD.encode(fs::read(&file.path).unwrap())})
            }).collect::<Vec<_>>())
        }
        fn bundled(bundle: Option<&BundleMetadata>) -> Value {
            bundle.map(|bundle|json!({"originalCount":bundle.original_count,"format":bundle.format,"name":bundle.bundle_path.file_name().unwrap().to_string_lossy()})).unwrap_or(Value::Null)
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/browser-prompt-reference.json"))
                .unwrap();
        assert_eq!(
            fixture["sourceRevision"],
            "0c1ac91b72761b1cca91bc3a31cbbd12b4732a33"
        );
        let mut differences = Vec::new();
        for case in fixture["cases"].as_array().unwrap() {
            let root = tempfile::tempdir().unwrap();
            let mut inputs = vec![];
            for file in case["files"].as_array().unwrap() {
                let name = file["path"].as_str().unwrap();
                let path = root.path().join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(
                    path,
                    STANDARD
                        .decode(file["contentBase64"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
                inputs.push(name.to_owned());
            }
            let options = &case["options"];
            let result = discover(&inputs, root.path(), options["maxFileSizeBytes"].as_u64())
                .and_then(|files| {
                    BrowserPrompt::assemble(
                        &files,
                        options["prompt"].as_str().unwrap(),
                        options["system"].as_str(),
                        options,
                        root.path(),
                    )
                });
            let mut plan = match result {
                Ok(plan) => plan,
                Err(error) => {
                    let observed = error
                        .to_string()
                        .replace(root.path().to_str().unwrap(), "<ROOT>")
                        .replace('\\', "/");
                    if case["error"].as_str() != Some(observed.as_str()) {
                        differences.push(format!(
                            "{}: error {observed}; expected {}",
                            case["name"], case["error"]
                        ));
                    }
                    continue;
                }
            };
            let actual = json!({"markdown":plan.markdown,"composerText":plan.composer_text,"estimatedInputTokens":plan.estimated_input_tokens,"attachments":attachments(&plan.attachments),"inlineFileCount":plan.inline_file_count,"tokenEstimateIncludesInlineFiles":plan.token_estimate_includes_inline_files,"attachmentsPolicy":plan.attachments_policy,"attachmentMode":plan.attachment_mode,"bundled":bundled(plan.bundled.as_ref()),"fallback":null,"materializedFallback":null});
            if actual != case["expected"] {
                for (key, value) in actual.as_object().unwrap() {
                    if *value != case["expected"][key] {
                        differences.push(format!(
                            "{}: {key}: candidate={value}; expected={}",
                            case["name"], case["expected"][key]
                        ));
                    }
                }
            }
            plan.cleanup().unwrap();
        }
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }
    fn file(root: &Path, name: &str, bytes: &[u8]) -> PromptFile {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        PromptFile {
            path,
            display_path: name.into(),
            content: if raw_upload(Path::new(name)) {
                String::new()
            } else {
                String::from_utf8_lossy(bytes).into_owned()
            },
            size_bytes: bytes.len() as u64,
        }
    }
    #[test]
    fn multiple_text_uploads_share_one_flattened_bundle_with_source_sections() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            file(root.path(), "a.rs", b"PUBLIC one\n"),
            file(root.path(), "b.rs", b"PUBLIC two\n"),
        ];
        let mut prompt = BrowserPrompt::assemble(
            &files,
            "PUBLIC bundle request",
            None,
            &json!({"browserAttachments":"always"}),
            root.path(),
        )
        .unwrap();
        assert_eq!(prompt.composer_text, "PUBLIC bundle request");
        assert_eq!(prompt.attachment_mode, "bundle");
        assert_eq!(prompt.inline_file_count, 0);
        assert_eq!(prompt.attachments.len(), 1);
        let bundle = prompt.bundled.as_ref().unwrap();
        assert_eq!(bundle.format, BundleFormat::Text);
        assert_eq!(bundle.original_count, 2);
        assert_eq!(
            fs::read_to_string(&bundle.bundle_path).unwrap(),
            "### File: a.rs\nLines: 1-1\n```rust\n1 | PUBLIC one\n```\n\n### File: b.rs\nLines: 1-1\n```rust\n1 | PUBLIC two\n```\n"
        );
        let directory = bundle.bundle_path.parent().unwrap().to_owned();
        prompt.cleanup().unwrap();
        assert!(!directory.exists());
    }
    #[test]
    fn text_and_raw_files_upload_and_disabled_uploads_refuse_any_file() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            file(root.path(), "public.rs", b"PUBLIC text"),
            file(
                root.path(),
                "PUBLIC.png",
                b"\x89PNG\r\n\x1a\nPUBLIC synthetic",
            ),
        ];
        let prompt =
            BrowserPrompt::assemble(&files, "PUBLIC raw request", None, &json!({}), root.path())
                .unwrap();
        assert_eq!(prompt.inline_file_count, 0);
        assert_eq!(prompt.attachments.len(), 2);
        assert_eq!(prompt.attachments[1].display_path, "PUBLIC.png");
        assert!(!prompt.markdown.contains("PUBLIC.png"));
        assert_eq!(prompt.composer_text, "PUBLIC raw request");
        assert_eq!(prompt.attachment_mode, "upload");
        for files in [&files[..1], &files[..]] {
            let error = BrowserPrompt::assemble(
                files,
                "PUBLIC",
                None,
                &json!({"browserAttachments":"never"}),
                root.path(),
            )
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .starts_with("Inline file delivery cannot preserve")
            );
        }
    }
    #[test]
    fn zip_bundles_reduce_raw_attachment_count_and_preserve_file_bytes() {
        let root = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..11)
            .map(|index| {
                file(
                    root.path(),
                    &format!("public-{index}.png"),
                    b"PUBLIC raw byte fixture",
                )
            })
            .collect();
        let mut prompt =
            BrowserPrompt::assemble(&files, "PUBLIC zip request", None, &json!({}), root.path())
                .unwrap();
        assert_eq!(prompt.attachments.len(), 1);
        let bundle = prompt.bundled.as_ref().unwrap();
        assert_eq!(bundle.format, BundleFormat::Zip);
        assert_eq!(bundle.original_count, 11);
        assert!(prompt.composer_text.contains("contains 11 selected files"));
        let bytes = fs::read(&bundle.bundle_path).unwrap();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 11);
        prompt.cleanup().unwrap();
    }
    #[test]
    fn unknown_bundle_contents_are_preserved_and_raw_basename_collisions_refuse() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            file(root.path(), "a.rs", b"PUBLIC a"),
            file(root.path(), "b.rs", b"PUBLIC b"),
        ];
        let mut prompt = BrowserPrompt::assemble(
            &files,
            "PUBLIC cleanup",
            None,
            &json!({"browserAttachments":"always"}),
            root.path(),
        )
        .unwrap();
        let directory = prompt.attachments[0].path.parent().unwrap().to_owned();
        fs::write(directory.join("unrecognised"), b"PUBLIC preserve").unwrap();
        assert!(prompt.cleanup().is_err());
        assert!(prompt.attachments[0].path.exists());
        assert_eq!(
            fs::read(directory.join("unrecognised")).unwrap(),
            b"PUBLIC preserve"
        );
        let duplicates = [
            file(root.path(), "a/public.png", b"PUBLIC a"),
            file(root.path(), "b/public.png", b"PUBLIC b"),
        ];
        assert!(
            BrowserPrompt::assemble(
                &duplicates,
                "PUBLIC collision",
                None,
                &json!({}),
                root.path()
            )
            .is_err()
        );
    }
    #[test]
    fn renderer_normalised_basename_collisions_refuse_before_transfer() {
        let root = tempfile::tempdir().unwrap();
        for (left, right) in [
            ("PUBLIC  image.png", "PUBLIC image.png"),
            ("PUBLIC.png", "public.PNG"),
        ] {
            let attachments: Vec<_> = [
                file(root.path(), &format!("left/{left}"), b"PUBLIC left"),
                file(root.path(), &format!("right/{right}"), b"PUBLIC right"),
            ]
            .iter()
            .map(Attachment::file)
            .collect();
            assert!(
                unique_basenames(&attachments).is_err(),
                "Renderer-equivalent names admitted: {left}, {right}"
            );
        }
    }
    #[test]
    fn folded_basename_collisions_name_the_first_original_file() {
        let root = tempfile::tempdir().unwrap();
        let attachments: Vec<_> = [
            file(root.path(), "right/public.PNG", b"PUBLIC right"),
            file(root.path(), "left/PUBLIC.png", b"PUBLIC left"),
        ]
        .iter()
        .map(Attachment::file)
        .collect();
        let error = unique_basenames(&attachments).unwrap_err().to_string();
        assert!(
            error.starts_with(
                "Browser upload cannot safely include multiple files named \"PUBLIC.png\":"
            ),
            "{error}"
        );
    }
}
