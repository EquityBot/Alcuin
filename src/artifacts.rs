use crate::{
    native_fs::{NativeEntry, NativeIdentity, no_follow, read_stamp, remove_owned_directory},
    sessions::{SessionStore, assert_id, timestamp},
    storage::{private_dir, private_file},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureReceipt {
    pub version: u8,
    pub session_id: String,
    pub file: String,
    pub encoding: String,
    pub bytes: u64,
    pub sha256: String,
    pub saved_at: String,
}
fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("Industrious operation was cancelled.");
    }
    Ok(())
}
pub fn capture(
    store: &SessionStore,
    id: &str,
    file: &str,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<CaptureReceipt> {
    capture_with(store, id, file, bytes, cancel, || {})
}
fn capture_with(
    store: &SessionStore,
    id: &str,
    file: &str,
    bytes: &[u8],
    cancel: &CancellationToken,
    before_verify: impl FnOnce(),
) -> Result<CaptureReceipt> {
    capture_with_until(store, id, file, bytes, cancel, None, before_verify)
}
pub(crate) fn capture_until(
    store: &SessionStore,
    id: &str,
    file: &str,
    bytes: &[u8],
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<CaptureReceipt> {
    capture_with_until(store, id, file, bytes, cancel, deadline, || {})
}
fn capture_with_until(
    store: &SessionStore,
    id: &str,
    file: &str,
    bytes: &[u8],
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
    before_verify: impl FnOnce(),
) -> Result<CaptureReceipt> {
    check(cancel)?;
    check_deadline(deadline)?;
    assert_id(file)?;
    let original = store.directory(id)?;
    let root_entry = NativeEntry::open(&store.root(), true)?;
    let directory = NativeEntry::open(&original, true)?;
    let root = fs::canonicalize(store.root())?;
    let dir = fs::canonicalize(&original)?;
    if !dir.starts_with(&root) || dir == root {
        bail!("The captured answer directory must be inside its session store.");
    }
    let target = dir.join(file);
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    no_follow(&mut options, false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    directory.require_matches(&original)?;
    let mut saved = NativeEntry::from_handle(options.open(&target)?, false)?;
    // A failed capture stays on disk for recovery and never produces a receipt.
    check(cancel)?;
    directory.require_matches(&original)?;
    let mut hash = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        check(cancel)?;
        check_deadline(deadline)?;
        saved.handle.write_all(chunk)?;
        hash.update(chunk);
    }
    let expected: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    saved.handle.sync_all()?;
    check(cancel)?;
    before_verify();
    saved.handle.seek(SeekFrom::Start(0))?;
    let stamp = read_stamp(&saved.handle)?;
    verify_streamed_file_until(
        &mut saved.handle,
        bytes.len() as u64,
        &expected,
        Some(cancel),
        deadline,
    )?;
    saved.require_matches(&target)?;
    directory.require_matches(&original)?;
    directory.require_matches(&dir)?;
    root_entry.require_matches(&store.root())?;
    if read_stamp(&saved.handle)? != stamp
        || saved.handle.metadata()?.len() != bytes.len() as u64
        || fs::canonicalize(store.root())? != root
        || fs::canonicalize(&original)? != dir
    {
        bail!("The captured answer bytes or storage identity changed while saving.");
    }
    check(cancel)?;
    Ok(CaptureReceipt {
        version: 1,
        session_id: id.to_owned(),
        file: file.to_owned(),
        encoding: "utf8".into(),
        bytes: bytes.len() as u64,
        sha256: expected,
        saved_at: timestamp(),
    })
}
pub fn verify_capture(store: &SessionStore, receipt: &CaptureReceipt) -> Result<Vec<u8>> {
    verify_capture_until(store, receipt, None, None)
}
pub(crate) fn verify_capture_until(
    store: &SessionStore,
    receipt: &CaptureReceipt,
    cancel: Option<&CancellationToken>,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u8>> {
    check_deadline(deadline)?;
    if let Some(cancel) = cancel {
        check(cancel)?;
    }
    assert_id(&receipt.file)?;
    anyhow::ensure!(
        receipt.version == 1 && receipt.encoding == "utf8",
        "Unsupported capture receipt"
    );
    let directory_path = store.directory(&receipt.session_id)?;
    let directory = NativeEntry::open(&directory_path, true)?;
    let path = directory_path.join(&receipt.file);
    let mut file = NativeEntry::open(&path, false)?;
    anyhow::ensure!(
        file.handle.metadata()?.len() == receipt.bytes,
        "Captured answer size changed"
    );
    let stamp = read_stamp(&file.handle)?;
    let mut bytes = Vec::new();
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        if let Some(cancel) = cancel {
            check(cancel)?;
        }
        let count = file.handle.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        anyhow::ensure!(
            (bytes.len() as u64).saturating_add(count as u64) <= receipt.bytes,
            "Captured answer exceeded its receipt"
        );
        hash.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    file.require_matches(&path)?;
    directory.require_matches(&directory_path)?;
    anyhow::ensure!(
        bytes.len() as u64 == receipt.bytes
            && digest == receipt.sha256
            && read_stamp(&file.handle)? == stamp,
        "Captured answer bytes changed"
    );
    Ok(bytes)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedArtifact {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
    pub native_identity: NativeIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_error: Option<String>,
}
/// A streamed payload staged beside its destination, with native identity retained.
pub(crate) struct ArtifactStage {
    parent_path: PathBuf,
    parent: NativeEntry,
    directory_path: PathBuf,
    directory: NativeEntry,
    path: PathBuf,
    payload: NativeEntry,
    cleaned: bool,
}
pub(crate) fn verify_streamed_file(
    file: &mut fs::File,
    size: u64,
    expected: &str,
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    verify_streamed_file_until(file, size, expected, cancel, None)
}
pub(crate) fn check_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    anyhow::ensure!(
        deadline.is_none_or(|deadline| std::time::Instant::now() < deadline),
        "Artifact recovery deadline reached"
    );
    Ok(())
}
pub(crate) fn verify_streamed_file_until(
    file: &mut fs::File,
    size: u64,
    expected: &str,
    cancel: Option<&CancellationToken>,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    verify_streamed_file_with(file, size, expected, || {
        check_deadline(deadline)?;
        if let Some(cancel) = cancel {
            check(cancel)?;
        }
        Ok(())
    })
}
fn verify_streamed_file_with(
    file: &mut fs::File,
    size: u64,
    expected: &str,
    mut require: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        require()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        anyhow::ensure!(bytes <= size, "Artifact exceeded declared size");
        hash.update(&buffer[..count]);
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    anyhow::ensure!(
        bytes == size && digest == expected,
        "Artifact size or digest differs"
    );
    Ok(())
}
impl ArtifactStage {
    pub(crate) fn create(destination: &Path) -> Result<Self> {
        let parent_path = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_owned();
        private_dir(&parent_path)?;
        let parent = NativeEntry::open(&parent_path, true)?;
        let directory_path = parent_path.join(format!(".calvino-publish-{}", Uuid::new_v4()));
        #[cfg_attr(windows, allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory_path)?;
        let directory = NativeEntry::open(&directory_path, true)?;
        let path = directory_path.join("payload");
        let handle = match private_file(&path, true) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = remove_owned_directory(&directory_path, &directory, &[]);
                return Err(error);
            }
        };
        let payload = NativeEntry::from_handle(handle, false)?;
        Ok(Self {
            parent_path,
            parent,
            directory_path,
            directory,
            path,
            payload,
            cleaned: false,
        })
    }
    fn check(&self) -> Result<()> {
        self.parent.require_matches(&self.parent_path)?;
        self.directory.require_matches(&self.directory_path)?;
        self.payload.require_matches(&self.path)
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.check()?;
        self.payload.handle.write_all(bytes)?;
        Ok(())
    }
    pub(crate) fn inspect<T>(&self, inspect: impl FnOnce(&mut fs::File) -> Result<T>) -> Result<T> {
        self.check()?;
        self.payload.handle.sync_all()?;
        let mut reader = NativeEntry::open(&self.path, false)?;
        anyhow::ensure!(
            reader.identity == self.payload.identity,
            "Artifact stage changed identity"
        );
        let before = read_stamp(&reader.handle)?;
        let value = inspect(&mut reader.handle)?;
        anyhow::ensure!(
            before == read_stamp(&reader.handle)?,
            "Artifact stage changed while being read"
        );
        reader.require_matches(&self.path)?;
        self.check()?;
        Ok(value)
    }
    pub(crate) fn publish(
        self,
        destination: &Path,
        unique: bool,
        size: u64,
        expected: &str,
        cancel: &CancellationToken,
    ) -> Result<PublishedArtifact> {
        self.publish_until(destination, unique, size, expected, cancel, None)
    }
    pub(crate) fn publish_until(
        mut self,
        destination: &Path,
        unique: bool,
        size: u64,
        expected: &str,
        cancel: &CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<PublishedArtifact> {
        let outcome = (|| {
            self.inspect(|file| {
                verify_streamed_file_until(file, size, expected, Some(cancel), deadline)
            })?;
            let mut candidate = destination.to_owned();
            let mut suffix = 2;
            loop {
                check_deadline(deadline)?;
                check(cancel)?;
                self.check()?;
                match fs::hard_link(&self.path, &candidate) {
                    Ok(()) => break,
                    Err(error) if unique && error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let stem = destination
                            .file_stem()
                            .context("Missing artifact filename")?
                            .to_string_lossy();
                        let extension = destination
                            .extension()
                            .map(|e| format!(".{}", e.to_string_lossy()))
                            .unwrap_or_default();
                        candidate = self.parent_path.join(format!("{stem}-{suffix}{extension}"));
                        suffix += 1;
                    }
                    Err(error) => {
                        return Err(error)
                            .context("Cannot publish artifact; existing output is preserved");
                    }
                }
            }
            // No cancellation check after publication: its receipt must reach the caller.
            self.payload.require_matches(&candidate)?;
            self.check()?;
            // Creating our link changes ctime/ChangeTime. Rehash under a new stable read stamp.
            self.inspect(|file| verify_streamed_file(file, size, expected, None))?;
            self.payload.require_matches(&candidate)?;
            #[cfg(unix)]
            self.parent.handle.sync_all()?;
            Ok(PublishedArtifact {
                path: candidate,
                size_bytes: size,
                sha256: expected.into(),
                native_identity: self.payload.identity.clone(),
                cleanup_error: None,
            })
        })();
        match outcome {
            Ok(mut published) => {
                published.cleanup_error=self.cleanup().err().map(|_|"Artifact publication succeeded and was verified, but staging cleanup failed; residual staging data may remain.".into());
                Ok(published)
            }
            Err(error) => Err(self.abort(error)),
        }
    }
    fn cleanup(&mut self) -> Result<()> {
        self.cleaned = true;
        remove_owned_directory(
            &self.directory_path,
            &self.directory,
            &[("payload", &self.payload)],
        )
    }
    pub(crate) fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
        match self.cleanup() {
            Ok(()) => error,
            Err(cleanup) => {
                error.context(format!("Artifact staging cleanup also failed: {cleanup}"))
            }
        }
    }
}
impl Drop for ArtifactStage {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}
/// A no-replace hard link is the publication point. A later cancellation preserves output.
pub fn publish(
    bytes: &[u8],
    destination: &Path,
    unique: bool,
    cancel: &CancellationToken,
) -> Result<PublishedArtifact> {
    publish_with(bytes, destination, unique, cancel, |_| {}, |_| {})
}
fn publish_with(
    bytes: &[u8],
    destination: &Path,
    unique: bool,
    cancel: &CancellationToken,
    before_publish: impl FnOnce(&Path),
    before_cleanup: impl FnOnce(&Path),
) -> Result<PublishedArtifact> {
    check(cancel)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    private_dir(parent)?;
    let parent_entry = NativeEntry::open(parent, true)?;
    let stage_dir = parent.join(format!(".calvino-publish-{}", Uuid::new_v4()));
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&stage_dir)?;
    let directory = NativeEntry::open(&stage_dir, true)?;
    let stage = stage_dir.join("payload");
    let mut payload = None;
    let result = (|| {
        let mut handle = private_file(&stage, true)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
        payload = Some(NativeEntry::from_handle(handle, false)?);
        let expected = sha256(bytes);
        before_publish(&stage_dir);
        parent_entry.require_matches(parent)?;
        directory.require_matches(&stage_dir)?;
        let payload = payload.as_ref().context("Missing artifact payload")?;
        payload.require_matches(&stage)?;
        let mut candidate = destination.to_owned();
        let mut suffix = 2;
        loop {
            check(cancel)?;
            parent_entry.require_matches(parent)?;
            directory.require_matches(&stage_dir)?;
            payload.require_matches(&stage)?;
            match fs::hard_link(&stage, &candidate) {
                Ok(()) => break,
                Err(error) if unique && error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stem = destination
                        .file_stem()
                        .context("Missing publication filename")?
                        .to_string_lossy();
                    let extension = destination
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    candidate = parent.join(format!("{stem}-{suffix}{extension}"));
                    suffix += 1;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Cannot publish artifact to {}", candidate.display())
                    });
                }
            }
        }
        // The published name must still identify the exact staged native file.
        payload.require_matches(&candidate)?;
        let mut linked = NativeEntry::open(&candidate, false)?;
        let mut observed = Vec::new();
        linked.handle.read_to_end(&mut observed)?;
        linked.require_matches(&candidate)?;
        parent_entry.require_matches(parent)?;
        if observed.len() != bytes.len() || sha256(&observed) != expected {
            bail!(
                "Cannot verify published artifact at {}; output is retained.",
                candidate.display()
            );
        }
        #[cfg(unix)]
        parent_entry.handle.sync_all()?;
        Ok(PublishedArtifact {
            path: candidate,
            size_bytes: observed.len() as u64,
            sha256: expected,
            native_identity: linked.identity.clone(),
            cleanup_error: None,
        })
    })();
    before_cleanup(&stage_dir);
    let cleanup = if let Some(payload) = payload.as_ref() {
        remove_owned_directory(&stage_dir, &directory, &[("payload", payload)])
    } else {
        remove_owned_directory(&stage_dir, &directory, &[])
    };
    match (result, cleanup) {
        (Ok(mut result), Err(_)) => {
            result.cleanup_error = Some("Artifact publication succeeded and was verified, but staging cleanup failed; residual staging data may remain.".into());
            Ok(result)
        }
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Artifact staging cleanup also failed: {cleanup}")))
        }
        (result, _) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (tempfile::TempDir, SessionStore, String) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(
                json!({"prompt":"PUBLIC capture identity","model":"gpt-6-astra"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        (home, store, id)
    }
    #[test]
    fn streamed_verification_stops_between_chunks_after_cancellation_or_expiry() {
        let mut file = tempfile::tempfile().unwrap();
        let bytes = vec![42; 3 * 64 * 1024];
        file.write_all(&bytes).unwrap();
        for expired in [false, true] {
            file.seek(SeekFrom::Start(0)).unwrap();
            let cancel = CancellationToken::new();
            let mut checks = 0;
            let result =
                verify_streamed_file_with(&mut file, bytes.len() as u64, &sha256(&bytes), || {
                    checks += 1;
                    if checks == 2 {
                        if expired {
                            check_deadline(Some(std::time::Instant::now()))?;
                        } else {
                            cancel.cancel();
                        }
                    }
                    check(&cancel)
                });
            assert!(result.is_err());
            assert_eq!(file.stream_position().unwrap(), 64 * 1024);
        }
    }
    #[test]
    fn capture_cancellation_before_readback_retains_file_without_a_receipt() {
        let (_home, store, id) = fixture();
        let cancel = CancellationToken::new();
        assert!(
            capture_with(
                &store,
                &id,
                "PUBLIC-interrupted.md",
                b"PUBLIC",
                &cancel,
                || cancel.cancel()
            )
            .is_err()
        );
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("PUBLIC-interrupted.md")).unwrap(),
            b"PUBLIC"
        );
        assert!(
            capture_until(
                &store,
                &id,
                "PUBLIC-expired.md",
                b"PUBLIC",
                &CancellationToken::new(),
                Some(std::time::Instant::now())
            )
            .is_err()
        );
        assert!(
            !store
                .directory(&id)
                .unwrap()
                .join("PUBLIC-expired.md")
                .exists()
        );
    }
    #[cfg(windows)]
    #[test]
    fn windows_capture_child_handle_blocks_parent_replacement_and_verifies_original_bytes() {
        let (_home, store, id) = fixture();
        let path = store.directory(&id).unwrap();
        let moved = path.with_file_name("original-session");
        let receipt = capture_with(
            &store,
            &id,
            "capture.md",
            b"PUBLIC",
            &CancellationToken::new(),
            || {
                let error = fs::rename(&path, &moved).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(!moved.exists());
            },
        )
        .unwrap();
        assert_eq!(verify_capture(&store, &receipt).unwrap(), b"PUBLIC");
    }
    #[cfg(windows)]
    #[test]
    fn windows_publication_blocks_stage_replacement_and_cleans_the_exact_stage() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("answer.md");
        let check_guard = |stage: &Path| {
            let moved = home.path().join("foreign-stage");
            let error = fs::rename(stage, &moved).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(!moved.exists());
        };
        let published = publish_with(
            b"PUBLIC",
            &destination,
            false,
            &CancellationToken::new(),
            check_guard,
            check_guard,
        )
        .unwrap();
        assert!(published.cleanup_error.is_none());
        assert_eq!(fs::read(destination).unwrap(), b"PUBLIC");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }
    #[test]
    fn capture_refuses_replaced_native_file_even_with_identical_bytes() {
        let (_home, store, id) = fixture();
        let path = store.directory(&id).unwrap().join("capture.md");
        let moved = path.with_extension("original");
        assert!(
            capture_with(
                &store,
                &id,
                "capture.md",
                b"PUBLIC exact bytes",
                &CancellationToken::new(),
                || {
                    fs::rename(&path, &moved).unwrap();
                    fs::write(&path, b"PUBLIC exact bytes").unwrap();
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&moved).unwrap(), b"PUBLIC exact bytes");
        assert_eq!(fs::read(path).unwrap(), b"PUBLIC exact bytes");
    }
    #[cfg(not(windows))]
    #[test]
    fn capture_refuses_replaced_session_directory() {
        let (_home, store, id) = fixture();
        let path = store.directory(&id).unwrap();
        let moved = path.with_file_name("original-session");
        assert!(
            capture_with(
                &store,
                &id,
                "capture.md",
                b"PUBLIC",
                &CancellationToken::new(),
                || {
                    fs::rename(&path, &moved).unwrap();
                    fs::create_dir(&path).unwrap();
                    fs::write(path.join("capture.md"), b"PUBLIC").unwrap();
                }
            )
            .is_err()
        );
        assert!(moved.join("capture.md").exists());
        assert!(path.join("capture.md").exists());
    }
    #[cfg(not(windows))]
    #[test]
    fn staging_replacement_before_publication_refuses_output_and_preserves_both_directories() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("answer.md");
        let moved = home.path().join("original-stage");
        assert!(
            publish_with(
                b"PUBLIC",
                &destination,
                false,
                &CancellationToken::new(),
                |stage| {
                    fs::rename(stage, &moved).unwrap();
                    fs::create_dir(stage).unwrap();
                    fs::write(stage.join("payload"), b"PUBLIC").unwrap();
                },
                |_| {}
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert!(moved.join("payload").exists());
        let replacement = fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".calvino-publish-")
            })
            .unwrap();
        assert!(replacement.path().join("payload").exists());
    }
    #[cfg(not(windows))]
    #[test]
    fn successful_publication_retains_replacement_stage_and_reports_cleanup_warning() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("answer.md");
        let result = publish_with(
            b"PUBLIC",
            &destination,
            false,
            &CancellationToken::new(),
            |_| {},
            |stage| {
                fs::rename(stage, home.path().join("original-stage")).unwrap();
                fs::create_dir(stage).unwrap();
                fs::write(stage.join("unrecognised"), b"preserve").unwrap();
            },
        )
        .unwrap();
        assert!(result.cleanup_error.is_some());
        assert_eq!(fs::read(destination).unwrap(), b"PUBLIC");
        let replacement = fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".calvino-publish-")
            })
            .unwrap();
        assert_eq!(
            fs::read(replacement.path().join("unrecognised")).unwrap(),
            b"preserve"
        );
    }
    #[test]
    fn unknown_stage_contents_are_preserved_after_verified_publication() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("answer.md");
        let result = publish_with(
            b"PUBLIC",
            &destination,
            false,
            &CancellationToken::new(),
            |_| {},
            |stage| {
                fs::write(stage.join("unrecognised"), b"preserve").unwrap();
            },
        )
        .unwrap();
        assert!(result.cleanup_error.is_some());
        assert_eq!(fs::read(destination).unwrap(), b"PUBLIC");
    }
}
