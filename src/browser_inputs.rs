//! Session-owned upload snapshots survive worker restarts without rereading original files.
use crate::{
    browser_dom::UploadFile,
    browser_prompt::Attachment,
    native_fs::{NativeEntry, NativeIdentity, remove_owned_directory},
    sessions::SessionStore,
    storage::private_dir,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedAttachment {
    pub manifest: Value,
    pub display_path: String,
    pub generated_bundle: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedUploads {
    pub directory: PathBuf,
    pub root_identity: NativeIdentity,
    pub session_identity: NativeIdentity,
    pub directory_identity: NativeIdentity,
    pub files: Vec<StagedAttachment>,
}
impl StagedUploads {
    pub(crate) fn capture_remote(
        store: &SessionStore,
        id: &str,
        attachments: &[Value],
    ) -> Result<Self> {
        use std::{collections::BTreeSet, io::Write};
        let root = NativeEntry::open(&store.root(), true)?;
        let session_path = store.directory(id)?;
        let session = NativeEntry::open(&session_path, true)?;
        let path = session_path.join(format!("browser-inputs-{}", uuid::Uuid::new_v4()));
        private_dir(&path)?;
        let directory = NativeEntry::open(&path, true)?;
        let names = attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let fallback = format!("attachment-{}", index + 1);
                let name = attachment["fileName"]
                    .as_str()
                    .unwrap_or(&fallback)
                    .encode_utf16()
                    .map(|unit| {
                        if unit <= 127
                            && ((unit as u8).is_ascii_alphanumeric()
                                || b"._-".contains(&(unit as u8)))
                        {
                            unit as u8 as char
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>();
                if matches!(name.as_str(), "" | "." | "..") {
                    fallback
                } else {
                    name
                }
            })
            .collect::<Vec<_>>();
        let mut reserved = names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let mut used = BTreeSet::new();
        let mut files = vec![];
        for (attachment, original) in attachments.iter().zip(names) {
            root.require_matches(&store.root())?;
            session.require_matches(&session_path)?;
            directory.require_matches(&path)?;
            let mut name = original.clone();
            let mut suffix = 2;
            if used.contains(&name.to_ascii_lowercase()) {
                let extension = Path::new(&original)
                    .extension()
                    .map(|ext| format!(".{}", ext.to_string_lossy()))
                    .unwrap_or_default();
                let stem = &original[..original.len() - extension.len()];
                loop {
                    name = format!("{stem}-{suffix}{extension}");
                    suffix += 1;
                    if !reserved.contains(&name.to_ascii_lowercase()) {
                        break;
                    }
                }
            }
            used.insert(name.to_ascii_lowercase());
            reserved.insert(name.to_ascii_lowercase());
            let bytes = decode_remote_base64(
                attachment["contentBase64"]
                    .as_str()
                    .context("Remote attachment has no encoded content")?,
            );
            let destination = path.join(&name);
            let mut handle = crate::storage::private_file(&destination, true)?;
            handle.write_all(&bytes)?;
            handle.sync_all()?;
            let linked = NativeEntry::from_handle(handle, false)?;
            let source = UploadFile::open(&destination)?;
            ensure!(
                source.manifest()["sha256"] == crate::artifacts::sha256(&bytes),
                "Remote attachment changed during staging"
            );
            linked.require_matches(&destination)?;
            directory.require_matches(&path)?;
            files.push(StagedAttachment {
                manifest: source.manifest(),
                display_path: attachment["displayPath"].as_str().unwrap_or(&name).into(),
                generated_bundle: false,
            });
        }
        let staged = Self {
            directory: path,
            root_identity: root.identity,
            session_identity: session.identity,
            directory_identity: directory.identity,
            files,
        };
        staged.verify(store, id)?;
        Ok(staged)
    }
    pub fn capture(store: &SessionStore, id: &str, attachments: &[Attachment]) -> Result<Self> {
        let root = NativeEntry::open(&store.root(), true)?;
        let session_path = store.directory(id)?;
        let session = NativeEntry::open(&session_path, true)?;
        let path = session_path.join(format!("browser-inputs-{}", uuid::Uuid::new_v4()));
        private_dir(&path)?;
        let directory = NativeEntry::open(&path, true)?;
        let mut files = Vec::new();
        for attachment in attachments {
            root.require_matches(&store.root())?;
            session.require_matches(&session_path)?;
            directory.require_matches(&path)?;
            let mut source = UploadFile::open(&attachment.path)?;
            ensure!(
                (!attachment.generated_bundle
                    && !crate::browser_prompt::raw_upload(&attachment.path))
                    || source.manifest()["bytes"].as_u64() == Some(attachment.size_bytes),
                "Attachment size changed before durable staging"
            );
            let saved = source.snapshot(&path.join(&source.name))?;
            directory.require_matches(&path)?;
            files.push(StagedAttachment {
                manifest: saved.manifest(),
                display_path: attachment.display_path.clone(),
                generated_bundle: attachment.generated_bundle,
            });
        }
        let staged = Self {
            directory: path,
            root_identity: root.identity,
            session_identity: session.identity,
            directory_identity: directory.identity,
            files,
        };
        staged.verify(store, id)?;
        Ok(staged)
    }
    /// Give a provider attempt its own immutable inputs without rediscovering
    /// original paths or copying another session's native storage identities.
    pub(crate) fn recapture_into(
        &self,
        store: &SessionStore,
        parent: &str,
        child: &str,
    ) -> Result<Self> {
        ensure!(
            parent != child,
            "Provider attempt must have a distinct session"
        );
        self.verify(store, parent)?;
        // Keep the verified source handles alive across copying and final checks.
        let sources = (0..self.files.len())
            .map(|index| self.open(index))
            .collect::<Result<Vec<_>>>()?;
        let attachments = sources
            .iter()
            .zip(&self.files)
            .map(|(source, saved)| {
                Ok(Attachment {
                    path: PathBuf::from(
                        source.manifest()["path"]
                            .as_str()
                            .context("Staged source path missing")?,
                    ),
                    display_path: saved.display_path.clone(),
                    size_bytes: source.manifest()["bytes"]
                        .as_u64()
                        .context("Staged source size missing")?,
                    generated_bundle: saved.generated_bundle,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let captured = Self::capture(store, child, &attachments)?;
        for (before, after) in self.files.iter().zip(&captured.files) {
            ensure!(
                before.manifest["sha256"] == after.manifest["sha256"]
                    && before.manifest["bytes"] == after.manifest["bytes"]
                    && before.display_path == after.display_path
                    && before.generated_bundle == after.generated_bundle,
                "Provider attempt input bytes changed during recapture"
            );
        }
        self.verify(store, parent)?;
        captured.verify(store, child)?;
        drop(sources);
        Ok(captured)
    }
    pub fn paths(&self) -> Result<Vec<PathBuf>> {
        self.files
            .iter()
            .map(|file| {
                file.manifest["path"]
                    .as_str()
                    .map(PathBuf::from)
                    .context("Staged upload has no path")
            })
            .collect()
    }
    fn parents(&self, store: &SessionStore, id: &str) -> Result<(NativeEntry, NativeEntry)> {
        let session_path = store.directory(id)?;
        ensure!(
            self.directory.parent() == Some(session_path.as_path())
                && self
                    .directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("browser-inputs-")),
            "Staged uploads must remain in their original session"
        );
        let root = NativeEntry::open(&store.root(), true)?;
        let session = NativeEntry::open(&session_path, true)?;
        ensure!(
            root.identity == self.root_identity && session.identity == self.session_identity,
            "Staged upload storage identity changed"
        );
        Ok((root, session))
    }
    fn entries(
        &self,
        store: &SessionStore,
        id: &str,
    ) -> Result<(NativeEntry, NativeEntry, NativeEntry)> {
        let (root, session) = self.parents(store, id)?;
        let directory = NativeEntry::open(&self.directory, true)?;
        ensure!(
            directory.identity == self.directory_identity,
            "Staged upload directory identity changed"
        );
        Ok((root, session, directory))
    }
    pub fn open(&self, index: usize) -> Result<UploadFile> {
        let saved = self
            .files
            .get(index)
            .context("Missing staged upload receipt")?;
        let path = Path::new(
            saved.manifest["path"]
                .as_str()
                .context("Staged upload has no path")?,
        );
        ensure!(
            path.parent() == Some(std::fs::canonicalize(&self.directory)?.as_path()),
            "Staged upload escaped its saved directory"
        );
        let mut file = UploadFile::open(path)?;
        file.require_manifest(&saved.manifest)?;
        file.generated_bundle = saved.generated_bundle;
        Ok(file)
    }
    pub fn verify(&self, store: &SessionStore, id: &str) -> Result<()> {
        let (root, session, directory) = self.entries(store, id)?;
        for index in 0..self.files.len() {
            self.open(index)?;
        }
        root.require_matches(&store.root())?;
        session.require_matches(&store.directory(id)?)?;
        directory.require_matches(&self.directory)?;
        Ok(())
    }
    pub fn cleanup(&self, store: &SessionStore, id: &str) -> Result<()> {
        let (root, session) = self.parents(store, id)?;
        if let Err(error) = crate::native_fs::reject_link(&self.directory) {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                root.require_matches(&store.root())?;
                session.require_matches(&store.directory(id)?)?;
                return Ok(());
            }
            return Err(error);
        }
        self.verify(store, id)?;
        let (root, session, directory) = self.entries(store, id)?;
        let paths = self.paths()?;
        let files = paths
            .iter()
            .map(|path| NativeEntry::open(path, false))
            .collect::<Result<Vec<_>>>()?;
        let owned = paths
            .iter()
            .zip(&files)
            .map(|(path, file)| {
                Ok((
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .context("Staged upload has no Unicode basename")?,
                    file,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        root.require_matches(&store.root())?;
        session.require_matches(&store.directory(id)?)?;
        remove_owned_directory(&self.directory, &directory, &owned)
    }
}
fn decode_remote_base64(value: &str) -> Vec<u8> {
    // Buffer.from(base64) ignores whitespace/invalid bytes, accepts URL alphabet and stops at padding.
    let mut output = Vec::with_capacity(value.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in value.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        accumulator = (accumulator << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    fn fixture() -> (
        tempfile::TempDir,
        SessionStore,
        String,
        PathBuf,
        StagedUploads,
    ) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let meta = store
            .create(
                json!({"prompt":"PUBLIC durable upload source","mode":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        let original = home.path().join("public.png");
        fs::write(&original, b"PUBLIC image bytes").unwrap();
        let staged = StagedUploads::capture(
            &store,
            &id,
            &[Attachment {
                path: original.clone(),
                display_path: "public.png".into(),
                size_bytes: 18,
                generated_bundle: false,
            }],
        )
        .unwrap();
        (home, store, id, original, staged)
    }
    #[test]
    fn durable_snapshot_survives_original_removal_and_serialisation() {
        let (_home, store, id, original, staged) = fixture();
        fs::remove_file(original).unwrap();
        let restored: StagedUploads =
            serde_json::from_slice(&serde_json::to_vec(&staged).unwrap()).unwrap();
        restored.verify(&store, &id).unwrap();
        assert_eq!(
            fs::read(restored.paths().unwrap()[0].clone()).unwrap(),
            b"PUBLIC image bytes"
        );
        restored.cleanup(&store, &id).unwrap();
        restored.cleanup(&store, &id).unwrap();
        assert!(!restored.directory.exists());
    }
    #[test]
    fn changed_bytes_or_replaced_native_file_refuse_before_upload() {
        let (_home, store, id, _original, staged) = fixture();
        let path = staged.paths().unwrap().remove(0);
        fs::write(&path, b"PUBLIC other bytes").unwrap();
        assert!(staged.verify(&store, &id).is_err());
        assert!(staged.cleanup(&store, &id).is_err());
        assert!(path.exists());
        fs::rename(&path, path.with_extension("retained")).unwrap();
        fs::write(&path, b"PUBLIC image bytes").unwrap();
        assert!(staged.verify(&store, &id).is_err());
    }
    #[test]
    fn unknown_staging_contents_are_preserved_during_cleanup() {
        let (_home, store, id, _original, staged) = fixture();
        let foreign = staged.directory.join("foreign.txt");
        fs::write(&foreign, b"PUBLIC preserve").unwrap();
        assert!(staged.cleanup(&store, &id).is_err());
        assert_eq!(fs::read(foreign).unwrap(), b"PUBLIC preserve");
        assert!(staged.paths().unwrap()[0].exists());
    }
    #[test]
    fn provider_attempt_recaptures_verified_bytes_with_independent_ownership() {
        let (home, store, parent, original, staged) = fixture();
        fs::remove_file(original).unwrap();
        let child = store
            .create(
                json!({"prompt":"PUBLIC provider attempt source","mode":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let child = child["id"].as_str().unwrap();
        assert!(staged.verify(&store, child).is_err());
        let copied = staged.recapture_into(&store, &parent, child).unwrap();
        copied.verify(&store, child).unwrap();
        assert!(copied.verify(&store, &parent).is_err());
        assert_ne!(copied.directory, staged.directory);
        assert_eq!(
            fs::read(copied.paths().unwrap().remove(0)).unwrap(),
            b"PUBLIC image bytes"
        );
        fs::write(copied.paths().unwrap().remove(0), b"PUBLIC altered attempt").unwrap();
        assert!(copied.verify(&store, child).is_err());
        staged.verify(&store, &parent).unwrap();
        assert!(staged.recapture_into(&store, &parent, &parent).is_err());
    }
    #[test]
    fn altered_parent_staging_never_creates_child_input_storage() {
        let (home, store, parent, _original, staged) = fixture();
        let child = store
            .create(
                json!({"prompt":"PUBLIC provider attempt source","mode":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let child = child["id"].as_str().unwrap();
        fs::write(staged.paths().unwrap().remove(0), b"PUBLIC altered parent").unwrap();
        assert!(staged.recapture_into(&store, &parent, child).is_err());
        assert!(
            !fs::read_dir(store.directory(child).unwrap())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("browser-inputs-"))
        );
    }
}
