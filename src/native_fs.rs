//! Native identity evidence is retained with an open handle to prevent inode reuse.
use anyhow::{Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}
impl NativeIdentity {
    /// Numeric filesystem fields used by saved Node stat receipts.
    pub fn legacy_stat(handle: &File) -> Result<(String, String)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = handle.metadata()?;
            Ok((metadata.dev().to_string(), metadata.ino().to_string()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: this live handle and the correctly sized output remain valid for the call.
            if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut information) } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let inode = (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow);
            Ok((
                information.dwVolumeSerialNumber.to_string(),
                inode.to_string(),
            ))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = handle;
            bail!("Legacy native stat receipts are unqualified on this platform")
        }
    }
    pub fn of(handle: &File) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = handle.metadata()?;
            if metadata.ino() == 0 {
                bail!("Native filesystem identity is unavailable");
            }
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
            };
            // SAFETY: the handle remains open and the output buffer has the documented size.
            let mut information: FILE_ID_INFO = unsafe { std::mem::zeroed() };
            if unsafe {
                GetFileInformationByHandleEx(
                    handle.as_raw_handle(),
                    FileIdInfo,
                    (&mut information as *mut FILE_ID_INFO).cast(),
                    std::mem::size_of::<FILE_ID_INFO>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if information.FileId.Identifier == [0; 16] {
                bail!("Native filesystem identity is unavailable");
            }
            Ok(Self {
                volume: information.VolumeSerialNumber,
                file_id: information.FileId.Identifier,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = handle;
            bail!("Native filesystem identity is unqualified on this platform");
        }
    }
}

pub fn reject_link(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        bail!("Native filesystem entry is a symbolic link");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("Native filesystem entry is a reparse point");
        }
    }
    Ok(metadata)
}
pub fn canonical_wire_path(path: &Path) -> Result<std::path::PathBuf> {
    let canonical = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let text = canonical
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Filesystem path is not valid UTF-8"))?;
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return Ok(std::path::PathBuf::from(format!(r"\\{unc}")));
        }
        if let Some(disk) = text.strip_prefix(r"\\?\") {
            return Ok(std::path::PathBuf::from(disk));
        }
    }
    Ok(canonical)
}

pub fn no_follow(options: &mut OpenOptions, directory: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (options, directory);
}

pub struct NativeEntry {
    pub handle: File,
    pub identity: NativeIdentity,
    directory: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReadStamp {
    bytes: u64,
    modified: i128,
    changed: i128,
}
pub(crate) fn read_stamp(handle: &File) -> Result<ReadStamp> {
    let metadata = handle.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(ReadStamp {
            bytes: metadata.len(),
            modified: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            changed: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
        })
    }
    #[cfg(windows)]
    {
        use std::{
            mem::{size_of, zeroed},
            os::windows::io::AsRawHandle,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
        };
        let mut info: FILE_BASIC_INFO = unsafe { zeroed() };
        // SAFETY: the owned native file handle and correctly sized output remain live for the call.
        let read = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle().cast(),
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        anyhow::ensure!(
            read != 0,
            "Cannot verify native record modification state: {}",
            std::io::Error::last_os_error()
        );
        Ok(ReadStamp {
            bytes: metadata.len(),
            modified: info.LastWriteTime as i128,
            changed: info.ChangeTime as i128,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        bail!("Native record modification checks are unavailable")
    }
}
impl NativeEntry {
    pub(crate) fn open_child(
        &self,
        directory_path: &Path,
        name: &std::ffi::OsStr,
        directory: bool,
    ) -> Result<Self> {
        anyhow::ensure!(
            self.directory
                && Path::new(name).components().count() == 1
                && matches!(
                    Path::new(name).components().next(),
                    Some(std::path::Component::Normal(_))
                ),
            "Expected one native child component"
        );
        #[cfg(unix)]
        {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            };
            let _ = directory_path;
            let name = std::ffi::CString::new(name.as_bytes())?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 };
            // SAFETY: the retained parent descriptor and one NUL-terminated component remain live.
            let descriptor = unsafe { libc::openat(self.handle.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: openat returned a fresh descriptor, transferred exactly once to File.
            Self::from_handle(unsafe { File::from_raw_fd(descriptor) }, directory)
        }
        #[cfg(not(unix))]
        {
            self.require_matches(directory_path)?;
            let child = Self::open(&directory_path.join(name), directory)?;
            self.require_matches(directory_path)?;
            Ok(child)
        }
    }
    pub fn open(path: &Path, directory: bool) -> Result<Self> {
        let metadata = reject_link(path)?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            bail!("Unexpected native filesystem entry type");
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        if directory {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
            options.access_mode(FILE_READ_ATTRIBUTES);
        }
        no_follow(&mut options, directory);
        let handle = options.open(path)?;
        Self::from_handle(handle, directory)
    }
    pub fn from_handle(handle: File, directory: bool) -> Result<Self> {
        let metadata = handle.metadata()?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            bail!("Unexpected native filesystem handle type");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                bail!("Native filesystem handle is a reparse point");
            }
        }
        let identity = NativeIdentity::of(&handle)?;
        Ok(Self {
            handle,
            identity,
            directory,
        })
    }
    pub fn matches(&self, path: &Path) -> bool {
        Self::open(path, self.directory).is_ok_and(|current| current.identity == self.identity)
    }
    pub fn require_matches(&self, path: &Path) -> Result<()> {
        if !self.matches(path) {
            bail!(
                "Native filesystem identity changed; preserving {}",
                path.display()
            );
        }
        Ok(())
    }
}

