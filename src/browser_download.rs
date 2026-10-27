//! Browser downloads belong to one armed native target/frame and exact Chrome GUIDs.
//! Reference: downloadOwnership.ts, chatgptFiles.ts, chatgptImages.ts, requiredImageDelivery.ts.
use crate::{
    artifacts::{publish, sha256},
    browser_chrome::AttachmentEndpoint,
    browser_target::OwnedTarget,
    cdp::Cdp,
    native_fs::{NativeEntry, remove_owned_directory, remove_owned_file},
    sessions::timestamp,
    storage::{DirectoryLock, private_dir, private_file},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
pub struct DownloadOwner {
    browser: Cdp,
    events: broadcast::Receiver<Value>,
    frame: String,
    target: String,
    context: Value,
    directory: PathBuf,
    stage: NativeEntry,
    marker_path: PathBuf,
    marker: NativeEntry,
    parents: Vec<(PathBuf, NativeEntry)>,
    lock: Option<DirectoryLock>,
    rows: BTreeMap<String, Value>,
    files: Vec<(String, NativeEntry)>,
    published: BTreeSet<String>,
    configured: bool,
    pending: bool,
    invalid: bool,
}
impl DownloadOwner {
    pub async fn begin(
        target: &OwnedTarget,
        endpoint: &AttachmentEndpoint,
        home: &Path,
        artifacts: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        endpoint.validate()?;
        let artifacts = &chrome_directory(artifacts)?;
        let info = target.require(timeout, cancel).await?;
        let tree = target
            .page()
            .call("Page.getFrameTree", json!({}), timeout, cancel)
            .await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .context("Download ownership has no exact page frame")?
            .to_owned();
        let lock = DirectoryLock::acquire(
            &home.join(".calvino-browser-download-mutex"),
            Duration::from_secs(10),
        )?;
        let parent = home.join(".calvino");
        let owners = parent.join("download-owners");
        let mut parents = vec![];
        for path in [&parent, &owners, artifacts] {
            private_dir(path)?;
            parents.push((path.to_owned(), NativeEntry::open(path, true)?));
        }
        let context = info.get("browserContextId").cloned().unwrap_or(Value::Null);
        let marker_path = owners.join(format!(
            "{}.json",
            sha256(
                format!(
                    "{}:{}",
                    endpoint.browser_ws_endpoint,
                    context.as_str().unwrap_or("default")
                )
                .as_bytes()
            )
        ));
        let marker_handle = private_file(&marker_path, true)
            .context("Browser download ownership is unresolved; refusing reconfiguration")?;
        let mut marker = NativeEntry::from_handle(marker_handle, false)?;
        let record = json!({"version":1,"nonce":Uuid::new_v4().to_string(),"controllerPid":std::process::id(),"processIdentity":crate::process::ProcessIdentity::current()?,"browser":sha256(endpoint.browser_ws_endpoint.as_bytes()),"context":context,"frame":frame,"target":target.id(),"createdAt":timestamp()});
        marker.handle.write_all(&serde_json::to_vec(&record)?)?;
        marker.handle.sync_all()?;
        let directory = artifacts.join(format!(".calvino-browser-download-{}", Uuid::new_v4()));
        private_dir(&directory)?;
        let stage = NativeEntry::open(&directory, true)?;
        let browser = target.browser().clone();
        let events = browser.events();
        let mut owner = Self {
            browser,
            events,
            frame,
            target: target.id().into(),
            context,
            directory,
            stage,
            marker_path,
            marker,
            parents,
            lock: Some(lock),
            rows: BTreeMap::new(),
            files: vec![],
            published: BTreeSet::new(),
            configured: false,
            pending: true,
            invalid: false,
        };
        owner.check()?;
        let params = owner.params(
            json!({"behavior":"allowAndName","downloadPath":owner.directory,"eventsEnabled":true}),
        );
        owner
            .browser
            .call("Browser.setDownloadBehavior", params, timeout, cancel)
            .await?;
        owner.configured = true;
        owner.pending = false;
        Ok(owner)
    }
    fn params(&self, mut value: Value) -> Value {
        if !self.context.is_null() {
            value["browserContextId"] = self.context.clone();
        }
        value
    }
    fn check(&self) -> Result<()> {
        self.stage.require_matches(&self.directory)?;
        self.marker.require_matches(&self.marker_path)?;
        for (path, entry) in &self.parents {
            entry.require_matches(path)?;
        }
        Ok(())
    }
    fn event(&mut self, event: Value, armed: bool) {
        match event["method"].as_str() {
            Some("Browser.downloadWillBegin")
                if armed && event["params"]["frameId"] == self.frame =>
            {
                let row = &event["params"];
                let Some(guid) = row["guid"]
                    .as_str()
                    .filter(|s| Uuid::parse_str(s).is_ok() && s.len() == 36)
                else {
                    self.invalid = true;
                    return;
                };
                if self.rows.contains_key(guid) {
                    self.invalid = true;
                    return;
                }
                self.rows.insert(guid.into(),json!({"guid":guid,"suggestedFilename":row["suggestedFilename"],"state":"inProgress","frameId":self.frame,"targetId":self.target}));
            }
            Some("Browser.downloadProgress") => {
                if let Some(guid) = event["params"]["guid"].as_str()
                    && let Some(row) = self.rows.get_mut(guid)
                    && matches!(
                        event["params"]["state"].as_str(),
                        Some("completed" | "canceled")
                    )
                {
                    row["state"] = event["params"]["state"].clone();
                }
            }
            _ => {}
        }
    }
    pub async fn capture(
        &mut self,
        page: &Cdp,
        function: &str,
        cfg: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(Vec<u8>, Value)> {
        self.check()?;
        anyhow::ensure!(
            self.configured && !self.pending && !self.invalid,
            "Browser download ownership is uncertain; no duplicate click is permitted"
        );
        // Discard earlier notifications before arming. No directory entry can establish ownership.
        loop {
            match self.events.try_recv() {
                Ok(event) => self.event(event, false),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(_) => bail!("Browser download event history is incomplete"),
            }
        }
        let before: BTreeSet<_> = self.rows.keys().cloned().collect();
        self.pending = true;
        let clicked = page
            .evaluate_with_arguments(function, &json!([cfg]), timeout, cancel)
            .await?;
        anyhow::ensure!(
            clicked["clicked"].as_u64() == Some(1),
            "Exact download click was not positively acknowledged; ownership retained"
        );
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            loop {
                match self.events.try_recv() {
                    Ok(event) => self.event(event, true),
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(_) => {
                        bail!("Owned browser download event history was lost; context retained")
                    }
                }
            }
            anyhow::ensure!(
                !self.invalid,
                "Owned download GUID is duplicated or malformed; context quarantined"
            );
            let added: Vec<_> = self
                .rows
                .iter()
                .filter(|(guid, _)| !before.contains(*guid))
                .collect();
            anyhow::ensure!(
                added.len() <= 1,
                "A single owned click produced ambiguous downloads; context quarantined"
            );
            if let [(guid, row)] = added.as_slice() {
                if row["state"] == "canceled" {
                    bail!("Owned browser download was cancelled; result retained");
                }
                if row["state"] == "completed" {
                    let guid = (*guid).clone();
                    let row = (*row).clone();
                    let path = self.directory.join(&guid);
                    let mut entry = NativeEntry::open(&path, false)?;
                    let size = entry.handle.metadata()?.len();
                    anyhow::ensure!(
                        size > 0 && size <= MAX_DOWNLOAD_BYTES,
                        "Owned download is empty or exceeds 128 MiB"
                    );
                    let mut bytes = vec![];
                    Read::by_ref(&mut entry.handle)
                        .take(MAX_DOWNLOAD_BYTES + 1)
                        .read_to_end(&mut bytes)?;
                    entry.handle.seek(SeekFrom::Start(0))?;
                    let mut verified = vec![];
                    Read::by_ref(&mut entry.handle)
                        .take(MAX_DOWNLOAD_BYTES + 1)
                        .read_to_end(&mut verified)?;
                    entry.require_matches(&path)?;
                    self.check()?;
                    anyhow::ensure!(
                        bytes == verified && bytes.len() as u64 == size,
                        "Owned download changed while capturing native bytes"
                    );
                    if let Some(stored) = self.rows.get_mut(&guid) {
                        stored["capturedSha256"] = json!(sha256(&bytes));
                        stored["capturedSize"] = json!(size);
                    }
                    self.files.push((guid, entry));
                    self.pending = false;
                    return Ok((bytes, row));
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "Owned download acknowledgement is uncertain; never replay the click automatically"
            );
            let event = tokio::select! {biased;_=cancel.cancelled()=>bail!("Owned download cancelled; context and staging retained"),event=tokio::time::timeout(remaining,self.events.recv())=>event.context("Owned download deadline expired; context retained")?.context("Owned download events were lost; context retained")?};
            self.event(event, true);
        }
    }
    pub async fn finish(mut self, timeout: Duration) -> Result<Option<String>> {
        let cancel = CancellationToken::new();
        self.check()?;
        let unsettled: Vec<_> = self
            .rows
            .iter()
            .filter(|(_, row)| row["state"] == "inProgress")
            .map(|(guid, _)| guid.clone())
            .collect();
        for guid in unsettled {
            let params = self.params(json!({"guid":guid}));
            let _ = self
                .browser
                .call("Browser.cancelDownload", params, timeout, &cancel)
                .await;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while self.rows.values().any(|row| row["state"] == "inProgress")
            && tokio::time::Instant::now() < deadline
        {
            if let Ok(Ok(event)) = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                self.events.recv(),
            )
            .await
            {
                self.event(event, false);
            } else {
                break;
            }
        }
        let params = self.params(json!({"behavior":"default","eventsEnabled":false}));
        if self.configured {
            self.browser
                .call("Browser.setDownloadBehavior", params, timeout, &cancel)
                .await?;
        }
        anyhow::ensure!(
            !self.pending
                && !self.invalid
                && self.rows.values().all(|row| row["state"] != "inProgress"),
            "Download context remains quarantined; exact marker and staging preserved"
        );
        self.check()?;
        remove_owned_file(&self.marker_path, &self.marker)?;
        self.lock
            .take()
            .context("Download mutex ownership was lost")?
            .release()?;
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|(name, _)| self.published.contains(name))
            .map(|(name, entry)| (name.as_str(), entry))
            .collect();
        let cleanup = remove_owned_directory(&self.directory, &self.stage, &files)
            .err()
            .map(|e| e.to_string());
        Ok(cleanup)
    }
    pub fn stage_path(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn publication_confirmed(&mut self, guid: &str, manifest: &Value) -> Result<()> {
        let row = self
            .rows
            .get(guid)
            .context("Publication refers to an unowned download GUID")?;
        anyhow::ensure!(
            row["capturedSha256"] == manifest["sha256"]
                && row["capturedSize"] == manifest["sizeBytes"],
            "Published artifact differs from captured owned download bytes"
        );
        self.published.insert(guid.into());
        Ok(())
    }
    pub(crate) fn discard_non_image(&mut self, guid: &str, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(
            image_type(bytes).is_none(),
            "Image bytes cannot be discarded as a non-image download"
        );
        self.publication_confirmed(
            guid,
            &json!({"sha256":sha256(bytes),"sizeBytes":bytes.len()}),
        )
    }
}
// Drop preserves markers, downloads and policy uncertainty. Only finish can retire exact evidence.

/// Chrome on Windows receives an owned download into `<guid>.crdownload` but fails the final
/// rename when the download directory has forward slashes or a verbatim prefix, and reports the
/// GUID as canceled. Path::join keeps the caller's separators (Node's path.join normalises them),
/// so the staging directory is made absolute and native before Chrome sees it.
fn chrome_directory(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut native = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => native.push(match prefix.kind() {
                    Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
                        format!("{}:", char::from(disk))
                    }
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                        r"\\{}\{}",
                        server
                            .to_str()
                            .context("Download share name is not Unicode")?,
                        share
                            .to_str()
                            .context("Download share name is not Unicode")?
                    ),
                    _ => bail!("Browser download directory has no drive or share form"),
                }),
                Component::CurDir | Component::ParentDir => {
                    bail!("Browser download directory is not normalised")
                }
                component => native.push(component),
            }
        }
        Ok(native)
    }
    #[cfg(not(windows))]
    Ok(path)
}
pub fn download_url(raw: &str) -> Result<reqwest::Url> {
    let mut url = if let Some(path) = raw.strip_prefix("sandbox:") {
        anyhow::ensure!(safe_sandbox(path), "Unsafe sandbox download path");
        let mut url = reqwest::Url::parse("https://chatgpt.com/backend-api/sandbox/download")?;
        url.query_pairs_mut().append_pair("path", path);
        url
    } else {
        reqwest::Url::parse(raw)?
    };
    anyhow::ensure!(
        url.scheme() == "https"
            && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "File/image download origin is unqualified"
    );
    let known = match url.path() {
        "/backend-api/sandbox/download" => url
            .query_pairs()
            .find(|(key, _)| key == "path")
            .is_some_and(|(_, path)| safe_sandbox(&path)),
        "/backend-api/estuary/content" => url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .is_some_and(|(_, id)| id.starts_with("file_")),
        path => {
            let segments: Vec<_> = path.trim_end_matches('/').split('/').collect();
            segments.len() == 5
                && segments[1] == "backend-api"
                && segments[2] == "files"
                && !segments[3].is_empty()
                && matches!(segments[4], "download" | "content")
        }
    };
    anyhow::ensure!(
        known,
        "File/image URL is not a maintained provider download route"
    );
    url.set_fragment(None);
    Ok(url)
}
fn safe_sandbox(path: &str) -> bool {
    path.starts_with("/mnt/data/")
        && !path.contains(['\\', '\0'])
        && !path.split('/').any(|part| part == "..")
        && !path.to_lowercase().contains("%2e")
        && !path.to_lowercase().contains("%5c")
}
pub fn image_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else {
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
        let text = text.trim_start();
        let text = if text.starts_with("<?xml") {
            text.split_once("?>")?.1.trim_start()
        } else {
            text
        };
        if text.to_lowercase().strip_prefix("<svg").is_some_and(|s| {
            s.starts_with('>') || s.chars().next().is_some_and(char::is_whitespace)
        }) {
            Some(("svg", "image/svg+xml"))
        } else {
            None
        }
    }
}
pub fn publish_download(
    bytes: &[u8],
    directory: &Path,
    name: &str,
    image: bool,
    cancel: &CancellationToken,
) -> Result<Value> {
    let detected = image_type(bytes);
    anyhow::ensure!(
        !image || detected.is_some(),
        "Required generated image has no verified image signature"
    );
    let mut name = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download.bin")
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .take(200)
        .collect::<String>();
    if name.trim_matches(['.', ' ']).is_empty() {
        name = "download.bin".into();
    }
    let path = PathBuf::from(&name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| detected.map_or("bin", |(ext, _)| ext));
    for index in 0..1000 {
        let filename = if index == 0 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem} ({index}).{extension}")
        };
        let destination = directory.join(&filename);
        if destination.symlink_metadata().is_ok() {
            continue;
        }
        let mime = detected.map(|(_, mime)| mime);
        let validation = if image {
            json!({"type":"image-signature","ok":true})
        } else {
            crate::remote_transfer::local_validation(bytes, &filename, mime)
        };
        let publication = publish(bytes, &destination, false, cancel)?;
        return Ok(
            json!({"kind":if image{"image"}else{"file"},"path":publication.path,"sizeBytes":publication.size_bytes,"sha256":publication.sha256,"nativeIdentity":publication.native_identity,"mimeType":mime,"validation":validation,"transfer":{"status":"not-needed"},"origin":{"mode":"local"},"cleanupWarning":publication.cleanup_error}),
        );
    }
    bail!("No unused artifact destination is available; existing output preserved")
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn chrome_download_directories_are_native_windows_paths() {
        for (path, native) in [
            (
                r"C:/PUBLIC/home\sessions\x/artifacts",
                r"C:\PUBLIC\home\sessions\x\artifacts",
            ),
            (r"\\?\C:\PUBLIC\artifacts", r"C:\PUBLIC\artifacts"),
            (r"\\?\UNC\server\share\PUBLIC", r"\\server\share\PUBLIC"),
            (r"//server/share/PUBLIC", r"\\server\share\PUBLIC"),
        ] {
            let converted = chrome_directory(Path::new(path)).unwrap();
            assert_eq!(converted.to_str(), Some(native), "{path}");
        }
        assert!(chrome_directory(Path::new(r"\\.\PUBLIC-device\artifacts")).is_err());
    }
    #[test]
    fn local_zip_downloads_record_the_reference_validation_result() {
        let home = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let truncated = publish_download(
            b"PK\x03\x04 PUBLIC truncated archive",
            home.path(),
            "Report.zip",
            false,
            &cancel,
        )
        .unwrap();
        assert_eq!(
            truncated["validation"],
            json!({"type":"zip","ok":false,"error":"zip-central-directory-missing"})
        );
        let mut empty = b"PK\x05\x06".to_vec();
        empty.extend([0; 18]);
        let archive = publish_download(&empty, home.path(), "Empty.zip", false, &cancel).unwrap();
        assert_eq!(archive["validation"], json!({"type":"zip","ok":true}));
        let text =
            publish_download(b"PUBLIC notes", home.path(), "notes.txt", false, &cancel).unwrap();
        assert_eq!(text["validation"], json!({"type":"generic","ok":true}));
    }
}
