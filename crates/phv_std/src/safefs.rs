//! Opening files beneath an allowed directory without following links.
//!
//! A path checked first and opened later can be changed in between (a
//! directory swapped for a symlink or junction). So paths are never resolved
//! by name in one go: the walk starts from a handle on the allowed directory
//! and opens one component at a time, refusing links.
//!
//! - Unix: `openat` with `O_NOFOLLOW` from a directory descriptor.
//! - Windows: each directory is opened with `FILE_FLAG_OPEN_REPARSE_POINT` and
//!   held without `FILE_SHARE_DELETE`, so it cannot be renamed or replaced
//!   while the file is used.
//!
//! The file that is opened must be a regular file with one link: a hard link
//! planted in the directory would otherwise reach a file elsewhere.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub enum FsError {
    /// A component is a symlink, junction or other reparse point.
    Link,
    /// The file has more than one hard link.
    HardLink,
    /// The path names a directory or a device, not a regular file.
    NotFile,
    /// A directory on the way is missing or is not a directory.
    NoDir,
    /// The name cannot be used safely (`:`, a trailing dot, `CON`, ...).
    BadName,
    /// The allowed directory is writable by other users.
    Shared,
    Io(io::Error),
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        FsError::Io(e)
    }
}

/// Whether a single path component is safe on every platform. Windows strips
/// trailing dots and spaces (`a.phv.` opens `a.phv`), treats `name:stream` as
/// an alternate data stream and maps `CON`, `NUL`, `COM1`... to devices.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.ends_with(['.', ' ']) || name.contains(['\0', ':', '\\', '/', '<', '>', '"', '|', '?', '*']) {
        return false;
    }
    if name.chars().any(|c| c.is_control()) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let numbered = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"));
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || numbered.is_some_and(|d| (d.len() == 1 && d.as_bytes()[0].is_ascii_digit()) || matches!(d, "¹" | "²" | "³"));
    !device
}

/// The normal components of a relative path, each checked with `valid_name`.
fn components(rel: &Path) -> Result<Vec<OsString>, FsError> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(n) if n.to_str().is_some_and(valid_name) => out.push(n.to_os_string()),
            _ => return Err(FsError::BadName),
        }
    }
    Ok(out)
}

/// A directory beneath the allowed one, held open (and on Windows locked
/// against renaming) for as long as the guard lives.
pub struct DirGuard {
    /// The innermost directory (Unix: used with `openat`).
    #[cfg_attr(windows, allow(dead_code))]
    dir: File,
    /// Every directory from the allowed one down, kept open to pin the chain.
    #[allow(dead_code)] // held, never read
    chain: Vec<File>,
    path: PathBuf,
}

/// Opens the directory that contains `rel` (relative to `base`) and returns it
/// with the final file name. `base` itself may be reached through links (it
/// comes from phv.toml); nothing below it may be.
pub fn open_parent(base: &Path, rel: &Path) -> Result<(DirGuard, OsString), FsError> {
    let mut comps = components(rel)?;
    let Some(name) = comps.pop() else { return Err(FsError::BadName) };
    let mut chain = Vec::new();
    let (mut dir, mut path) = imp::open_base(base)?;
    for c in &comps {
        let next = imp::open_subdir(&dir, &path, c)?;
        path.push(c);
        chain.push(std::mem::replace(&mut dir, next));
    }
    imp::check_not_shared(&dir)?;
    for d in &chain {
        imp::check_not_shared(d)?;
    }
    Ok((DirGuard { dir, chain, path }, name))
}

impl DirGuard {
    /// The directory's path (canonical for the allowed directory, then the components).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `name` for reading, or for writing (created if missing, emptied if present).
    pub fn open(&self, name: &OsStr, write: bool) -> Result<File, FsError> {
        let f = imp::open_file(self, name, write)?;
        check_plain(&f)?;
        if write {
            f.set_len(0)?;
        }
        Ok(f)
    }

    /// Whether `name` exists. Links are reported as errors, not followed.
    pub fn exists(&self, name: &OsStr) -> Result<bool, FsError> {
        imp::exists(self, name)
    }