/// Terminated UTF-16 verbatim path for direct Win32 calls, which std otherwise supplies.
/// Without the `\\?\` form, paths beyond MAX_PATH fail with ERROR_PATH_NOT_FOUND.
#[cfg(windows)]
pub(crate) fn win32_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(fs::canonicalize(parent)?
        .join(name)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect())
}

/// Rename a directory without overwriting an existing entry, including an empty one.
pub fn rename_no_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: both C strings live through the call. No file descriptors are borrowed.
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let (source, target) = (win32_path(source)?, win32_path(target)?);
        // SAFETY: both paths are terminated UTF-16 strings and live through this call.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (source, target);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "No-replace native rename is unqualified",
        ))
    }
}

fn remove_exact(path: &Path, expected: &NativeEntry) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
            FILE_DISPOSITION_INFO_EX, FILE_READ_ATTRIBUTES, FileDispositionInfoEx,
            SetFileInformationByHandle,
        };
        reject_link(path)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .access_mode(DELETE | FILE_READ_ATTRIBUTES);
        no_follow(&mut options, expected.directory);
        let entry = NativeEntry::from_handle(options.open(path)?, expected.directory)?;
        anyhow::ensure!(
            entry.identity == expected.identity,
            "Native deletion identity changed; preserving {}",
            path.display()
        );
        // POSIX disposition unlinks this exact handle even while identity witnesses are open.
        // An unsupported volume fails safely; do not fall back to path-based recursive deletion.
        let information = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        // SAFETY: this handle has DELETE access and the input buffer has the documented size.
        if unsafe {
            SetFileInformationByHandle(
                entry.handle.as_raw_handle(),
                FileDispositionInfoEx,
                (&information as *const FILE_DISPOSITION_INFO_EX).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        expected.require_matches(path)?;
        if expected.directory {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

pub(crate) fn remove_owned_file(path: &Path, expected: &NativeEntry) -> Result<()> {
    remove_owned_file_with(path, expected, || {})
}
fn remove_owned_file_with(
    path: &Path,
    expected: &NativeEntry,
    before_detach: impl FnOnce(),
) -> Result<()> {
    anyhow::ensure!(
        !expected.directory,
        "File retirement requires a native file identity"
    );
    #[cfg(windows)]
    {
        before_detach();
        remove_exact(path, expected)
    }
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("File retirement has no parent"))?;
        let directory = NativeEntry::open(parent, true)?;
        expected.require_matches(path)?;
        let detached = parent.join(format!(".alcuin-remove-{}", uuid::Uuid::new_v4()));
        before_detach();
        directory.require_matches(parent)?;
        rename_no_replace(path, &detached)?;
        let result = (|| {
            expected.require_matches(&detached)?;
            directory.require_matches(parent)?;
            remove_exact(&detached, expected)
        })();
        if result.is_err() {
            let _ = rename_no_replace(&detached, path);
        }
        result
    }
}
pub(crate) fn recognised_contents(
    path: &Path,
    directory: &NativeEntry,
    files: &[(&str, &NativeEntry)],
) -> Result<()> {
    directory.require_matches(path)?;
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Some((_, expected)) = files.iter().find(|(name, _)| entry.file_name() == *name) else {
            bail!(
                "Unrecognised directory contents; preserving {}",
                path.display()
            );
        };
        expected.require_matches(&entry.path())?;
        count += 1;
    }
    if count != files.len() {
        bail!("Directory contents changed; preserving {}", path.display());
    }
    directory.require_matches(path)
}

