//! Handle-anchored writes and retirement for one private disposable profile.
use crate::native_fs::NativeEntry;
use anyhow::{Context, Result, ensure};
#[cfg(windows)]
use std::fs;
use std::{
    ffi::OsStr,
    fs::File,
    path::{Component, Path, PathBuf},
    time::Instant,
};

pub(crate) struct Directory {
    pub(crate) entry: NativeEntry,
    pub(crate) path: PathBuf,
    #[cfg(windows)]
    _ancestors: Vec<File>,
}
fn component(name: &OsStr) -> Result<()> {
    ensure!(
        Path::new(name).components().count() == 1
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ),
        "Expected one private profile component"
    );
    Ok(())
}
fn budget(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "Disposable profile cleanup deadline reached; remainder retained"
    );
    Ok(())
}
#[cfg(windows)]
fn pinned(path: &Path, directory: bool, deletion: bool) -> Result<File> {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::*;
    let mut options = OpenOptions::new();
    options
        .access_mode(
            FILE_READ_ATTRIBUTES
                | if directory { FILE_LIST_DIRECTORY } else { 0 }
                | if deletion { DELETE } else { 0 },
        )
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    crate::native_fs::no_follow(&mut options, directory);
    options
        .open(path)
        .with_context(|| format!("Cannot pin private profile component {}", path.display()))
}
impl Directory {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_with_delete_access(path, true)
    }
    /// A retained parent must be pinned, but is never itself deleted. Avoid DELETE
    /// access so independently owned sibling profiles can retain the same parent.
    pub(crate) fn open_parent(path: &Path) -> Result<Self> {
        Self::open_with_delete_access(path, false)
    }
    fn open_with_delete_access(path: &Path, deletion: bool) -> Result<Self> {
        let _ = deletion;
        #[cfg(windows)]
        {
            // Denying delete sharing pins every path component while descendants are accessed.
            let mut current = PathBuf::new();
            let mut ancestors = Vec::new();
            let mut rooted = false;
            for part in path.components() {
                rooted |= part == Component::RootDir;
                current.push(part);
                if rooted && current != path {
                    let file = pinned(&current, true, false)?;
                    NativeEntry::from_handle(file.try_clone()?, true)?;
                    ancestors.push(file);
                }
            }
            let entry = NativeEntry::from_handle(pinned(path, true, deletion)?, true)?;
            Ok(Self {
                entry,
                path: path.into(),
                _ancestors: ancestors,
            })
        }
        #[cfg(not(windows))]
        Ok(Self {
            entry: NativeEntry::open(path, true)?,
            path: path.into(),
        })
    }
    pub(crate) fn create_directory(&self, name: &OsStr) -> Result<Self> {
        component(name)?;
        self.entry.require_matches(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: the retained parent and one NUL-terminated child name are valid.
            ensure!(
                unsafe { libc::mkdirat(self.entry.handle.as_raw_fd(), name_c.as_ptr(), 0o700) }
                    == 0,
                "Cannot create private profile directory: {}",
                std::io::Error::last_os_error()
            );
            Ok(Self {
                entry: self.entry.open_child(&self.path, name, true)?,
                path: self.path.join(name),
            })
        }
        #[cfg(windows)]
        {
            let path = self.path.join(name);
            fs::create_dir(&path)?;
            let entry = NativeEntry::from_handle(pinned(&path, true, true)?, true)?;
            Ok(Self {
                entry,
                path,
                _ancestors: vec![],
            })
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("Private profile creation is unavailable")
    }
    pub(crate) fn create_file(&self, name: &OsStr) -> Result<File> {
        component(name)?;
        self.entry.require_matches(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            };
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: openat creates only this leaf beneath the retained directory, without following links.
            let fd = unsafe {
                libc::openat(
                    self.entry.handle.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            ensure!(
                fd >= 0,
                "Cannot create private profile file: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: the fresh descriptor transfers to exactly one File owner.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        #[cfg(windows)]
        {
            crate::storage::private_file(&self.path.join(name), true)
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("Private profile file creation is unavailable")
    }
    pub(crate) fn remove_contents(&self, deadline: Instant, depth: usize) -> Result<()> {
        budget(deadline)?;
        ensure!(
            depth <= 128,
            "Disposable profile cleanup depth exceeded; remainder retained"
        );
        #[cfg(unix)]
        {
            use std::{
                ffi::OsString,
                os::{
                    fd::AsRawFd,
                    unix::ffi::{OsStrExt, OsStringExt},
                },
            };
            struct Listing(*mut libc::DIR);
            impl Drop for Listing {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            // Open a fresh description so repeated enumeration does not share the retained fd's offset.
            let dot = c".";
            let fd = unsafe {
                libc::openat(
                    self.entry.handle.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            ensure!(
                fd >= 0,
                "Cannot enumerate private profile: {}",
                std::io::Error::last_os_error()
            );
            let listing = unsafe { libc::fdopendir(fd) };
            if listing.is_null() {
                unsafe {
                    libc::close(fd);
                }
                return Err(std::io::Error::last_os_error().into());
            }
            let listing = Listing(listing);
            loop {
                budget(deadline)?;
                let row = unsafe { libc::readdir(listing.0) };
                if row.is_null() {
                    break;
                }
                let bytes = unsafe { std::ffi::CStr::from_ptr((*row).d_name.as_ptr()) }.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let name = OsString::from_vec(bytes.to_vec());
                let name_c = std::ffi::CString::new(name.as_bytes())?;
                let mut info: libc::stat = unsafe { std::mem::zeroed() };
                ensure!(
                    unsafe {
                        libc::fstatat(
                            self.entry.handle.as_raw_fd(),
                            name_c.as_ptr(),
                            &mut info,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } == 0,
                    "Cannot inspect private profile child"
                );
                let directory = info.st_mode & libc::S_IFMT == libc::S_IFDIR;
                if directory {
                    let child = Self {
                        entry: self.entry.open_child(&self.path, &name, true)?,
                        path: self.path.join(&name),
                    };
                    child.remove_contents(deadline, depth + 1)?;
                }
                // Never recurse through a path. A replaced leaf can only be unlinked here;
                // replacement directories must be empty and links are never followed.
                ensure!(
                    unsafe {
                        libc::unlinkat(
                            self.entry.handle.as_raw_fd(),
                            name_c.as_ptr(),
                            if directory { libc::AT_REMOVEDIR } else { 0 },
                        )
                    } == 0,
                    "Cannot retire private profile child: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            self.entry.require_matches(&self.path)?;
            for row in fs::read_dir(&self.path)? {
                budget(deadline)?;
                let path = row?.path();
                let metadata = fs::symlink_metadata(&path)?;
                let file = pinned(&path, metadata.is_dir(), true)?;
                if metadata.is_dir()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                {
                    let child = Self {
                        entry: NativeEntry::from_handle(file, true)?,
                        path,
                        _ancestors: vec![],
                    };
                    child.remove_contents(deadline, depth + 1)?;
                    delete_handle(&child.entry.handle)?;
                } else {
                    // Delete a reparse point by its own handle; never inspect its target.
                    delete_handle(&file)?;
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("Private profile retirement is unavailable")
    }
    pub(crate) fn remove_empty(&self, parent: &Self, deadline: Instant) -> Result<()> {
        budget(deadline)?;
        parent.entry.require_matches(&parent.path)?;
        self.entry.require_matches(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
            let name = std::ffi::CString::new(
                self.path
                    .file_name()
                    .context("Private profile has no name")?
                    .as_bytes(),
            )?;
            // Only an empty directory can be removed; recursive contents use retained descriptors above.
            ensure!(
                unsafe {
                    libc::unlinkat(
                        parent.entry.handle.as_raw_fd(),
                        name.as_ptr(),
                        libc::AT_REMOVEDIR,
                    )
                } == 0,
                "Cannot remove empty private profile: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        #[cfg(windows)]
        {
            delete_handle(&self.entry.handle)
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("Private profile retirement is unavailable")
    }
}
#[cfg(windows)]
fn delete_handle(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::*;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: this retained, non-inherited handle has DELETE access and the buffer has the documented size.
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "Cannot retire private profile handle: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
