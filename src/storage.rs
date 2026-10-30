use crate::{
    native_fs::{NativeEntry, no_follow, remove_owned_directory},
    process::{Liveness, ProcessIdentity},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

pub fn private_file(path: &Path, exclusive: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    no_follow(&mut options, false);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

pub fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_bytes_with_deadline(path, bytes, None)
}
pub(crate) fn atomic_json_until(
    path: &Path,
    value: &impl Serialize,
    deadline: Instant,
) -> Result<()> {
    atomic_bytes_with_deadline(path, &serde_json::to_vec_pretty(value)?, Some(deadline))
}
fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    anyhow::ensure!(
        deadline.is_none_or(|deadline| Instant::now() < deadline),
        "Atomic publication deadline reached"
    );
    Ok(())
}
fn atomic_bytes_with_deadline(path: &Path, bytes: &[u8], deadline: Option<Instant>) -> Result<()> {
    check_deadline(deadline)?;
    let temporary = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .context("Missing filename")?
            .to_string_lossy(),
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = private_file(&temporary, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        for delay in [0, 10, 25, 50, 100, 200, 400, 800] {
            check_deadline(deadline)?;
            if delay != 0 {
                let pause = Duration::from_millis(delay);
                std::thread::sleep(deadline.map_or(pause, |deadline| {
                    pause.min(deadline.saturating_duration_since(Instant::now()))
                }));
            }
            check_deadline(deadline)?;
            match replace_file(&temporary, path) {
                Ok(()) => {
                    #[cfg(unix)]
                    File::open(path.parent().context("Missing parent")?)?.sync_all()?;
                    return Ok(());
                }
                Err(error)
                    if delay != 800
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
                        ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!()
    })();
    let _ = fs::remove_file(temporary);
    result
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec_pretty(value)?)
}

#[cfg(not(windows))]
pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}
#[cfg(windows)]
pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let (source, target) = (
        crate::native_fs::win32_path(source)?,
        crate::native_fs::win32_path(target)?,
    );
    // SAFETY: both paths are terminated UTF-16 and live through this call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    id: String,
    pid: u32,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    process_started_at_ms: Option<f64>,
    #[serde(default)]
    process_identity: Option<ProcessIdentity>,
}
struct OwnerSnapshot {
    record: Owner,
    entry: NativeEntry,
    bytes: Vec<u8>,
}
fn owner(path: &Path) -> Option<OwnerSnapshot> {
    use std::io::Read;
    let mut entry = NativeEntry::open(&path.join("owner.json"), false).ok()?;
    // Ownership records are small. An oversized or unknown record grants no cleanup authority.
    if entry.handle.metadata().ok()?.len() > 64 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    (&mut entry.handle)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let record: Owner = serde_json::from_slice(&bytes).ok()?;
    if record.id.is_empty()
        || record.pid == 0
        || record
            .process_started_at_ms
            .is_some_and(|n| !n.is_finite() || n <= 0.0)
    {
        return None;
    }
    if let Some(identity) = &record.process_identity
        && (identity.pid != record.pid
            || identity.native_start == 0
            || !identity.started_at_ms.is_finite()
            || identity.started_at_ms <= 0.0)
    {
        return None;
    }
    Some(OwnerSnapshot {
        record,
        entry,
        bytes,
    })
}
fn same_owner(path: &Path, snapshot: &OwnerSnapshot) -> bool {
    owner(path).is_some_and(|current| {
        current.entry.identity == snapshot.entry.identity && current.bytes == snapshot.bytes
    })
}
fn owner_liveness(owner: &Owner) -> Liveness {
    if let Some(identity) = &owner.process_identity {
        return identity.liveness();
    }
    match ProcessIdentity::read(owner.pid) {
        Ok(None) => Liveness::Dead,
        Ok(Some(actual)) => {
            // Preserve the maintained tolerance only for historical owners lacking native starts.
            if let Some(started) = owner.process_started_at_ms {
                if (actual.started_at_ms - started).abs() <= 10000.0 {
                    Liveness::Alive
                } else {
                    Liveness::Dead
                }
            } else if let Some(created) = owner
                .created_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            {
                if actual.started_at_ms <= created.timestamp_millis() as f64 + 10000.0 {
                    Liveness::Alive
                } else {
                    Liveness::Dead
                }
            } else {
                Liveness::Unknown
            }
        }
        Err(_) => Liveness::Unknown,
    }
}