/// Remove only captured native identities and refuse unknown contents.
/// Unix detaches and rechecks entries; Windows deletes by exact handle because an
/// open child witness prevents renaming its parent directory.
/// A scanner or indexer can briefly hold an entry without delete sharing. Like the reference's
/// lock release, retry the exact removal 6 times with 50 ms steps; each attempt rechecks identity.
#[cfg(windows)]
fn remove_exact_retrying(path: &Path, expected: &NativeEntry) -> Result<()> {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    let mut attempt = 1;
    loop {
        match remove_exact(path, expected) {
            Err(error)
                if attempt < 6
                    && error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        .is_some_and(|code| {
                            matches!(
                                code,
                                ERROR_ACCESS_DENIED
                                    | ERROR_SHARING_VIOLATION
                                    | ERROR_LOCK_VIOLATION
                            )
                        }) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                attempt += 1;
            }
            result => return result,
        }
    }
}
pub(crate) fn remove_owned_directory(
    path: &Path,
    directory: &NativeEntry,
    files: &[(&str, &NativeEntry)],
) -> Result<()> {
    remove_owned_directory_with(path, directory, files, || {})
}
fn remove_owned_directory_with(
    path: &Path,
    directory: &NativeEntry,
    files: &[(&str, &NativeEntry)],
    before_detach: impl FnOnce(),
) -> Result<()> {
    recognised_contents(path, directory, files)?;
    #[cfg(windows)]
    {
        before_detach();
        recognised_contents(path, directory, files)?;
        for (name, expected) in files {
            directory.require_matches(path)?;
            remove_exact_retrying(&path.join(name), expected)?;
        }
        directory.require_matches(path)?;
        remove_exact_retrying(path, directory)
    }
    #[cfg(not(windows))]
    {
        let quarantine = path.with_file_name(format!(".alcuin-retired-{}", uuid::Uuid::new_v4()));
        before_detach();
        rename_no_replace(path, &quarantine)?;
        let removal = (|| {
            recognised_contents(&quarantine, directory, files)?;
            for (name, expected) in files {
                directory.require_matches(&quarantine)?;
                let file = quarantine.join(name);
                expected.require_matches(&file)?;
                // A private name permits verification after the rename, before unlinking.
                let detached = quarantine.join(format!(".alcuin-remove-{}", uuid::Uuid::new_v4()));
                rename_no_replace(&file, &detached)?;
                if let Err(error) = expected.require_matches(&detached) {
                    let _ = rename_no_replace(&detached, &file);
                    return Err(error);
                }
                if let Err(error) = remove_exact(&detached, expected) {
                    let _ = rename_no_replace(&detached, &file);
                    return Err(error);
                }
            }
            directory.require_matches(&quarantine)?;
            // rmdir fails on any newly introduced contents. It never follows a symlink.
            remove_exact(&quarantine, directory)?;
            Ok(())
        })();
        if removal.is_err() {
            let _ = rename_no_replace(&quarantine, path);
        }
        removal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    #[test]
    fn exact_file_retirement_preserves_a_replacement_with_identical_bytes() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("marker.json");
        let original = home.path().join("original.json");
        fs::write(&path, b"PUBLIC same bytes").unwrap();
        let expected = NativeEntry::open(&path, false).unwrap();
        assert!(
            remove_owned_file_with(&path, &expected, || {
                fs::rename(&path, &original).unwrap();
                fs::write(&path, b"PUBLIC same bytes").unwrap();
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"PUBLIC same bytes");
        assert_eq!(fs::read(&original).unwrap(), b"PUBLIC same bytes");
    }
    #[cfg(not(windows))]
    #[test]
    fn replacement_during_detach_is_restored_and_never_removed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("owner.json"), b"identical reused owner").unwrap();
        let directory = NativeEntry::open(&path, true).unwrap();
        let owner = NativeEntry::open(&path.join("owner.json"), false).unwrap();
        let moved = root.path().join("original");
        let result =
            remove_owned_directory_with(&path, &directory, &[("owner.json", &owner)], || {
                fs::rename(&path, &moved).unwrap();
                fs::create_dir(&path).unwrap();
                fs::write(path.join("owner.json"), b"identical reused owner").unwrap();
            });
        assert!(result.is_err());
        assert_eq!(
            fs::read(path.join("owner.json")).unwrap(),
            b"identical reused owner"
        );
        assert_eq!(
            fs::read(moved.join("owner.json")).unwrap(),
            b"identical reused owner"
        );
    }
    #[cfg(windows)]
    #[test]
    fn child_witness_blocks_directory_replacement_and_exact_cleanup_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("owner.json"), b"PUBLIC owner").unwrap();
        let directory = NativeEntry::open(&path, true).unwrap();
        let owner = NativeEntry::open(&path.join("owner.json"), false).unwrap();
        let moved = root.path().join("replacement");
        let result =
            remove_owned_directory_with(&path, &directory, &[("owner.json", &owner)], || {
                let error = fs::rename(&path, &moved).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert_eq!(fs::read(path.join("owner.json")).unwrap(), b"PUBLIC owner");
                assert!(!moved.exists());
            });
        result.unwrap();
        assert!(!path.exists());
        assert!(!moved.exists());
    }
}
