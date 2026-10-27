//! A copied Chrome profile belongs to one disposable launch, never to its source.
use crate::{
    browser_profile_tree::Directory,
    native_fs::{NativeEntry, read_stamp},
    storage::private_dir,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const MAX_LOCAL_STATE: u64 = 4 * 1024 * 1024;
const MAX_COPY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_COPY_ENTRIES: usize = 100_000;
const EXCLUDED: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "Service Worker/CacheStorage",
    "Service Worker/ScriptCache",
    "Service Worker/Database",
];

pub(crate) struct TemporaryProfile {
    path: PathBuf,
    directory: Option<Arc<Directory>>,
    parent: Arc<Directory>,
    signal: SignalSlot,
    cleanup_on_drop: bool,
}
/// The profile and parent handles a termination signal may remove with. The owner's cleanup
/// and the signal take turns under its lock, since the signal exits the process mid-removal;
/// whichever removes the profile clears it, so no stale pin keeps a Windows name delete-pending.
type SignalSlot = Arc<Mutex<Option<(Arc<Directory>, Arc<Directory>)>>>;
/// The profile's own pinned handles, for removal by a termination signal: Windows refuses
/// a second pin while the owner holds the first.
pub(crate) struct ProfilePins {
    path: PathBuf,
    signal: SignalSlot,
}
impl ProfilePins {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn remove(&self, deadline: Instant) -> Result<()> {
        let mut slot = self.signal.lock().unwrap_or_else(PoisonError::into_inner);
        // The owner already removed it.
        let Some((directory, parent)) = slot.as_ref() else {
            return Ok(());
        };
        remove(&self.path, directory, parent, deadline, || {})?;
        *slot = None;
        Ok(())
    }
}
fn remove(
    path: &Path,
    directory: &Directory,
    parent: &Directory,
    deadline: Instant,
    before_contents: impl FnOnce(),
) -> Result<()> {
    parent.entry.require_matches(&parent.path)?;
    directory.entry.require_matches(path)?;
    before_contents();
    directory.remove_contents(deadline, 0)?;
    directory.remove_empty(parent, deadline)
}
pub(crate) fn configured_source(config: &Value) -> Result<Option<&str>> {
    ensure!(
        config["chromeProfile"].is_null() || config["chromeProfile"].is_string(),
        "Chrome profile selection must be a directory name"
    );
    if config["copyProfileSource"].is_null() {
        // Ordinary sync reads this source profile; it never becomes the launch profile.
        ensure!(
            config["chromeProfile"].is_null()
                || config["chromeProfile"] == "Default"
                || config["cookieSync"] == true
                || config["manualLoginCookieSync"] == true,
            "Custom Chrome profile selection requires copy-profile or Chrome cookie sync"
        );
        return Ok(None);
    }
    let source = config["copyProfileSource"]
        .as_str()
        .filter(|source| !source.trim().is_empty())
        .context("Copy-profile requires a source user-data directory")?;
    ensure!(
        config["chromeProfile"].is_null() || config["chromeProfile"].is_string(),
        "Copied Chrome profile selection must be a directory name"
    );
    if let Some(name) = config["chromeProfile"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        direct_profile(name)?;
    }
    ensure!(
        config["manualLogin"] != true
            && config["manualLoginProfileDir"].is_null()
            && config["keepBrowser"] != true
            && config["attachRunning"] != true
            && !config["remoteChrome"].is_object()
            && config["cookieBootstrapRequired"].is_null(),
        "Copied profiles require a disposable local browser without manual login, keep-browser, borrowed attachment or explicit cookie bootstrap"
    );
    Ok(Some(source))
}
impl TemporaryProfile {
    pub(crate) fn reopen_owned(
        path: &Path,
        identity: &crate::native_fs::NativeIdentity,
        parent_identity: &crate::native_fs::NativeIdentity,
    ) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Missing disposable profile name")?;
        ensure!(
            name.strip_prefix("calvino-browser-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
            "Only an exact disposable profile can be recovered"
        );
        let parent =
            Directory::open_parent(path.parent().context("Disposable profile has no parent")?)?;
        let directory = Directory::open(path)?;
        ensure!(
            directory.entry.identity == *identity && parent.entry.identity == *parent_identity,
            "Disposable profile or parent identity changed; preserving its contents"
        );
        directory.entry.require_matches(path)?;
        parent.entry.require_matches(&parent.path)?;
        Ok(Self::pinned(path.into(), directory, parent, false))
    }
    pub(crate) fn create(base: &Path) -> Result<Self> {
        private_dir(base)?;
        let base = fs::canonicalize(base)?;
        let parent = Directory::open_parent(&base)?;
        let directory = parent.create_directory(std::ffi::OsStr::new(&format!(
            "calvino-browser-{}",
            uuid::Uuid::new_v4()
        )))?;
        parent.entry.require_matches(&base)?;
        Ok(Self::pinned(
            directory.path.clone(),
            directory,
            parent,
            true,
        ))
    }
    fn pinned(
        path: PathBuf,
        directory: Directory,
        parent: Directory,
        cleanup_on_drop: bool,
    ) -> Self {
        let (directory, parent) = (Arc::new(directory), Arc::new(parent));
        Self {
            path,
            signal: Arc::new(Mutex::new(Some((directory.clone(), parent.clone())))),
            directory: Some(directory),
            parent,
            cleanup_on_drop,
        }
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn pins(&self) -> Option<ProfilePins> {
        self.directory.as_ref()?;
        Some(ProfilePins {
            path: self.path.clone(),
            signal: self.signal.clone(),
        })
    }
    pub(crate) fn create_file(&self, name: &std::ffi::OsStr) -> Result<fs::File> {
        self.directory().create_file(name)
    }
    fn directory(&self) -> &Directory {
        self.directory
            .as_ref()
            .expect("Disposable profile was already retired")
    }
    /// Once Chrome can use the directory, only its confirmed exit permits deletion.
    pub(crate) fn launched(&mut self) {
        self.cleanup_on_drop = false;
    }
    pub(crate) fn cleanup(&mut self) -> Result<()> {
        self.cleanup_until(Instant::now() + std::time::Duration::from_secs(5))
    }
    pub(crate) fn cleanup_until(&mut self, deadline: Instant) -> Result<()> {
        self.cleanup_with(deadline, || {})
    }
    fn cleanup_with(&mut self, deadline: Instant, before_contents: impl FnOnce()) -> Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        {
            let mut signal = self.signal.lock().unwrap_or_else(PoisonError::into_inner);
            // Otherwise a termination signal already removed it.
            if signal.is_some() {
                remove(
                    &self.path,
                    directory,
                    &self.parent,
                    deadline,
                    before_contents,
                )?;
                *signal = None;
            }
        }
        // Windows can retain a delete-pending name until its final owning handle closes.
        self.directory = None;
        self.cleanup_on_drop = false;
        ensure!(
            !self.path.try_exists()?,
            "Disposable profile removal is not yet acknowledged"
        );
        Ok(())
    }
}
impl Drop for TemporaryProfile {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Err(error) = self.cleanup()
        {
            eprintln!("Disposable profile cleanup failed: {error}");
        }
    }
}