/// The reference's description of a held profile lock's owner, for wait and timeout messages.
pub(crate) fn lock_holder(path: &Path) -> String {
    owner(path).map_or_else(
        || "unverified ownership".to_owned(),
        |observed| format!("pid {}", observed.record.pid),
    )
}

pub fn same_directory(a: &Path, b: &Path) -> bool {
    match (NativeEntry::open(a, true), NativeEntry::open(b, true)) {
        (Ok(a), Ok(b)) => a.identity == b.identity,
        _ => false,
    }
}

/// Exclusive acquisition and crash-released native recovery, independent of Tokio.
pub struct DirectoryLock {
    path: PathBuf,
    directory: NativeEntry,
    owner: OwnerSnapshot,
    released: bool,
    deadline: Option<Instant>,
}
impl DirectoryLock {
    /// Checks the original native lock and owner without acquiring or changing a lock.
    pub(crate) fn require_owned(&self) -> Result<()> {
        anyhow::ensure!(!self.released, "Directory lock was already released");
        self.directory.require_matches(&self.path)?;
        anyhow::ensure!(
            same_owner(&self.path, &self.owner),
            "Directory lock owner changed; preserving the replacement"
        );
        Ok(())
    }

    pub(crate) fn acquire_cancellable_until(
        path: &Path,
        deadline: Instant,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        Self::acquire_with_deadline_and_cancel(
            path,
            deadline.saturating_duration_since(Instant::now()),
            Some(deadline),
            &mut |_| {},
            Some(cancel),
        )
    }
    pub(crate) async fn acquire_async(
        path: &Path,
        timeout: Duration,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("Invalid directory lock deadline")?;
        let path = path.to_owned();
        let task_cancel = cancel.child_token();
        let _cancel_on_drop = task_cancel.clone().drop_guard();
        let lock = tokio::task::spawn_blocking(move || {
            Self::acquire_cancellable_until(&path, deadline, &task_cancel)
        })
        .await
        .context("Directory lock admission task failed")??;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Directory lock acquisition cancelled"
        );
        Ok(lock)
    }
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_with(path, timeout, &mut |_| {})
    }
    pub(crate) fn acquire_until(path: &Path, deadline: Instant) -> Result<Self> {
        Self::acquire_with_deadline(
            path,
            deadline.saturating_duration_since(Instant::now()),
            Some(deadline),
            &mut |_| {},
        )
    }
    fn acquire_with(
        path: &Path,
        timeout: Duration,
        after_probe: &mut impl FnMut(&Path),
    ) -> Result<Self> {
        Self::acquire_with_deadline(path, timeout, None, after_probe)
    }
    fn acquire_with_deadline(
        path: &Path,
        timeout: Duration,
        deadline: Option<Instant>,
        after_probe: &mut impl FnMut(&Path),
    ) -> Result<Self> {
        Self::acquire_with_deadline_and_cancel(path, timeout, deadline, after_probe, None)
    }
    fn acquire_with_deadline_and_cancel(
        path: &Path,
        timeout: Duration,
        deadline: Option<Instant>,
        after_probe: &mut impl FnMut(&Path),
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Self> {
        check_deadline(deadline)?;
        anyhow::ensure!(
            !cancel.is_some_and(|cancel| cancel.is_cancelled()),
            "Directory lock acquisition cancelled"
        );
        private_dir(path.parent().context("Missing lock parent")?)?;
        let started = Instant::now();
        let identity = ProcessIdentity::current()?;
        let id = Uuid::new_v4().to_string();
        let mut final_attempt = false;
        loop {
            check_deadline(deadline)?;
            anyhow::ensure!(
                !cancel.is_some_and(|cancel| cancel.is_cancelled()),
                "Directory lock acquisition cancelled"
            );
            let mutex = RecoveryMutex::acquire(path, timeout.saturating_sub(started.elapsed()))?;
            check_deadline(deadline)?;
            #[cfg_attr(windows, allow(unused_mut))]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {
                    let directory = NativeEntry::open(path, true)?;
                    crate::native_fs::recognised_contents(path, &directory, &[])?;
                    let record = Owner {
                        id: id.clone(),
                        pid: identity.pid,
                        created_at: Some(
                            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        ),
                        process_started_at_ms: Some(identity.started_at_ms),
                        process_identity: Some(identity.clone()),
                    };
                    let result = Self::publish_created(path, directory, record, deadline);
                    drop(mutex);
                    return result;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists
                            | std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            if final_attempt {
                bail!(
                    "Timed out waiting for Industrious directory lock {}; preserving the existing lock to avoid split-brain cleanup.",
                    path.display()
                );
            }
            let directory = NativeEntry::open(path, true).ok();
            let modified = directory
                .as_ref()
                .and_then(|d| d.handle.metadata().ok()?.modified().ok());
            let observed = directory.as_ref().and_then(|_| owner(path));
            drop(mutex);
            if let (Some(directory), Some(observed)) = (directory.as_ref(), observed) {
                let liveness = owner_liveness(&observed.record);
                after_probe(path);
                if !directory.matches(path) || !same_owner(path, &observed) {
                    final_attempt = started.elapsed() >= timeout;
                    continue;
                }
                if liveness == Liveness::Dead {
                    let _mutex =
                        RecoveryMutex::acquire(path, timeout.saturating_sub(started.elapsed()))?;
                    check_deadline(deadline)?;
                    if directory.matches(path)
                        && same_owner(path, &observed)
                        && owner_liveness(&observed.record) == Liveness::Dead
                    {
                        // Only recognised owner contents can be removed. Never recursively reap a lock.
                        if remove_owned_directory(
                            path,
                            directory,
                            &[("owner.json", &observed.entry)],
                        )
                        .is_ok()
                        {
                            final_attempt = started.elapsed() >= timeout;
                            continue;
                        }
                    }
                }
            }
            if let (Some(directory), Some(modified)) = (directory.as_ref(), modified) {
                let missing = || matches!(fs::symlink_metadata(path.join("owner.json")),Err(error) if error.kind()==std::io::ErrorKind::NotFound);
                let aged = std::time::SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= Duration::from_secs(5 * 60));
                if aged && missing() {
                    let _mutex =
                        RecoveryMutex::acquire(path, timeout.saturating_sub(started.elapsed()))?;
                    check_deadline(deadline)?;
                    if directory.matches(path)
                        && directory.handle.metadata()?.modified()? == modified
                        && missing()
                        && remove_owned_directory(path, directory, &[]).is_ok()
                    {
                        final_attempt = started.elapsed() >= timeout;
                        continue;
                    }
                }
            }
            // Malformed and inaccessible ownership, and non-empty ownerless locks, remain protected.
            if started.elapsed() >= timeout {
                bail!(
                    "Timed out waiting for Industrious directory lock {}; preserving the existing lock to avoid split-brain cleanup.",
                    path.display()
                );
            }
            std::thread::sleep(
                Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
            );
        }
    }
    // The caller retains the recovery mutex throughout publication and rollback.
    fn publish_created(
        path: &Path,
        directory: NativeEntry,
        record: Owner,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        let publication = (|| {
            directory.require_matches(path)?;
            atomic_bytes_with_deadline(
                &path.join("owner.json"),
                &serde_json::to_vec_pretty(&record)?,
                deadline,
            )?;
            directory.require_matches(path)?;
            let owner = owner(path).context("Unable to verify published lock ownership")?;
            anyhow::ensure!(owner.record == record, "Published lock owner changed");
            crate::native_fs::recognised_contents(
                path,
                &directory,
                &[("owner.json", &owner.entry)],
            )?;
            Ok(owner)
        })();
        let owner = match publication {
            Ok(owner) => owner,
            Err(error) => {
                // Publication can fail after mkdir or even after the owner rename. The
                // recovery mutex is still held: remove only this exact empty directory
                // or its exact published owner, without starting another lock wait.
                let cleanup = match owner(path) {
                    Some(owner) if owner.record == record => {
                        remove_owned_directory(path, &directory, &[("owner.json", &owner.entry)])
                    }
                    _ => remove_owned_directory(path, &directory, &[]),
                };
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!(
                        "Failed lock publication rollback remains pending: {cleanup:#}"
                    ))),
                };
            }
        };
        Ok(Self {
            path: path.to_owned(),
            directory,
            owner,
            released: false,
            deadline,
        })
    }
    pub fn release(mut self) -> Result<()> {
        let result = self.release_owned();
        // A failed explicit release must not make a second destructive attempt in Drop.
        self.released = true;
        result
    }
    pub(crate) fn release_until(mut self, deadline: Instant) -> Result<()> {
        self.deadline = Some(deadline);
        self.release()
    }
    fn release_owned(&self) -> Result<()> {
        let timeout = self.deadline.map_or(Duration::from_secs(10), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let _mutex = RecoveryMutex::acquire(&self.path, timeout)?;
        self.directory.require_matches(&self.path)?;
        anyhow::ensure!(
            same_owner(&self.path, &self.owner),
            "Unable to verify ownership while releasing Industrious directory lock {}; preserving it.",
            self.path.display()
        );
        remove_owned_directory(
            &self.path,
            &self.directory,
            &[("owner.json", &self.owner.entry)],
        )
    }
}
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release_owned();
        }
    }
}