    /// Fails if `name` exists and is not a plain file (a link, a hard link, a directory).
    pub fn check_plain_or_missing(&self, name: &OsStr) -> Result<(), FsError> {
        if !self.exists(name)? {
            return Ok(());
        }
        match imp::open_file(self, name, false) {
            Ok(f) => check_plain(&f),
            // Deleted meanwhile (SQLite removes its journal when a transaction ends).
            Err(FsError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The identity of the plain file `name` (a regular file with one link,
    /// reached by its own name, not a link), or None when there is none. Read
    /// without opening the file's contents.
    pub fn plain_file_id(&self, name: &OsStr) -> Result<Option<FileId>, FsError> {
        imp::plain_file_id(self, name)
    }

    pub fn remove(&self, name: &OsStr) -> Result<(), FsError> {
        if !self.exists(name)? {
            return Err(FsError::Io(io::ErrorKind::NotFound.into()));
        }
        imp::remove(self, name)
    }

    /// Opens `name` for appending (created if missing).
    pub fn open_append(&self, name: &OsStr) -> Result<File, FsError> {
        use std::io::Seek;
        let mut f = imp::open_file(self, name, true)?;
        check_plain(&f)?;
        f.seek(io::SeekFrom::End(0))?;
        Ok(f)
    }

    /// Creates the directory `name` here; an existing directory is fine.
    pub fn make_dir(&self, name: &OsStr) -> Result<(), FsError> {
        match imp::make_dir(self, name) {
            Err(FsError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                // It must be a real directory, not a link or a file of that name.
                imp::open_subdir(&self.dir, &self.path, name).map(|_| ())
            }
            r => r,
        }
    }

    /// The names of the files and directories here, sorted; links and other
    /// entries are left out.
    pub fn entries(&self) -> Result<Vec<String>, FsError> {
        let mut names = imp::entries(self)?;
        names.retain(|n| valid_name(n));
        names.sort();
        Ok(names)
    }

    /// Moves the plain file `name` to `to_name` in `to` (replacing a plain file there).
    pub fn rename_to(&self, name: &OsStr, to: &DirGuard, to_name: &OsStr) -> Result<(), FsError> {
        check_plain(&imp::open_file(self, name, false)?)?;
        to.check_plain_or_missing(to_name)?;
        imp::rename(self, name, to, to_name)
    }
}

/// Which file an entry is (device and inode, or volume and file index): the
/// same identity means the same file, whatever its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId(u64, u64);

/// Why a path was refused, for messages.
pub fn describe(e: &FsError) -> String {
    match e {
        FsError::Link => "a link or junction is on the way".into(),
        FsError::HardLink => "the file has more than one hard link".into(),
        FsError::NotFile => "it is not a file".into(),
        FsError::NoDir => "a directory on the way is missing or not a directory".into(),
        FsError::BadName => "the name cannot be used safely".into(),
        FsError::Shared => "the directory is writable by other users".into(),
        FsError::Io(e) => e.to_string(),
    }
}

/// An open file that is a regular file with one name (no other hard link).
pub fn single_file(f: &File) -> Result<(), FsError> {
    check_plain(f)
}

/// `path` with each 8.3 short name (`RUNNER~1`) written out in full, and no
/// link followed: `None` if part of it does not exist.
#[cfg(windows)]
pub fn long_name(path: &Path) -> Option<PathBuf> {
    imp::long_name(path)
}

/// Whether `f`, opened by the canonical path `path`, is the file at that path:
/// not reached through a link or junction put on the way after `path` was
/// found (which would lead elsewhere). On Unix, open the file with
/// `open_parent` instead (found by the 1.1.1 review: checking a file opened
/// by name took two more lookups by name, which a link swapped in and out
/// could pass).
#[cfg(windows)]
pub fn opened_at(f: &File, path: &Path) -> Result<(), FsError> {
    imp::check_path(f, path)
}

/// Whether `path` is on another machine, or a link or junction on the way
/// leads to one: a network share, a mapped network drive or a device path.
/// Opening one makes Windows contact the host with the user's credentials, so
/// links are read, never followed, and each target is looked at first (found
/// by the 1.1.1 review: a link to a share, or a network drive, passed the
/// check of the name's first characters). A missing path is not refused here
/// (opening it fails). On Unix, network file systems are mounted by the
/// administrator: false.
#[cfg(windows)]
pub fn on_another_machine(path: &Path) -> bool {
    imp::on_another_machine(path)
}

#[cfg(unix)]
pub fn on_another_machine(_path: &Path) -> bool {
    false
}

fn check_plain(f: &File) -> Result<(), FsError> {
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(FsError::NotFile);
    }
    if imp::link_count(f, &meta)? != 1 {
        return Err(FsError::HardLink);
    }
    Ok(())
}

// Files that SQLite opens by their full name beneath a `DirGuard` (see `db::vfs`).

/// Checks a file SQLite opened as `path`: a plain file reached by its own name,
/// not through a link.
#[cfg(windows)]
pub fn check_opened(f: &File, path: &Path) -> Result<(), FsError> {
    imp::check_path(f, path)?;
    check_plain(f)
}

/// Opens `path` (created if missing), checked like `check_opened`, and holds
/// it: while the handle is open the name cannot be replaced by a link.
#[cfg(windows)]
pub fn hold(path: &Path) -> Result<File, FsError> {
    let f = imp::hold(path)?;
    check_opened(&f, path)?;
    Ok(f)
}

/// Checks the entry at `path` without following it. SQLite's descriptor is
/// not reachable on Unix; SQLite itself refuses symlinks there (`O_NOFOLLOW`).
#[cfg(unix)]
pub fn check_path(path: &Path) -> Result<(), FsError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(FsError::Link);
    }
    if !meta.is_file() {
        return Err(FsError::NotFile);
    }
    if meta.nlink() != 1 {
        return Err(FsError::HardLink);
    }
    Ok(())
}

#[cfg(unix)]
mod imp {
    use super::{DirGuard, FsError};
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    fn cstr(s: &OsStr) -> Result<CString, FsError> {
        CString::new(s.as_bytes()).map_err(|_| FsError::BadName)
    }