fn check(deadline: Instant, cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Chrome profile copy cancelled");
    ensure!(
        Instant::now() < deadline,
        "Chrome profile copy deadline reached"
    );
    Ok(())
}
/// Reference resolveChromeProfileDirectory: Local State is parsed only to find the last-used
/// profile when none was requested.
pub(crate) fn profile_directory(local_state: &[u8], requested: Option<&str>) -> Result<String> {
    let mut name = requested.map(str::trim).unwrap_or_default().to_owned();
    if name.is_empty() {
        let state: Value = serde_json::from_slice(local_state).map_err(|error| {
            anyhow::anyhow!(
                "--copy-profile: could not parse \"Local State\" to select the active Chrome profile: {error}"
            )
        })?;
        name = state["profile"]["last_used"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
    }
    if name.is_empty() {
        name = "Default".into();
    }
    direct_profile(&name).with_context(|| {
        format!(
            "--copy-profile: Chrome profile must be a direct child of the user-data directory; received {}.",
            Value::from(name.as_str())
        )
    })
}
fn direct_profile(name: &str) -> Result<String> {
    ensure!(
        !name.contains(['/', '\\', ':', '\0'])
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
            && Path::new(name).components().count() == 1,
        "Chrome profile must be one direct child of its user-data directory"
    );
    Ok(name.into())
}

struct CopyBudget<'a> {
    deadline: Instant,
    cancel: &'a CancellationToken,
    bytes: u64,
    entries: usize,
}
impl CopyBudget<'_> {
    fn entry(&mut self) -> Result<()> {
        check(self.deadline, self.cancel)?;
        self.entries += 1;
        ensure!(
            self.entries <= MAX_COPY_ENTRIES,
            "Chrome profile exceeds the copy entry limit"
        );
        Ok(())
    }
    fn file(
        &mut self,
        mut source: NativeEntry,
        path: &Path,
        dest: &Directory,
        name: &std::ffi::OsStr,
    ) -> Result<()> {
        self.entry()?;
        let stamp = read_stamp(&source.handle)?;
        let mut output = dest.create_file(name)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            check(self.deadline, self.cancel)?;
            let count = source.handle.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            self.bytes += count as u64;
            ensure!(
                self.bytes <= MAX_COPY_BYTES,
                "Chrome profile exceeds the 2 GiB copy limit"
            );
            output.write_all(&buffer[..count])?;
        }
        source.require_matches(path)?;
        let after = read_stamp(&source.handle)?;
        ensure!(
            after == stamp,
            "Chrome profile file changed while copying; retry after Chrome settles"
        );
        output.sync_all()?;
        check(self.deadline, self.cancel)
    }
    fn tree(
        &mut self,
        source: &NativeEntry,
        path: &Path,
        dest: &Directory,
        relative: &Path,
        depth: usize,
    ) -> Result<()> {
        self.entry()?;
        ensure!(depth <= 64, "Chrome profile exceeds the copy depth limit");
        source.require_matches(path)?;
        for entry in fs::read_dir(path)? {
            check(self.deadline, self.cancel)?;
            let entry = entry?;
            let relative = relative.join(entry.file_name());
            if EXCLUDED
                .iter()
                .any(|excluded| relative == Path::new(excluded))
            {
                continue;
            }
            let metadata = crate::native_fs::reject_link(&entry.path())?;
            let child = source.open_child(path, &entry.file_name(), metadata.is_dir())?;
            if metadata.is_dir() {
                let target = dest.create_directory(&entry.file_name())?;
                self.tree(&child, &entry.path(), &target, &relative, depth + 1)?;
            } else {
                self.file(child, &entry.path(), dest, &entry.file_name())?;
            }
            source.require_matches(path)?;
        }
        source.require_matches(path)
    }
}