#[cfg(windows)]
fn normalise_windows_path_identity(raw: &str) -> String {
    let raw = if let Some(tail) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{tail}")
    } else {
        raw.strip_prefix(r"\\?\").unwrap_or(raw).to_owned()
    };
    raw.to_lowercase()
}

struct RecoveryMutex {
    #[cfg(unix)]
    _file: File,
    #[cfg(windows)]
    pipe: windows_sys::Win32::Foundation::HANDLE,
}
#[cfg(windows)]
impl Drop for RecoveryMutex {
    fn drop(&mut self) {
        // SAFETY: this mutex uniquely owns the successfully created native pipe handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.pipe);
        }
    }
}
impl RecoveryMutex {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let canonical = fs::canonicalize(path.parent().context("Missing parent")?)?
            .join(path.file_name().context("Missing name")?);
        let identity = canonical.to_string_lossy().into_owned();
        #[cfg(windows)]
        let identity = normalise_windows_path_identity(&identity);
        let digest: String = Sha256::digest(identity.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        #[cfg(unix)]
        {
            use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
            let mutex_path = path
                .parent()
                .context("Missing parent")?
                .join(format!(".oracle-directory-recovery-{}.lock", &digest[..24]));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(mutex_path)?;
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "Recovery mutex is not a native file"
            );
            let started = Instant::now();
            loop {
                // SAFETY: the descriptor remains open. flock is released by close or process death.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Self { _file: file });
                }
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK | libc::EINTR))
                    || started.elapsed() >= timeout
                {
                    return Err(error).context("Unable to acquire directory recovery mutex");
                }
                std::thread::sleep(
                    Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
                );
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::{
                Foundation::INVALID_HANDLE_VALUE,
                Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
                System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT},
            };
            let name: Vec<_> = std::ffi::OsStr::new(&format!(
                r"\\.\pipe\oracle-directory-recovery-{}",
                &digest[..24]
            ))
            .encode_wide()
            .chain(Some(0))
            .collect();
            let started = Instant::now();
            loop {
                // SAFETY: the terminated name is valid and no security attributes are borrowed.
                let pipe = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        1,
                        0,
                        0,
                        0,
                        std::ptr::null(),
                    )
                };
                if pipe != INVALID_HANDLE_VALUE {
                    return Ok(Self { pipe });
                }
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(5 | 231)) || started.elapsed() >= timeout {
                    return Err(error).context("Unable to acquire directory recovery mutex");
                }
                std::thread::sleep(
                    Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
                );
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (digest, timeout);
            bail!("Native directory recovery mutex is unqualified");
        }
    }
}