    fn openat(dir: libc::c_int, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
        let c = cstr(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `c` is a valid NUL-terminated string; the result is checked.
        let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_CLOEXEC, 0o644 as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a freshly opened descriptor that nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn link_error(e: io::Error) -> FsError {
        match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::EMLINK) => FsError::Link,
            Some(libc::ENOTDIR) | Some(libc::ENOENT) => FsError::NoDir,
            _ => FsError::Io(e),
        }
    }

    pub fn open_base(base: &Path) -> Result<(File, PathBuf), FsError> {
        // The allowed directory may itself sit below a link (macOS /var); the
        // canonical name keeps SQLite's NOFOLLOW check happy.
        let base = base.canonicalize().map_err(link_error)?;
        let f = openat(libc::AT_FDCWD, base.as_os_str(), libc::O_RDONLY | libc::O_DIRECTORY).map_err(link_error)?;
        Ok((f, base))
    }

    pub fn open_subdir(dir: &File, _path: &Path, name: &OsStr) -> Result<File, FsError> {
        openat(dir.as_raw_fd(), name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW).map_err(|e| {
            // Linux reports a symlink here as "not a directory"; tell the two apart.
            if e.raw_os_error() == Some(libc::ENOTDIR) && is_symlink(dir, name) {
                FsError::Link
            } else {
                link_error(e)
            }
        })
    }

    fn is_symlink(dir: &File, name: &OsStr) -> bool {
        let Ok(c) = cstr(name) else { return false };
        // SAFETY: `st` is plain data filled by fstatat; the result is checked.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        r == 0 && st.st_mode & libc::S_IFMT == libc::S_IFLNK
    }

    /// Other users must not be able to add entries (links) to the directory:
    /// SQLite opens its files by name, so the directory has to be trusted.
    pub fn check_not_shared(dir: &File) -> Result<(), FsError> {
        let mode = dir.metadata()?.mode();
        if mode & 0o002 != 0 {
            return Err(FsError::Shared);
        }
        Ok(())
    }