pub(crate) fn copy_profile(
    source: &Path,
    dest: &TemporaryProfile,
    requested: Option<&str>,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<String> {
    check(deadline, cancel)?;
    ensure!(
        source.is_absolute(),
        "Chrome profile source must resolve to an absolute path"
    );
    let mut resolved = PathBuf::new();
    for component in source.components() {
        match component {
            Component::ParentDir => {
                if resolved.file_name().is_some() {
                    resolved.pop();
                }
            }
            Component::CurDir => {}
            _ => resolved.push(component),
        }
    }
    let source = resolved.as_path();
    // Capture every source ancestor before reading any session-bearing file.
    let mut path = PathBuf::new();
    let mut ancestors = Vec::new();
    for component in source.components() {
        path.push(component);
        if path.has_root() {
            ancestors.push((path.clone(), NativeEntry::open(&path, true)?));
        }
    }
    let root = &ancestors
        .last()
        .context("Chrome profile source is missing")?
        .1;
    let mut state = root.open_child(source, std::ffi::OsStr::new("Local State"), false)?;
    let stamp = read_stamp(&state.handle)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut state.handle)
        .take(MAX_LOCAL_STATE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_LOCAL_STATE,
        "Chrome Local State exceeds its copy limit"
    );
    ensure!(
        read_stamp(&state.handle)? == stamp,
        "Chrome Local State changed while selecting the profile"
    );
    let name = profile_directory(&bytes, requested)?;
    let profile = root.open_child(source, std::ffi::OsStr::new(&name), true)?;
    dest.directory().entry.require_matches(dest.path())?;
    let mut copied_state = dest.create_file(std::ffi::OsStr::new("Local State"))?;
    copied_state.write_all(&bytes)?;
    copied_state.sync_all()?;
    let target = dest
        .directory()
        .create_directory(std::ffi::OsStr::new(&name))?;
    let mut budget = CopyBudget {
        deadline,
        cancel,
        bytes: bytes.len() as u64,
        entries: 1,
    };
    budget.tree(&profile, &source.join(&name), &target, Path::new(""), 0)?;
    state.require_matches(&source.join("Local State"))?;
    ensure!(
        read_stamp(&state.handle)? == stamp,
        "Chrome Local State changed during profile copy"
    );
    for (path, entry) in ancestors {
        entry.require_matches(&path)?;
    }
    dest.directory().entry.require_matches(dest.path())?;
    check(deadline, cancel)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sibling_disposable_profiles_share_only_parent_pins_and_retire_independently() {
        let home = tempfile::tempdir().unwrap();
        let mut first = TemporaryProfile::create(home.path()).unwrap();
        let mut second = TemporaryProfile::create(home.path()).unwrap();
        let second_path = second.path().to_owned();
        first.cleanup().unwrap();
        assert!(second_path.exists());
        second.cleanup().unwrap();
        assert!(!second_path.exists());
    }
    use std::time::Duration;
    #[test]
    fn selects_only_one_direct_profile_and_copies_full_bytes_without_caches() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        private_dir(&source.join("Profile 2/Service Worker/CacheStorage")).unwrap();
        private_dir(&source.join("Default")).unwrap();
        fs::write(
            source.join("Local State"),
            br#"{"profile":{"last_used":"Profile 2"}}"#,
        )
        .unwrap();
        fs::write(
            source.join("Profile 2/Cookies"),
            b"PUBLIC synthetic cookie bytes\0full",
        )
        .unwrap();
        fs::write(
            source.join("Profile 2/Service Worker/CacheStorage/cache"),
            b"PUBLIC omitted",
        )
        .unwrap();
        fs::write(source.join("Default/foreign"), b"PUBLIC untouched").unwrap();
        let mut copied = TemporaryProfile::create(&home.path().join("owned")).unwrap();
        assert_eq!(
            copy_profile(
                &source.join("..").join("source"),
                &copied,
                None,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new()
            )
            .unwrap(),
            "Profile 2"
        );
        assert_eq!(
            fs::read(copied.path().join("Profile 2/Cookies")).unwrap(),
            b"PUBLIC synthetic cookie bytes\0full"
        );
        assert!(
            !copied
                .path()
                .join("Profile 2/Service Worker/CacheStorage")
                .exists()
        );
        assert!(!copied.path().join("Default").exists());
        copied.cleanup().unwrap();
        assert!(!copied.path().exists());
        assert!(source.join("Default/foreign").exists());
        for name in [
            "..",
            ".",
            "../Default",
            "Profile/Other",
            "Profile\\Other",
            "C:Default",
            "/Default",
        ] {
            assert!(profile_directory(b"{}", Some(name)).is_err(), "{name}");
        }
        assert_eq!(
            profile_directory(b"not json", Some(" Profile 2 ")).unwrap(),
            "Profile 2"
        );
        assert!(
            profile_directory(b"not json", None)
                .unwrap_err()
                .to_string()
                .starts_with(
                    "--copy-profile: could not parse \"Local State\" to select the active Chrome profile: "
                )
        );
        assert_eq!(
            profile_directory(b"{}", Some("../Default"))
                .unwrap_err()
                .to_string(),
            "--copy-profile: Chrome profile must be a direct child of the user-data directory; received \"../Default\"."
        );
    }
    #[cfg(unix)]
    #[test]
    fn replacement_directory_never_inherits_disposable_cleanup_authority() {
        let home = tempfile::tempdir().unwrap();
        let mut copied = TemporaryProfile::create(home.path()).unwrap();
        let path = copied.path().to_owned();
        fs::write(path.join("PUBLIC-original"), b"PUBLIC preserve original").unwrap();
        let moved = home.path().join("PUBLIC-original-moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("PUBLIC-foreign"), b"PUBLIC preserve replacement").unwrap();
        copied.launched();
        assert!(copied.cleanup().is_err());
        assert_eq!(
            fs::read(path.join("PUBLIC-foreign")).unwrap(),
            b"PUBLIC preserve replacement"
        );
        assert_eq!(
            fs::read(moved.join("PUBLIC-original")).unwrap(),
            b"PUBLIC preserve original"
        );
    }
    #[cfg(unix)]
    #[test]
    fn replacement_after_cleanup_check_never_receives_recursive_deletion() {
        let home = tempfile::tempdir().unwrap();
        let mut copy = TemporaryProfile::create(home.path()).unwrap();
        copy.launched();
        let path = copy.path().to_owned();
        fs::write(path.join("PUBLIC-owned"), b"PUBLIC owned").unwrap();
        let moved = home.path().join("PUBLIC-original-moved");
        assert!(
            copy.cleanup_with(Instant::now() + Duration::from_secs(2), || {
                fs::rename(&path, &moved).unwrap();
                fs::create_dir(&path).unwrap();
                fs::write(path.join("PUBLIC-foreign"), b"PUBLIC preserve replacement").unwrap();
            })
            .is_err()
        );
        assert_eq!(
            fs::read(path.join("PUBLIC-foreign")).unwrap(),
            b"PUBLIC preserve replacement"
        );
        assert!(!moved.join("PUBLIC-owned").exists());
    }
    #[test]
    fn either_remover_retires_the_profile_whilst_the_other_holds_its_pins() {
        let home = tempfile::tempdir().unwrap();
        for signal_first in [false, true] {
            let mut copy = TemporaryProfile::create(home.path()).unwrap();
            copy.launched();
            let path = copy.path().to_owned();
            fs::write(path.join("PUBLIC-owned"), b"PUBLIC owned").unwrap();
            let pins = copy.pins().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            if signal_first {
                pins.remove(deadline).unwrap();
            }
            copy.cleanup().unwrap();
            pins.remove(deadline).unwrap();
            assert!(!path.exists(), "signal first: {signal_first}");
        }
    }
    #[test]
    fn signal_removal_waits_for_the_owner_cleanup_in_progress() {
        let home = tempfile::tempdir().unwrap();
        let mut copy = TemporaryProfile::create(home.path()).unwrap();
        copy.launched();
        let path = copy.path().to_owned();
        for index in 0..50 {
            fs::write(path.join(format!("PUBLIC-{index}")), b"PUBLIC owned").unwrap();
        }
        let pins = copy.pins().unwrap();
        let mut signal = None;
        // A termination signal removes the same profile whilst the owner's cleanup is under way.
        copy.cleanup_with(Instant::now() + Duration::from_secs(5), || {
            signal = Some(std::thread::spawn(move || {
                pins.remove(Instant::now() + Duration::from_secs(5))
            }));
            std::thread::sleep(Duration::from_millis(200));
        })
        .unwrap();
        signal.unwrap().join().unwrap().unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn destination_parent_cannot_redirect_profile_writes() {
        #[cfg(windows)]
        use std::io::Write;
        let home = tempfile::tempdir().unwrap();
        let mut copy = TemporaryProfile::create(home.path()).unwrap();
        let target = copy
            .directory()
            .create_directory(std::ffi::OsStr::new("Default"))
            .unwrap();
        let moved = copy.path().join("PUBLIC-moved");
        let replacement = home.path().join("PUBLIC-foreign");
        fs::create_dir(&replacement).unwrap();
        #[cfg(unix)]
        {
            fs::rename(&target.path, &moved).unwrap();
            std::os::unix::fs::symlink(&replacement, &target.path).unwrap();
            assert!(target.create_file(std::ffi::OsStr::new("Cookies")).is_err());
        }
        #[cfg(windows)]
        {
            // The retained parent cannot be replaced by a junction or renamed directory.
            assert!(fs::rename(&target.path, &moved).is_err());
            target
                .create_file(std::ffi::OsStr::new("Cookies"))
                .unwrap()
                .write_all(b"PUBLIC copied")
                .unwrap();
        }
        assert!(!replacement.join("Cookies").exists());
        drop(target);
        copy.cleanup().unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn retained_root_and_ancestors_refuse_replacement_until_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join("PUBLIC-base");
        let mut copy = TemporaryProfile::create(&base).unwrap();
        let path = copy.path().to_owned();
        assert!(fs::rename(&path, base.join("PUBLIC-moved")).is_err());
        assert!(fs::rename(&base, home.path().join("PUBLIC-base-moved")).is_err());
        fs::write(path.join("PUBLIC-owned"), b"PUBLIC owned").unwrap();
        copy.cleanup().unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn expired_or_cancelled_copy_never_reads_the_source_and_drop_removes_partial_copy() {
        let home = tempfile::tempdir().unwrap();
        for cancelled in [false, true] {
            let copy = TemporaryProfile::create(home.path()).unwrap();
            let path = copy.path().to_owned();
            let cancel = CancellationToken::new();
            if cancelled {
                cancel.cancel();
            }
            assert!(
                copy_profile(
                    &home.path().join("absent"),
                    &copy,
                    None,
                    Instant::now(),
                    &cancel
                )
                .is_err()
            );
            drop(copy);
            assert!(!path.exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn linked_profile_and_nested_link_refuse_without_reading_their_targets() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        private_dir(&source.join("Default")).unwrap();
        fs::write(source.join("Local State"), b"{}").unwrap();
        let foreign = home.path().join("PUBLIC-foreign");
        fs::write(&foreign, b"PUBLIC preserve").unwrap();
        symlink(&foreign, source.join("Default/Cookies")).unwrap();
        let copy = TemporaryProfile::create(&home.path().join("owned")).unwrap();
        assert!(
            copy_profile(
                &source,
                &copy,
                None,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new()
            )
            .is_err()
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"PUBLIC preserve");
    }
}