#[cfg(test)]
mod lock_race_tests {
    use super::*;
    #[test]
    fn async_lock_budget_includes_the_blocking_pool_queue() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-queued.lock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                held.recv().unwrap();
            });
            running.await.unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let lock = DirectoryLock::acquire_async(&path, Duration::from_millis(50), &cancel);
            let release_after_deadline = async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                release.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(lock, release_after_deadline);
            blocker.await.unwrap();
            assert!(
                result.is_err(),
                "Queued work restarted its expired lock budget"
            );
            assert!(!path.exists(), "Expired queued work published a lock owner");
        });
    }
    #[test]
    fn expired_lock_budget_does_not_create_an_owner() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-expired.lock");
        assert!(DirectoryLock::acquire_until(&path, Instant::now()).is_err());
        assert!(!path.exists());
    }
    #[test]
    fn expiry_after_directory_creation_does_not_leave_an_ownerless_lock() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-unpublished.lock");
        let mutex = RecoveryMutex::acquire(&path, Duration::from_secs(1)).unwrap();
        fs::create_dir(&path).unwrap();
        let directory = NativeEntry::open(&path, true).unwrap();
        let identity = ProcessIdentity::current().unwrap();
        let record = Owner {
            id: Uuid::new_v4().to_string(),
            pid: identity.pid,
            created_at: None,
            process_started_at_ms: Some(identity.started_at_ms),
            process_identity: Some(identity),
        };
        let result = DirectoryLock::publish_created(&path, directory, record, Some(Instant::now()));
        assert!(result.is_err());
        drop(mutex);
        assert!(
            !path.exists(),
            "Failed publication stranded a fresh ownerless lock"
        );
        DirectoryLock::acquire(&path, Duration::from_secs(1))
            .unwrap()
            .release()
            .unwrap();
    }
    #[test]
    fn failed_publication_rollback_requires_exact_contents_and_ownership() {
        for case in ["own-published", "foreign-owner", "unknown-file"] {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join("PUBLIC-unpublished.lock");
            let mutex = RecoveryMutex::acquire(&path, Duration::from_secs(1)).unwrap();
            fs::create_dir(&path).unwrap();
            let directory = NativeEntry::open(&path, true).unwrap();
            let identity = ProcessIdentity::current().unwrap();
            let record = Owner {
                id: Uuid::new_v4().to_string(),
                pid: identity.pid,
                created_at: None,
                process_started_at_ms: Some(identity.started_at_ms),
                process_identity: Some(identity),
            };
            let (name, bytes) = if case == "unknown-file" {
                ("PUBLIC-foreign", b"PUBLIC preserved".to_vec())
            } else {
                let mut value = serde_json::to_value(&record).unwrap();
                if case == "foreign-owner" {
                    value["id"] = json!("PUBLIC-replacement");
                }
                ("owner.json", serde_json::to_vec_pretty(&value).unwrap())
            };
            fs::write(path.join(name), &bytes).unwrap();
            let result =
                DirectoryLock::publish_created(&path, directory, record, Some(Instant::now()));
            assert!(result.is_err());
            drop(mutex);
            if case == "own-published" {
                assert!(!path.exists());
            } else {
                assert_eq!(fs::read(path.join(name)).unwrap(), bytes);
            }
        }
    }
    #[test]
    fn bounded_lock_release_and_drop_do_not_restart_the_mutex_timeout() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-bounded.lock");
        let deadline = Instant::now() + Duration::from_millis(100);
        let lock = DirectoryLock::acquire_until(&path, deadline).unwrap();
        let mutex = RecoveryMutex::acquire(&path, Duration::from_secs(1)).unwrap();
        let owner = fs::read(path.join("owner.json")).unwrap();
        let started = Instant::now();
        assert!(lock.release().is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(fs::read(path.join("owner.json")).unwrap(), owner);
        drop(mutex);

        let other_path = home.path().join("PUBLIC-drop.lock");
        let lock =
            DirectoryLock::acquire_until(&other_path, Instant::now() + Duration::from_millis(100))
                .unwrap();
        let mutex = RecoveryMutex::acquire(&other_path, Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        drop(lock);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(other_path.join("owner.json").is_file());
        drop(mutex);
    }
    #[test]
    fn expired_atomic_publication_preserves_the_record_without_creating_staging() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-record.json");
        fs::write(&path, b"PUBLIC preserved record").unwrap();
        assert!(
            atomic_json_until(
                &path,
                &serde_json::json!({"PUBLIC":"late replacement"}),
                Instant::now()
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"PUBLIC preserved record");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }
    use serde_json::json;
    fn dead(path: &Path) -> Vec<u8> {
        fs::create_dir(path).unwrap();
        atomic_json(&path.join("owner.json"),&json!({"id":"reused-owner","pid":u32::MAX,"processIdentity":{"pid":u32::MAX,"nativeStart":1,"startedAtMs":1.0}})).unwrap();
        fs::read(path.join("owner.json")).unwrap()
    }
    #[cfg(not(windows))]
    #[test]
    fn overdue_stale_probe_does_not_reap_replacement_directory_with_identical_owner() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("example.lock");
        let raw = dead(&path);
        let original = home.path().join("original.lock");
        let mut probes = 0;
        let result = DirectoryLock::acquire_with(&path, Duration::from_millis(10), &mut |path| {
            probes += 1;
            fs::rename(path, &original).unwrap();
            std::thread::sleep(Duration::from_millis(40));
            fs::create_dir(path).unwrap();
            fs::write(path.join("owner.json"), &raw).unwrap();
        });
        assert!(result.is_err());
        assert_eq!(probes, 1);
        assert_eq!(fs::read(path.join("owner.json")).unwrap(), raw);
        assert_eq!(fs::read(original.join("owner.json")).unwrap(), raw);
    }
    #[test]
    fn overdue_probe_retries_exclusive_creation_when_the_observed_lock_disappears() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("example.lock");
        dead(&path);
        let mut probes = 0;
        DirectoryLock::acquire_with(&path, Duration::from_millis(10), &mut |path| {
            probes += 1;
            fs::remove_file(path.join("owner.json")).unwrap();
            fs::remove_dir(path).unwrap();
            std::thread::sleep(Duration::from_millis(40));
        })
        .unwrap()
        .release()
        .unwrap();
        assert_eq!(probes, 1);
        assert!(!path.exists());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn only_aged_empty_ownerless_locks_are_recovered() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("example.lock");
        fs::create_dir(&path).unwrap();
        assert!(DirectoryLock::acquire(&path, Duration::from_millis(10)).is_err());
        let age = std::time::SystemTime::now() - Duration::from_secs(600);
        fs::File::open(&path).unwrap().set_modified(age).unwrap();
        DirectoryLock::acquire(&path, Duration::from_secs(1))
            .unwrap()
            .release()
            .unwrap();
        assert!(!path.exists());
        fs::create_dir(&path).unwrap();
        fs::write(path.join("unknown"), b"PUBLIC").unwrap();
        fs::File::open(&path).unwrap().set_modified(age).unwrap();
        assert!(DirectoryLock::acquire(&path, Duration::from_millis(10)).is_err());
        assert_eq!(fs::read(path.join("unknown")).unwrap(), b"PUBLIC");
    }

    #[cfg(windows)]
    #[test]
    fn atomic_records_and_locks_work_beyond_max_path() {
        let home = tempfile::tempdir().unwrap();
        // Two components, so the path passes MAX_PATH however short the temporary root is.
        let deep = home
            .path()
            .join("PUBLIC-".repeat(30))
            .join("PUBLIC-".repeat(30));
        fs::create_dir_all(&deep).unwrap();
        let record = deep.join("PUBLIC-record.json");
        atomic_json(&record, &serde_json::json!({"PUBLIC":1})).unwrap();
        atomic_json(&record, &serde_json::json!({"PUBLIC":2})).unwrap();
        assert!(record.as_os_str().len() > 260);
        assert_eq!(
            fs::read(&record).unwrap(),
            br#"{
  "PUBLIC": 2
}"#
        );
        let lock =
            DirectoryLock::acquire(&deep.join("PUBLIC.lock"), Duration::from_secs(1)).unwrap();
        lock.release().unwrap();
        assert!(!deep.join("PUBLIC.lock").exists());
    }
    #[cfg(windows)]
    #[test]
    fn release_retries_a_transient_sharing_violation() {
        use std::os::windows::fs::OpenOptionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("PUBLIC-shared.lock");
        let lock = DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap();
        // An indexer or scanner briefly holds the owner record without delete sharing.
        let scanner = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(path.join("owner.json"))
            .unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(scanner);
        });
        lock.release().unwrap();
        closer.join().unwrap();
        assert!(!path.exists());
    }
}