    pub fn open_file(g: &DirGuard, name: &OsStr, write: bool) -> Result<File, FsError> {
        let access = if write { libc::O_WRONLY | libc::O_CREAT } else { libc::O_RDONLY };
        // O_NONBLOCK: opening a FIFO must not hang (it is then refused as not a file).
        let flags = access | libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK;
        openat(g.dir.as_raw_fd(), name, flags).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::EMLINK) => FsError::Link,
            Some(libc::EISDIR) => FsError::NotFile,
            _ => FsError::Io(e),
        })
    }

    pub fn exists(g: &DirGuard, name: &OsStr) -> Result<bool, FsError> {
        let c = cstr(name)?;
        // SAFETY: `st` is plain data filled by fstatat; the result is checked.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(g.dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if r != 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::NotFound { Ok(false) } else { Err(FsError::Io(e)) };
        }
        if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(FsError::Link);
        }
        Ok(true)
    }

    pub fn plain_file_id(g: &DirGuard, name: &OsStr) -> Result<Option<super::FileId>, FsError> {
        let c = cstr(name)?;
        // SAFETY: `st` is plain data filled by fstatat; the result is checked.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(g.dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if r != 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(FsError::Io(e)) };
        }
        match st.st_mode & libc::S_IFMT {
            libc::S_IFLNK => return Err(FsError::Link),
            libc::S_IFREG => {}
            _ => return Err(FsError::NotFile),
        }
        if st.st_nlink != 1 {
            return Err(FsError::HardLink);
        }
        #[allow(clippy::unnecessary_cast)] // the types differ between platforms
        Ok(Some(super::FileId(st.st_dev as u64, st.st_ino as u64)))
    }

    pub fn remove(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        let c = cstr(name)?;
        // SAFETY: valid descriptor and NUL-terminated name; unlinkat never follows links.
        if unsafe { libc::unlinkat(g.dir.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn link_count(_: &File, meta: &std::fs::Metadata) -> Result<u64, FsError> {
        Ok(meta.nlink())
    }

    pub fn make_dir(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        let c = cstr(name)?;
        // SAFETY: valid descriptor and NUL-terminated name; mkdirat creates exactly that entry.
        if unsafe { libc::mkdirat(g.dir.as_raw_fd(), c.as_ptr(), 0o755) } != 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// The names of the regular files and directories of the open directory
    /// (read through its own descriptor, never by path).
    pub fn entries(g: &DirGuard) -> Result<Vec<String>, FsError> {
        // SAFETY: `dup` gives a descriptor that `fdopendir` then owns and `closedir` closes.
        let fd = unsafe { libc::dup(g.dir.as_raw_fd()) };
        if fd < 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(FsError::Io(e));
        }
        // From the start, whatever was read through the original descriptor.
        unsafe { libc::rewinddir(dir) };
        let mut names = Vec::new();
        loop {
            // SAFETY: `dir` is open; the entry stays valid until the next readdir.
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let Ok(text) = std::str::from_utf8(bytes) else { continue };
            // lstat-like: links are left out, never followed.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let r = unsafe { libc::fstatat(g.dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
            let kind = st.st_mode & libc::S_IFMT;
            if r == 0 && (kind == libc::S_IFREG || kind == libc::S_IFDIR) {
                names.push(text.to_string());
            }
        }
        unsafe { libc::closedir(dir) };
        Ok(names)
    }

    pub fn rename(from: &DirGuard, name: &OsStr, to: &DirGuard, to_name: &OsStr) -> Result<(), FsError> {
        let (a, b) = (cstr(name)?, cstr(to_name)?);
        // SAFETY: valid descriptors and names; renameat moves the entry itself and never follows links.
        if unsafe { libc::renameat(from.dir.as_raw_fd(), a.as_ptr(), to.dir.as_raw_fd(), b.as_ptr()) } != 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::{DirGuard, FsError};
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, GetLongPathNameW, BY_HANDLE_FILE_INFORMATION,
    };

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const ATTR_DIRECTORY: u32 = 0x10;
    const ATTR_REPARSE_POINT: u32 = 0x400;

    /// Opens a directory and holds it: without FILE_SHARE_DELETE nobody can
    /// rename, delete or replace it while the handle is open.
    fn open_dir(path: &Path, follow: bool) -> Result<File, FsError> {
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let f = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)
            .map_err(|e| if e.kind() == io::ErrorKind::NotFound { FsError::NoDir } else { FsError::Io(e) })?;
        let attrs = f.metadata()?.file_attributes();
        if attrs & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        if attrs & ATTR_DIRECTORY == 0 {
            return Err(FsError::NoDir);
        }
        Ok(f)
    }

    pub fn long_name(path: &Path) -> Option<PathBuf> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `wide` ends with NUL; the buffer pointer and length are valid.
            let n = unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
            if n == 0 {
                return None;
            }
            if n < buf.len() {
                buf.truncate(n);
                return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
            }
            buf.resize(n + 1, 0);
        }
    }

    /// The path of an open handle, in the same form as `canonicalize` (`\\?\C:\...`).
    fn final_path(f: &File) -> Result<PathBuf, FsError> {
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: the buffer pointer and length are valid; the handle is open.
            let n =
                unsafe { GetFinalPathNameByHandleW(f.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
            if n == 0 {
                return Err(FsError::Io(io::Error::last_os_error()));
            }
            if n < buf.len() {
                buf.truncate(n);
                return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
            }
            buf.resize(n + 1, 0);
        }
    }

    pub fn open_base(base: &Path) -> Result<(File, PathBuf), FsError> {
        let f = open_dir(base, true)?;
        let path = final_path(&f)?;
        Ok((f, path))
    }

    pub fn open_subdir(_dir: &File, path: &Path, name: &OsStr) -> Result<File, FsError> {
        let f = open_dir(&path.join(name), false)?;
        check_real_name(&f, name)?;
        Ok(f)
    }

    /// The name must be the entry's own name, not an alias that Windows also
    /// accepts (an 8.3 short name like `LONGTE~1.HTM`, or a different case is
    /// fine). Otherwise checks on the name (like "no .phv") could be bypassed.
    fn check_real_name(f: &File, name: &OsStr) -> Result<(), FsError> {
        let real = final_path(f)?;
        // A file deleted while it is open (POSIX delete semantics) is moved there:
        // it is gone, not an alias.
        if real.to_string_lossy().to_ascii_lowercase().contains(r"\$extend\$deleted\") {
            return Err(FsError::Io(io::ErrorKind::NotFound.into()));
        }
        let real = real.file_name().map(|n| n.to_string_lossy().to_lowercase());
        if real.as_deref() == Some(name.to_string_lossy().to_lowercase().as_str()) {
            Ok(())
        } else {
            Err(FsError::BadName)
        }
    }

    /// `f` must be the entry at `path` itself. A handle opened through a link
    /// has its target's path; one opened with FILE_FLAG_OPEN_REPARSE_POINT is
    /// the link.
    pub fn check_path(f: &File, path: &Path) -> Result<(), FsError> {
        if f.metadata()?.file_attributes() & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        check_real_name(f, path.file_name().ok_or(FsError::BadName)?)?;
        let plain = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").to_lowercase();
        if plain(&final_path(f)?) == plain(path) {
            Ok(())
        } else {
            Err(FsError::BadName)
        }
    }

    /// Without FILE_SHARE_DELETE the file cannot be renamed or deleted (and
    /// its name reused) while the handle is open.
    pub fn hold(path: &Path) -> Result<File, FsError> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?)
    }

    /// Opens an entry only to read its attributes and real name.
    fn open_entry(g: &DirGuard, name: &OsStr) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(0x80) // FILE_READ_ATTRIBUTES
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(g.path.join(name))
    }

    pub fn check_not_shared(_: &File) -> Result<(), FsError> {
        Ok(())
    }

    pub fn open_file(g: &DirGuard, name: &OsStr, write: bool) -> Result<File, FsError> {
        let mut o = OpenOptions::new();
        if write {
            o.write(true).create(true);
        } else {
            o.read(true);
        }
        // The link itself is opened, never its target, and then refused.
        o.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let f = o.open(g.path.join(name)).map_err(|e| {
            if e.raw_os_error() == Some(5) && g.path.join(name).is_dir() {
                FsError::NotFile
            } else {
                FsError::Io(e)
            }
        })?;
        if f.metadata()?.file_attributes() & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        check_real_name(&f, name)?;
        Ok(f)
    }

    pub fn exists(g: &DirGuard, name: &OsStr) -> Result<bool, FsError> {
        let f = match open_entry(g, name) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(FsError::Io(e)),
        };
        if f.metadata()?.file_attributes() & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        match check_real_name(&f, name) {
            Ok(()) => Ok(true),
            Err(FsError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn plain_file_id(g: &DirGuard, name: &OsStr) -> Result<Option<super::FileId>, FsError> {
        let f = match open_entry(g, name) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(FsError::Io(e)),
        };
        let attrs = f.metadata()?.file_attributes();
        if attrs & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        if attrs & ATTR_DIRECTORY != 0 {
            return Err(FsError::NotFile);
        }
        match check_real_name(&f, name) {
            Ok(()) => {}
            Err(FsError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        if info.nNumberOfLinks != 1 {
            return Err(FsError::HardLink);
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok(Some(super::FileId(u64::from(info.dwVolumeSerialNumber), index)))
    }

    pub fn remove(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        // The directory is pinned, so the name still means the checked entry.
        let f = open_entry(g, name)?;
        check_real_name(&f, name)?;
        drop(f);
        Ok(std::fs::remove_file(g.path.join(name))?)
    }

    /// The directories on the way are held open without FILE_SHARE_DELETE, so
    /// the path still names the checked directory.
    pub fn make_dir(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        Ok(std::fs::create_dir(g.path.join(name))?)
    }

    pub fn entries(g: &DirGuard) -> Result<Vec<String>, FsError> {
        let mut names = Vec::new();
        for e in std::fs::read_dir(&g.path)?.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.file_attributes() & ATTR_REPARSE_POINT != 0 {
                continue;
            }
            if let (true, Some(n)) = (meta.is_file() || meta.is_dir(), e.file_name().to_str()) {
                names.push(n.to_string());
            }
        }
        Ok(names)
    }

    pub fn rename(from: &DirGuard, name: &OsStr, to: &DirGuard, to_name: &OsStr) -> Result<(), FsError> {
        let f = open_entry(from, name)?;
        check_real_name(&f, name)?;
        drop(f);
        Ok(std::fs::rename(from.path.join(name), to.path.join(to_name))?)
    }

    pub fn link_count(f: &File, _: &std::fs::Metadata) -> Result<u64, FsError> {
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(info.nNumberOfLinks as u64)
    }

    const DRIVE_REMOTE: u32 = 4;

    /// Whether the spelling of `p` leaves the machine: a UNC path (`\\host`,
    /// `\\?\UNC\host`), a device or object path (`\\.\`, `\\?\GLOBALROOT`), or
    /// a path on a network drive. Nothing is opened.
    pub fn spelled_remote(p: &Path) -> bool {
        let s = p.as_os_str().to_string_lossy().replace('/', "\\").to_ascii_lowercase();
        let rest = match s.strip_prefix(r"\\?\").or_else(|| s.strip_prefix(r"\??\")) {
            Some(rest) if rest.starts_with(r"unc\") => return true,
            // A local volume by its name (mount points lead there).
            Some(rest) if rest.starts_with("volume{") => return false,
            Some(rest) => rest,
            None if s.starts_with(r"\\") => return true,
            None => s.as_str(),
        };
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            let root: Vec<u16> = format!("{}:\\", b[0] as char).encode_utf16().chain(Some(0)).collect();
            // SAFETY: `root` ends with NUL; the call only reads it.
            return unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE;
        }
        // After `\\?\`, anything but a drive or a volume is a device.
        rest.len() != s.len()
    }

    pub fn on_another_machine(path: &Path) -> bool {
        use std::path::Component;
        let mut p = path.to_path_buf();
        // As many links in a row as Windows follows.
        for _ in 0..63 {
            if spelled_remote(&p) {
                return true;
            }
            let comps: Vec<Component> = p.components().collect();
            let mut at = PathBuf::new();
            let mut next = None;
            for (i, c) in comps.iter().enumerate() {
                at.push(c.as_os_str());
                if !matches!(c, Component::Normal(_)) {
                    continue;
                }
                // Read without following it (FILE_FLAG_OPEN_REPARSE_POINT).
                match std::fs::symlink_metadata(&at) {
                    // A symbolic link or a junction: where it leads, then the rest.
                    Ok(m) if m.file_type().is_symlink() => {
                        let Ok(target) = std::fs::read_link(&at) else { return true };
                        let mut t = at.parent().unwrap_or(Path::new("")).join(target);
                        for rest in &comps[i + 1..] {
                            t.push(rest.as_os_str());
                        }
                        next = Some(t);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
            match next {
                Some(t) => p = t,
                None => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phv-safefs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data/sub")).unwrap();
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/secret.txt"), "secret").unwrap();
        d
    }

    #[test]
    fn names() {
        for ok in ["a.txt", "app.db", ".hidden", "x y.txt", "日本語.txt"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["a.phv.", "a ", "a:b", "x.txt::$DATA", "CON", "nul.txt", "COM1", "lpt9.log", "a\\b", "", ".."] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn reads_and_writes_plain_files() {
        let d = tmp("plain");
        let base = d.join("data");
        let (g, name) = open_parent(&base, Path::new("sub/a.txt")).unwrap();
        g.open(&name, true).unwrap().write_all(b"hello").unwrap();
        let mut s = String::new();
        g.open(&name, false).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        // Writing again truncates.
        g.open(&name, true).unwrap().write_all(b"hi").unwrap();
        assert_eq!(std::fs::read_to_string(base.join("sub/a.txt")).unwrap(), "hi");
        assert!(g.exists(&name).unwrap());
        g.remove(&name).unwrap();
        assert!(!g.exists(&name).unwrap());
        assert!(matches!(open_parent(&base, Path::new("missing/a.txt")), Err(FsError::NoDir)));
        assert!(matches!(open_parent(&base, Path::new("../outside/secret.txt")), Err(FsError::BadName)));
        let (g, name) = open_parent(&base, Path::new("sub")).unwrap();
        assert!(matches!(g.open(&name, false), Err(FsError::NotFile)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_hard_links() {
        let d = tmp("hard");
        let base = d.join("data");
        if std::fs::hard_link(d.join("outside/secret.txt"), base.join("hard.txt")).is_err() {
            return;
        }
        let (g, name) = open_parent(&base, Path::new("hard.txt")).unwrap();
        assert!(matches!(g.open(&name, true), Err(FsError::HardLink)));
        assert!(matches!(g.open(&name, false), Err(FsError::HardLink)));
        assert_eq!(std::fs::read_to_string(d.join("outside/secret.txt")).unwrap(), "secret");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks() {
        let d = tmp("sym");
        let base = d.join("data");
        std::os::unix::fs::symlink(d.join("outside"), base.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(d.join("outside/new.txt"), base.join("dangling.txt")).unwrap();
        assert!(matches!(open_parent(&base, Path::new("dirlink/secret.txt")), Err(FsError::Link)));
        let (g, name) = open_parent(&base, Path::new("dangling.txt")).unwrap();
        assert!(matches!(g.open(&name, true), Err(FsError::Link)));
        assert!(!d.join("outside/new.txt").exists());
        // A directory other users can write to is not trusted.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(base.join("sub"), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(open_parent(&base, Path::new("sub/a.txt")), Err(FsError::Shared)));
        // FIFOs and other special files are refused without blocking.
        let fifo = std::ffi::CString::new(base.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        if unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) } == 0 {
            let (g, name) = open_parent(&base, Path::new("fifo")).unwrap();
            assert!(matches!(g.open(&name, false), Err(FsError::NotFile)));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn refuses_short_name_aliases() {
        let d = tmp("short");
        let base = d.join("data");
        std::fs::write(base.join("longtemplatename.phv.html"), "x").unwrap();
        let out = std::process::Command::new("cmd").args(["/c", "dir", "/x"]).arg(&base).output().unwrap();
        let listing = String::from_utf8_lossy(&out.stdout).into_owned();
        // The short name is on the file's own line (the header line can contain
        // `~` too, e.g. `C:\Users\RUNNER~1\...`). Volumes without 8.3 names have none.
        let short = listing
            .lines()
            .find(|l| l.contains("longtemplatename.phv.html"))
            .and_then(|l| l.split_whitespace().find(|w| w.contains('~') && !w.contains(['\\', '/'])))
            .map(str::to_string);
        if let Some(short) = short {
            let (g, name) = open_parent(&base, Path::new(&short)).unwrap();
            assert!(matches!(g.open(&name, true), Err(FsError::BadName)), "{short}");
            assert!(matches!(g.remove(&name), Err(FsError::BadName)));
            assert!(matches!(g.exists(&name), Err(FsError::BadName)));
        }
        // Another case is the same entry, not an alias.
        let (g, name) = open_parent(&base, Path::new("LONGTEMPLATENAME.PHV.HTML")).unwrap();
        assert!(g.exists(&name).unwrap());
        assert_eq!(std::fs::read_to_string(base.join("longtemplatename.phv.html")).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Found by the 1.1.1 review: a link to a share passed the check of the
    /// name's first characters, and was followed.
    #[cfg(windows)]
    #[test]
    fn paths_to_other_machines_are_found_without_opening_them() {
        for p in [r"\\host\share\k.pem", "//host/share/k.pem", r"\\?\UNC\host\share\k.pem", r"\??\UNC\host\s\k"] {
            assert!(on_another_machine(Path::new(p)), "{p}");
        }
        for p in [r"\\.\pipe\x", r"\\?\GLOBALROOT\Device\Mup\host\share"] {
            assert!(on_another_machine(Path::new(p)), "{p}");
        }
        let d = tmp("remote");
        let local = d.join("outside/secret.txt");
        assert!(!on_another_machine(&local));
        assert!(!on_another_machine(&local.canonicalize().unwrap()));
        assert!(!on_another_machine(&d.join("missing/k.pem")));
        // A link to a share is read, not followed (the host is never contacted).
        // Creating one needs Developer Mode or the privilege: skipped without.
        let link = d.join("data/k.pem");
        if std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\k.pem", &link).is_ok() {
            assert!(on_another_machine(&link));
            // Also behind a link to a directory that holds such a link.
            std::os::windows::fs::symlink_dir(d.join("data"), d.join("via")).unwrap();
            assert!(on_another_machine(&d.join("via/k.pem")));
            // A relative link to a local file is fine.
            std::os::windows::fs::symlink_file(r"..\outside\secret.txt", d.join("data/ok.txt")).unwrap();
            assert!(!on_another_machine(&d.join("data/ok.txt")));
        }
        // A junction (always local) to a local directory is fine.
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(d.join("j"))
            .arg(d.join("outside"))
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert!(!on_another_machine(&d.join("j/secret.txt")));
            let _ = std::fs::remove_dir(d.join("j"));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn refuses_junctions_and_pins_directories() {
        let d = tmp("junction");
        let base = d.join("data");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(base.join("j"))
            .arg(d.join("outside"))
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert!(matches!(open_parent(&base, Path::new("j/secret.txt")), Err(FsError::Link)));
            let _ = std::fs::remove_dir(base.join("j"));
        }
        // While a guard is held, its directories cannot be renamed away (and replaced).
        let (g, _) = open_parent(&base, Path::new("sub/a.txt")).unwrap();
        assert!(std::fs::rename(base.join("sub"), base.join("moved")).is_err());
        drop(g);
        assert!(std::fs::rename(base.join("sub"), base.join("moved")).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}
