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
        self.check_own(&f, name)?;
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
    ///
    /// A file that is being deleted is missing: SQLite removes its journal
    /// when a transaction ends, while other requests look at it here. On
    /// Windows such a file stays in the directory until its last handle is
    /// closed (this check's own, in another request) and cannot be opened
    /// meanwhile: the name is then looked at again, a few milliseconds at
    /// most (audit 21: 54 of 12,000 `Database::open()` of a file other
    /// requests were writing to failed with "permission denied" or "several
    /// hard links"). A file whose access is really denied is still refused.
    pub fn check_plain_or_missing(&self, name: &OsStr) -> Result<(), FsError> {
        let mut looks = 0;
        loop {
            match self.plain_or_missing(name) {
                Err(FsError::Io(e)) if looks < 3 && imp::may_be_deleting(&e) => {
                    looks += 1;
                    std::thread::sleep(std::time::Duration::from_millis(looks));
                }
                r => return r,
            }
        }
    }

    fn plain_or_missing(&self, name: &OsStr) -> Result<(), FsError> {
        if !self.exists(name)? {
            return Ok(());
        }
        match imp::open_file(self, name, false) {
            Ok(f) => {
                let meta = f.metadata()?;
                if !meta.is_file() {
                    return Err(FsError::NotFile);
                }
                // The count is read once. No link at all: deleted after it
                // was opened (a hard link of something has two or more).
                match imp::link_count(&f, &meta)? {
                    0 | 1 => Ok(()),
                    _ => Err(FsError::HardLink),
                }
            }
            // Deleted meanwhile.
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

    /// Opens `name` for appending (created if missing). The operating system
    /// puts every write at the end of the file as it is at that moment
    /// (`O_APPEND`, `FILE_APPEND_DATA`), so scripts, job runners and server
    /// processes appending to one file at once each add their text whole
    /// (found by the 21st assessment: the file was opened for writing and
    /// positioned at its end once, so two writers wrote at the same offset
    /// and one's lines were lost or cut). Checked as any file opened for
    /// writing is.
    pub fn open_append(&self, name: &OsStr) -> Result<File, FsError> {
        let f = imp::open_append(self, name)?;
        self.check_own(&f, name)?;
        Ok(f)
    }

    /// `f`, opened as `name` here, is a regular file whose one link is that
    /// name. The count alone does not say so: a name that was a hard link to
    /// a file elsewhere when it was opened, and was removed right after,
    /// leaves the file with one link, the one elsewhere (found by the 21st
    /// assessment: a process allowed to write in the directory and to link
    /// the other file made a few of 20,000 appends land in it). On Windows
    /// the system gives the file's links with their directories in one
    /// answer; on Unix the name is looked at again, which leaves a much
    /// narrower race to a process that can link the file (with the kernel's
    /// `protected_hardlinks`, the default, only its owner or one who may
    /// write it anyway).
    fn check_own(&self, f: &File, name: &OsStr) -> Result<(), FsError> {
        check_plain(f)?;
        imp::sole_link(self, f, name)
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
        self.check_own(&imp::open_file(self, name, false)?, name)?;
        to.check_plain_or_missing(to_name)?;
        imp::rename(self, name, to, to_name)
    }
}

/// Which file an entry is (device and inode, or volume and file index): the
/// same identity means the same file, whatever its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64, u64);

/// What can be known of a file's content without reading it: which file it
/// is (see `FileId`), its size, when it was last written and when it last
/// changed (the inode's change time on Unix, which a program cannot set back;
/// NTFS's ChangeTime on Windows, which setting the write time back changes).
/// Two equal stamps taken at different times are the same file, unchanged in
/// between, unless it was written again within the clock's resolution of the
/// first stamp (the callers' "racy" rule), or (on Windows) a program set both
/// times back through the native API, or the file system keeps no change
/// time (FAT and exFAT; a network share may not either).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    file: FileId,
    len: u64,
    /// Seconds and nanoseconds since the epoch (None: before it).
    written: Option<(u64, u32)>,
    /// The change time (seconds, nanoseconds since the epoch): the inode's on
    /// Unix, the file's ChangeTime on Windows (NTFS); zero where the file
    /// system keeps none.
    changed: (i64, i64),
    is_file: bool,
}

impl Stamp {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A regular file (not a directory, a pipe or a device).
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// When the file was last written: seconds and nanoseconds since the
    /// epoch, as `Metadata::modified` gives them (None before the epoch).
    pub fn modified(&self) -> Option<(u64, u32)> {
        self.written
    }

    /// When the file last changed in any way (seconds and nanoseconds since
    /// the epoch; zero where the file system keeps no change time).
    pub fn changed(&self) -> (i64, i64) {
        self.changed
    }

    /// Whether the file was last written and changed in any way more than two
    /// seconds before `at` (seconds since the epoch). A file stamped
    /// at `at` that was not could have been written again within the clock's
    /// resolution without its stamp changing (the "racy" case). A time before
    /// the epoch is never settled.
    pub fn settled_before(&self, at: u64) -> bool {
        let changed = u64::try_from(self.changed.0).unwrap_or(0);
        self.written.is_some_and(|(w, _)| w.saturating_add(2) < at) && changed.saturating_add(2) < at
    }
}

/// The stamp of an open file.
pub fn stamp(f: &File) -> io::Result<Stamp> {
    imp::stamp(f)
}

/// The stamp of the file at `path`, links followed as opening it would. The
/// file is not opened for reading (on Unix not opened at all, so a pipe does
/// not block).
pub fn stamp_path(path: &Path) -> io::Result<Stamp> {
    imp::stamp_path(path)
}

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
/// check of the name's first characters). A missing file is not refused here
/// (opening it fails), but a folder on the way that cannot be read after a
/// link is. On Unix, network file systems are mounted by the administrator:
/// false.
#[cfg(windows)]
pub fn on_another_machine(path: &Path) -> bool {
    imp::on_another_machine(None, path)
}

#[cfg(unix)]
pub fn on_another_machine(_path: &Path) -> bool {
    false
}

/// Like `on_another_machine` for a path below `base` (a project's directory,
/// which the user chose and may be anywhere): only the part of `path` below
/// `base` is looked at, and where its links lead. A link that leads back
/// below `base` is fine; one that leads elsewhere is looked at whole. A path
/// not below `base` is looked at whole. `base` is taken by its real folder
/// (where Windows resolves the relative links below it), unless it is on
/// another machine.
#[cfg(windows)]
pub fn on_another_machine_below(base: &Path, path: &Path) -> bool {
    imp::on_another_machine(Some(base), path)
}

#[cfg(unix)]
pub fn on_another_machine_below(_base: &Path, _path: &Path) -> bool {
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

    fn stamp_of(m: &std::fs::Metadata) -> super::Stamp {
        let written = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()));
        super::Stamp {
            file: super::FileId(m.dev(), m.ino()),
            len: m.len(),
            written,
            changed: (m.ctime(), m.ctime_nsec()),
            is_file: m.is_file(),
        }
    }

    pub fn stamp(f: &File) -> io::Result<super::Stamp> {
        Ok(stamp_of(&f.metadata()?))
    }

    pub fn stamp_path(path: &Path) -> io::Result<super::Stamp> {
        Ok(stamp_of(&std::fs::metadata(path)?))
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
        open_with(g, name, if write { libc::O_WRONLY | libc::O_CREAT } else { libc::O_RDONLY })
    }

    /// `open_file` for writing, with every write put at the end of the file.
    pub fn open_append(g: &DirGuard, name: &OsStr) -> Result<File, FsError> {
        open_with(g, name, libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND)
    }

    fn open_with(g: &DirGuard, name: &OsStr, access: libc::c_int) -> Result<File, FsError> {
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

    /// See `DirGuard::check_own`: the name still names the opened file, and
    /// the file has that one link. A file deleted or replaced since it was
    /// opened (no link left) is fine, as before: it is nobody else's.
    pub fn sole_link(g: &DirGuard, f: &File, name: &OsStr) -> Result<(), FsError> {
        let c = cstr(name)?;
        let opened = f.metadata()?;
        // SAFETY: `st` is plain data filled by fstatat; the result is checked.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(g.dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        #[allow(clippy::unnecessary_cast)] // the types differ between platforms
        let same = r == 0 && st.st_dev as u64 == opened.dev() && st.st_ino as u64 == opened.ino();
        if same && st.st_nlink == 1 {
            return Ok(());
        }
        // Not under its name any more: deleted or replaced, unless it still has a link somewhere.
        if !same && f.metadata()?.nlink() == 0 {
            return Ok(());
        }
        Err(FsError::HardLink)
    }

    /// See the Windows one: here a deleted file's name is gone at once.
    pub fn may_be_deleting(_: &io::Error) -> bool {
        false
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
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileDispositionInfo, FileDispositionInfoEx, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetLongPathNameW,
        SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
        FILE_DISPOSITION_INFO_EX, FILE_FULL_DIR_INFO, FILE_RENAME_INFO,
    };

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const ATTR_DIRECTORY: u32 = 0x10;
    const ATTR_REPARSE_POINT: u32 = 0x400;

    // What `NtCreateFile` takes (ntdll): the access wanted, what to do when
    // the name exists or not, and how to open it.
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    /// What `OpenOptions::append` asks for: every write right but FILE_WRITE_DATA.
    const APPEND_ONLY: u32 = 0x0012_0116 & !0x2;
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_OPEN_IF: u32 = 3;
    const FILE_DIRECTORY_FILE: u32 = 0x1;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
    const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x4000;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *const UnicodeString,
        attributes: u32,
        security_descriptor: *const std::ffi::c_void,
        security_quality_of_service: *const std::ffi::c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status: isize,
        information: usize,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateFile(
            handle: *mut HANDLE,
            access: u32,
            attributes: *const ObjectAttributes,
            status: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *const std::ffi::c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
        fn NtQueryInformationFile(
            handle: HANDLE,
            status: *mut IoStatusBlock,
            information: *mut std::ffi::c_void,
            length: u32,
            class: u32,
        ) -> i32;
        fn NtSetInformationFile(
            handle: HANDLE,
            status: *mut IoStatusBlock,
            information: *const std::ffi::c_void,
            length: u32,
            class: u32,
        ) -> i32;
    }

    /// Why `open_at` failed: the error as the Win32 calls give it, and
    /// whether the name is a directory where a file was asked for.
    struct OpenAtError {
        error: io::Error,
        is_directory: bool,
    }

    /// An error of `open_at` as the callers report it. The system refuses to
    /// open anything relative to a directory that has become a link since its
    /// handle was opened (ERROR_CANT_RESOLVE_FILENAME): that is a link on the
    /// way, like any other.
    fn link_or_io(e: io::Error) -> FsError {
        if e.raw_os_error() == Some(1921) {
            FsError::Link
        } else {
            FsError::Io(e)
        }
    }

    /// Opens the entry `name` of the open directory `dir`, relative to its
    /// handle: the system looks the one name up in that very directory, so
    /// nothing that happens to the directory's path meanwhile (found by the
    /// 21st assessment: an empty subdirectory given a junction's reparse
    /// point in place, while its handle was held, sent a file opened by path
    /// to the junction's target; `mk1` lines were appended outside the
    /// permitted directory) can lead the open elsewhere. The entry itself is
    /// opened, never what it links to (`FILE_OPEN_REPARSE_POINT`); the callers
    /// refuse a link. `name` is one component (`valid_name`).
    fn open_at(
        dir: &File,
        name: &OsStr,
        access: u32,
        share: u32,
        disposition: u32,
        options: u32,
    ) -> Result<File, OpenAtError> {
        let fail = |error: io::Error| OpenAtError { error, is_directory: false };
        let wide: Vec<u16> = name.encode_wide().collect();
        let bytes = u16::try_from(wide.len() * 2).map_err(|_| fail(io::ErrorKind::InvalidInput.into()))?;
        if wide.is_empty() || wide.iter().any(|c| *c == b'\\' as u16 || *c == b'/' as u16 || *c == 0) {
            return Err(fail(io::ErrorKind::InvalidInput.into()));
        }
        let object_name = UnicodeString { length: bytes, maximum_length: bytes, buffer: wide.as_ptr() };
        let attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: dir.as_raw_handle() as HANDLE,
            object_name: &object_name,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null(),
            security_quality_of_service: std::ptr::null(),
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        let mut io_status = IoStatusBlock { status: 0, information: 0 };
        // SAFETY: every pointer is to a live value of the type the call takes;
        // `wide` outlives the call; the handle is owned by the `File` below.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                // As `CreateFileW` adds them: to wait for its I/O, and to read what it is.
                access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
                &attributes,
                &mut io_status,
                std::ptr::null(),
                0x80, // FILE_ATTRIBUTE_NORMAL, for a file it creates
                share,
                disposition,
                options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: a plain conversion of the status.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(OpenAtError {
                error: io::Error::from_raw_os_error(code as i32),
                is_directory: status == STATUS_FILE_IS_A_DIRECTORY,
            });
        }
        // SAFETY: a handle the call just opened, owned by nothing else.
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    /// Opens a directory by its path and holds it: without FILE_SHARE_DELETE
    /// nobody can rename, delete or replace it while the handle is open. Only
    /// for the permitted directory itself (`open_base`): what is below it is
    /// opened relative to its parent's handle (`open_at`).
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

    /// The subdirectory `name` of the open directory `dir`, held like it
    /// (see `open_dir`), opened relative to `dir`'s handle.
    pub fn open_subdir(dir: &File, _path: &Path, name: &OsStr) -> Result<File, FsError> {
        let f = open_at(
            dir,
            name,
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_OPEN_FOR_BACKUP_INTENT,
        )
        .map_err(|e| {
            if e.error.kind() == io::ErrorKind::NotFound {
                FsError::NoDir
            } else {
                link_or_io(e.error)
            }
        })?;
        let attrs = f.metadata()?.file_attributes();
        if attrs & ATTR_REPARSE_POINT != 0 {
            return Err(FsError::Link);
        }
        if attrs & ATTR_DIRECTORY == 0 {
            return Err(FsError::NoDir);
        }
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

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    /// Opens an entry only to read its attributes and real name.
    fn open_entry(g: &DirGuard, name: &OsStr) -> io::Result<File> {
        open_at(&g.dir, name, FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT)
            .map_err(|e| e.error)
    }

    pub fn check_not_shared(_: &File) -> Result<(), FsError> {
        Ok(())
    }

    pub fn open_file(g: &DirGuard, name: &OsStr, write: bool) -> Result<File, FsError> {
        if write {
            open_with(g, name, GENERIC_WRITE, FILE_OPEN_IF)
        } else {
            open_with(g, name, GENERIC_READ, FILE_OPEN)
        }
    }

    /// `open_file` for writing, with every write put at the end of the file
    /// (the handle may append and nothing else: `FILE_APPEND_DATA`).
    pub fn open_append(g: &DirGuard, name: &OsStr) -> Result<File, FsError> {
        open_with(g, name, APPEND_ONLY, FILE_OPEN_IF)
    }

    /// Opens the file `name` of the guard's directory, relative to the
    /// directory's handle (`open_at`), never by its path.
    fn open_with(g: &DirGuard, name: &OsStr, access: u32, disposition: u32) -> Result<File, FsError> {
        // The link itself is opened, never its target, and then refused.
        let f = open_at(&g.dir, name, access, SHARE_ALL, disposition, FILE_NON_DIRECTORY_FILE).map_err(|e| {
            if e.is_directory {
                FsError::NotFile
            } else {
                link_or_io(e.error)
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
            Err(e) => return Err(link_or_io(e)),
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
            Err(e) => return Err(link_or_io(e)),
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

    /// Opens the file `name` to delete or move it: the entry itself, by the
    /// directory's handle, under its own name.
    fn open_to_move(g: &DirGuard, name: &OsStr) -> Result<File, FsError> {
        let f = open_at(&g.dir, name, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, FILE_NON_DIRECTORY_FILE)
            .map_err(|e| link_or_io(e.error))?;
        check_real_name(&f, name)?;
        Ok(f)
    }

    /// Deletes the very entry that was opened and checked, through its handle
    /// (as `std::fs::remove_file` does it: at once, also when others have the
    /// file open; on a file system without that, when the last handle closes).
    pub fn remove(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        let f = open_to_move(g, name)?;
        let h = f.as_raw_handle() as HANDLE;
        // FILE_DISPOSITION_FLAG_DELETE | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE
        let ex = FILE_DISPOSITION_INFO_EX { Flags: 0x1 | 0x2 | 0x10 };
        let size = std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32;
        // SAFETY: `ex` and `plain` are plain data of the size given; the handle is open.
        if unsafe { SetFileInformationByHandle(h, FileDispositionInfoEx, (&raw const ex).cast(), size) } != 0 {
            return Ok(());
        }
        let plain = FILE_DISPOSITION_INFO { DeleteFile: true };
        let size = std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32;
        if unsafe { SetFileInformationByHandle(h, FileDispositionInfo, (&raw const plain).cast(), size) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Creates the directory `name` in the guard's directory, by its handle.
    pub fn make_dir(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        open_at(&g.dir, name, GENERIC_READ, SHARE_ALL, FILE_CREATE, FILE_DIRECTORY_FILE)
            .map(drop)
            .map_err(|e| link_or_io(e.error))
    }

    /// The names of the files and directories of the open directory, read
    /// through its own handle, never by its path; links are left out.
    pub fn entries(g: &DirGuard) -> Result<Vec<String>, FsError> {
        let h = g.dir.as_raw_handle() as HANDLE;
        let mut names = Vec::new();
        // Aligned for the entries, which start with a 32-bit offset and hold 64-bit times.
        let mut buf = vec![0u64; 8192];
        let bytes = (buf.len() * 8) as u32;
        let name_at = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
        let mut class = FileFullDirectoryRestartInfo;
        loop {
            // SAFETY: the buffer is `bytes` long and aligned; the handle is open.
            if unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), bytes) } == 0 {
                let e = io::Error::last_os_error();
                // ERROR_NO_MORE_FILES; an empty directory answers ERROR_FILE_NOT_FOUND to the first call.
                return match e.raw_os_error() {
                    Some(18) | Some(2) => Ok(names),
                    _ => Err(FsError::Io(e)),
                };
            }
            class = FileFullDirectoryInfo;
            let base = buf.as_ptr().cast::<u8>();
            let mut at = 0usize;
            loop {
                // SAFETY: the system filled the buffer with entries, each
                // starting at an offset it gave, inside the buffer.
                let entry = unsafe { &*base.add(at).cast::<FILE_FULL_DIR_INFO>() };
                let len = (entry.FileNameLength as usize / 2).min((bytes as usize - at - name_at) / 2);
                let wide = unsafe { std::slice::from_raw_parts(base.add(at + name_at).cast::<u16>(), len) };
                if entry.FileAttributes & ATTR_REPARSE_POINT == 0 {
                    if let Ok(name) = String::from_utf16(wide) {
                        if name != "." && name != ".." {
                            names.push(name);
                        }
                    }
                }
                let next = entry.NextEntryOffset as usize;
                if next == 0 || at + next + name_at > bytes as usize {
                    break;
                }
                at += next;
            }
        }
    }

    /// Moves the very entry that was opened and checked, through its handle,
    /// to the name `to_name` of the directory `to` holds open (given by its
    /// handle, not its path), replacing a file of that name.
    pub fn rename(from: &DirGuard, name: &OsStr, to: &DirGuard, to_name: &OsStr) -> Result<(), FsError> {
        let f = open_to_move(from, name)?;
        let wide: Vec<u16> = to_name.encode_wide().collect();
        let name_at = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = name_at + wide.len() * 2 + 2;
        let mut buf = vec![0u64; size.div_ceil(8) + 1];
        let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        let mut io_status = IoStatusBlock { status: 0, information: 0 };
        // The system's own call, not `SetFileInformationByHandle`, which reads
        // the name as a path and takes no directory handle.
        // SAFETY: the buffer is aligned and large enough for the structure
        // and the name after it; both handles are open. 10: FileRenameInformation.
        let status = unsafe {
            (*info).Anonymous.ReplaceIfExists = true;
            (*info).RootDirectory = to.dir.as_raw_handle() as HANDLE;
            (*info).FileNameLength = (wide.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                buf.as_mut_ptr().cast::<u8>().add(name_at).cast::<u16>(),
                wide.len(),
            );
            NtSetInformationFile(f.as_raw_handle() as HANDLE, &mut io_status, info.cast(), size as u32, 10)
        };
        if status < 0 {
            // SAFETY: a plain conversion of the status.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(FsError::Io(io::Error::from_raw_os_error(code as i32)));
        }
        Ok(())
    }

    /// See `DirGuard::check_own`: the system lists the file's links, each
    /// with the directory it is in, in one answer (`FileHardLinkInformation`):
    /// there must be one, `name` in the guard's directory. A file system
    /// without hard links (FAT, exFAT) does not answer; there the count
    /// `check_plain` read is all there is.
    pub fn sole_link(g: &DirGuard, f: &File, name: &OsStr) -> Result<(), FsError> {
        // FILE_LINKS_INFORMATION: BytesNeeded, EntriesReturned, then entries of
        // NextEntryOffset, ParentFileId (at 8), FileNameLength (at 16, in
        // characters) and the name (at 20).
        let mut buf = vec![0u64; 512];
        let mut io_status = IoStatusBlock { status: 0, information: 0 };
        // SAFETY: the buffer is as long as said and aligned; the handle is open. 46: FileHardLinkInformation.
        let status = unsafe {
            NtQueryInformationFile(f.as_raw_handle() as HANDLE, &mut io_status, buf.as_mut_ptr().cast(), 4096, 46)
        };
        // STATUS_BUFFER_OVERFLOW: more links than fit, so more than one.
        if status == 0x8000_0005u32 as i32 {
            return Err(FsError::HardLink);
        }
        if status < 0 {
            return Ok(());
        }
        let entries = (buf[0] >> 32) as u32;
        let parent = buf[2] as i64;
        let len = (buf[3] as u32 as usize).min(255);
        // SAFETY: the name starts 28 bytes into the buffer and is `len` characters, within it.
        let wide = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>().add(28).cast::<u16>(), len) };
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(g.dir.as_raw_handle(), &mut info) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        let dir = ((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)) as i64;
        let own = String::from_utf16_lossy(wide).to_lowercase() == name.to_string_lossy().to_lowercase();
        if entries == 1 && parent == dir && own {
            Ok(())
        } else {
            Err(FsError::HardLink)
        }
    }

    pub fn link_count(f: &File, _: &std::fs::Metadata) -> Result<u64, FsError> {
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(info.nNumberOfLinks as u64)
    }

    /// Whether opening a file failed the way it does while the file is being
    /// deleted (deleted with a handle still open on it): ERROR_ACCESS_DENIED.
    /// A file whose access is denied for another reason fails the same way,
    /// so the caller looks again and takes the answer it then gets.
    pub fn may_be_deleting(e: &io::Error) -> bool {
        e.raw_os_error() == Some(5)
    }

    pub fn stamp(f: &File) -> io::Result<super::Stamp> {
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // 100-nanosecond intervals since 1601, read as `Metadata::modified` does.
        let ticks =
            (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32) | u64::from(info.ftLastWriteTime.dwLowDateTime);
        const EPOCH: u64 = 11_644_473_600;
        let secs = ticks / 10_000_000;
        let written = (secs >= EPOCH).then(|| (secs - EPOCH, ((ticks % 10_000_000) * 100) as u32));
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        // The change time (NTFS's ChangeTime, as Unix's ctime): the file
        // system sets it on every write and on every change of the file's
        // attributes, its write time included, so a rewrite of the same size
        // that puts the old write time back changes it.
        // SAFETY: `basic` is plain data of the size given, filled by the call; the handle is open.
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<FILE_BASIC_INFO>() as u32;
        let got = unsafe {
            GetFileInformationByHandleEx(
                f.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                size,
            )
        };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        let ticks = u64::try_from(basic.ChangeTime).unwrap_or(0);
        let changed = match (ticks / 10_000_000).checked_sub(EPOCH) {
            Some(secs) => (secs as i64, ((ticks % 10_000_000) * 100) as i64),
            None => (0, 0),
        };
        Ok(super::Stamp {
            file: super::FileId(u64::from(info.dwVolumeSerialNumber), index),
            len: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            written,
            changed,
            // Any reparse point counts as not a plain file here (its stamp is
            // then never taken for an earlier one's).
            is_file: info.dwFileAttributes & (ATTR_DIRECTORY | ATTR_REPARSE_POINT) == 0,
        })
    }

    pub fn stamp_path(path: &Path) -> io::Result<super::Stamp> {
        // No access to the content: only its attributes are read. Without
        // FILE_FLAG_BACKUP_SEMANTICS a directory is not opened.
        let f = OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)?;
        stamp(&f)
    }

    const DRIVE_REMOTE: u32 = 4;

    /// Whether the spelling of `p` leaves the machine: a UNC path (`\\host`,
    /// `\\?\UNC\host`), a device or object path (`\\.\`, `\\?\GLOBALROOT`,
    /// `\Device\Mup\host`), the same in the object manager's spellings that a
    /// link's target can have (`\??\`, `\GLOBAL??\`, `\DosDevices\`), or a
    /// path on a network drive. Nothing is opened.
    pub fn spelled_remote(p: &Path) -> bool {
        let s = p.as_os_str().to_string_lossy().replace('/', "\\").to_ascii_lowercase();
        // All of these lead to the names of the DOS devices, as `\\?\` does
        // (found by the 15th assessment: `\GLOBAL??\UNC\host` passed as local).
        let named = [r"\\?\", r"\??\", r"\global??\", r"\dosdevices\"];
        let rest = match named.iter().find_map(|n| s.strip_prefix(n)) {
            Some(rest) if rest.starts_with(r"unc\") => return true,
            // A local volume by its name (mount points lead there).
            Some(rest) if rest.starts_with("volume{") => return false,
            Some(rest) => rest,
            None if s.starts_with(r"\\") => return true,
            // `\Device\Mup\host\share`, `\Device\HarddiskVolume1\`...: devices.
            None if s.starts_with(r"\device\") => return true,
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

    /// Whether a link's `target` may leave the machine: as `spelled_remote`,
    /// and any target that starts with a single `\` and has no drive. That is
    /// an object manager's path (`\GLOBAL??\UNC\host\share`, `\Device\Mup\...`),
    /// or a folder of the drive the link is on: it is refused rather than
    /// guessed (found by the 15th assessment).
    pub fn remote_link_target(target: &Path) -> bool {
        let s = target.as_os_str().to_string_lossy();
        let mut c = s.chars();
        let rooted = matches!((c.next(), c.next()), (Some('\\' | '/'), second) if !matches!(second, Some('\\' | '/')));
        rooted || spelled_remote(target)
    }

    /// `p` with `.` and `..` taken out by its spelling, as Windows does with a
    /// name (and with a link's target, against the folder the link is in).
    fn lexical(p: &Path) -> PathBuf {
        use std::path::Component;
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                        out.pop();
                    } else if !out.has_root() {
                        out.push("..");
                    }
                }
                c => out.push(c.as_os_str()),
            }
        }
        out
    }

    /// `p` without the `\\?\` that `canonicalize` puts before a drive or a share.
    fn plain(p: PathBuf) -> PathBuf {
        use std::path::{Component, Prefix};
        let mut comps = p.components();
        let start = match comps.next() {
            Some(Component::Prefix(x)) => match x.kind() {
                Prefix::VerbatimDisk(d) => PathBuf::from(format!("{}:\\", d as char)),
                Prefix::VerbatimUNC(host, share) => {
                    let mut s = std::ffi::OsString::from(r"\\");
                    s.push(host);
                    s.push(r"\");
                    s.push(share);
                    s.push(r"\");
                    PathBuf::from(s)
                }
                _ => return p,
            },
            _ => return p,
        };
        let mut out = start;
        for c in comps {
            if !matches!(c, Component::RootDir) {
                out.push(c.as_os_str());
            }
        }
        out
    }

    /// See `super::on_another_machine_below`; `base` None: the whole path.
    pub fn on_another_machine(base: Option<&Path>, path: &Path) -> bool {
        use std::path::Component;
        let mut p = path.to_path_buf();
        let mut base: Option<PathBuf> = base.map(Path::to_path_buf);
        // Whether the base was taken by its real folder (at the first link).
        let mut rebased = false;
        // Whether a link was followed: after one, the path is our own spelling
        // of where Windows goes, and a part of it that cannot be read is refused
        // rather than passed (found by the 16th assessment).
        let mut followed = false;
        // As many links in a row as Windows follows.
        for _ in 0..63 {
            // The components of `base` are not looked at.
            let known = match &base {
                Some(b) if p.starts_with(b) => b.components().count(),
                _ => {
                    if spelled_remote(&p) {
                        return true;
                    }
                    0
                }
            };
            let comps: Vec<Component> = p.components().collect();
            let mut at = PathBuf::new();
            let mut next = None;
            for (i, c) in comps.iter().enumerate() {
                at.push(c.as_os_str());
                if i < known || !matches!(c, Component::Normal(_)) {
                    continue;
                }
                // Read without following it (FILE_FLAG_OPEN_REPARSE_POINT).
                match std::fs::symlink_metadata(&at) {
                    // A symbolic link or a junction: where it leads, then the rest.
                    Ok(m) if m.file_type().is_symlink() => {
                        let Ok(target) = std::fs::read_link(&at) else { return true };
                        if remote_link_target(&target) {
                            return true;
                        }
                        // At the first link, the base is taken by its real folder
                        // and the path looked at again from there.
                        if !rebased {
                            rebased = true;
                            if let Some((real, moved)) = base.as_deref().and_then(|b| real_base(b, &p)) {
                                if moved != p {
                                    base = Some(real);
                                    next = Some((moved, false));
                                    break;
                                }
                            }
                        }
                        let mut t = at.parent().unwrap_or(Path::new("")).join(target);
                        for rest in &comps[i + 1..] {
                            t.push(rest.as_os_str());
                        }
                        // The folders before the link are real (looked at, or the
                        // base's real folder), so `..` is taken out by spelling.
                        next = Some((lexical(&t), true));
                        break;
                    }
                    Ok(_) => {}
                    // A missing file is not refused (opening it fails); a part
                    // before it that cannot be read after a link is.
                    Err(_) => return followed && i + 1 < comps.len(),
                }
            }
            match next {
                Some((t, link)) => {
                    p = t;
                    followed |= link;
                }
                None => return false,
            }
        }
        true
    }

    /// The real folder of `base`, and `p` (below `base`) moved there; none for
    /// a base on another machine (resolving it would contact the host) or a `p`
    /// not below it. Found by the 16th assessment: Windows resolves a relative
    /// link's target against the folder the link is really in, so with a base
    /// reached through a link to a folder, joining it to the base as spelled
    /// led the check elsewhere (often to a missing folder, which passed).
    fn real_base(base: &Path, p: &Path) -> Option<(PathBuf, PathBuf)> {
        let b = std::path::absolute(base).ok()?;
        let rest = std::path::absolute(p).ok()?.strip_prefix(&b).ok()?.to_path_buf();
        if on_another_machine(None, &b) {
            return None;
        }
        let real = plain(b.canonicalize().ok()?);
        let moved = real.join(rest);
        Some((real, moved))
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

    /// Found by the 15th assessment: a link's target in the object manager's
    /// spellings (one `\`, no drive) was taken for a folder of this machine.
    #[cfg(windows)]
    #[test]
    fn object_manager_spellings_of_shares_are_remote() {
        use imp::{remote_link_target, spelled_remote};
        let remote = [
            r"\GLOBAL??\UNC\host\share\m.phv",
            r"\global??\unc\host\share",
            r"\DosDevices\UNC\host\share\m.phv",
            r"\??\UNC\host\share\m.phv",
            r"\Device\Mup\host\share\m.phv",
            r"\Device\LanmanRedirector\host\share",
            r"\Device\HarddiskVolume1\x",
            r"\GLOBAL??\GLOBALROOT\Device\Mup\host\share",
            r"\\?\UNC\host\share",
            r"\\host\share",
        ];
        for p in remote {
            assert!(spelled_remote(Path::new(p)), "{p}");
            assert!(remote_link_target(Path::new(p)), "{p}");
        }
        // A drive or a volume of this machine in the same spellings is local as
        // a name, but a link's target spelled so is refused all the same (reading
        // a link gives a local drive as `C:\`).
        let drive = std::env::temp_dir().to_string_lossy()[..2].to_string();
        for p in [format!(r"\GLOBAL??\{drive}\x"), format!(r"\DosDevices\{drive}\x"), format!(r"\??\{drive}\x")] {
            assert!(!spelled_remote(Path::new(&p)), "{p}");
            assert!(remote_link_target(Path::new(&p)), "{p}");
        }
        assert!(!spelled_remote(Path::new(r"\??\Volume{0e5ad7f2-0000-0000-0000-100000000000}\x")));
        // Any other target with one `\` and no drive is refused as a link's target.
        for p in [r"\Users\x\m.phv", r"\RPC Control\x", "/srv/m.phv"] {
            assert!(remote_link_target(Path::new(p)), "{p}");
        }
        for p in [r"..\outside\m.phv", r"lib\m.phv", r"C:\Users\x\m.phv"] {
            assert!(!remote_link_target(Path::new(p)), "{p}");
        }
        // Behind a link, without opening it (needs Developer Mode or the privilege).
        let d = tmp("ntlink");
        let link = d.join("data/m.phv");
        if std::os::windows::fs::symlink_file(r"\GLOBAL??\UNC\phv-test.invalid\share\m.phv", &link).is_ok() {
            assert!(on_another_machine(&link));
            assert!(on_another_machine_below(&d, &link));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `on_another_machine_below` looks only below the base, and at where links lead.
    #[cfg(windows)]
    #[test]
    fn only_the_part_below_the_base_is_looked_at() {
        let d = tmp("below");
        // The base itself may be anywhere (a project on a share): not looked at.
        assert!(!on_another_machine_below(
            Path::new(r"\\phv-test.invalid\share"),
            Path::new(r"\\phv-test.invalid\share\m.phv")
        ));
        assert!(on_another_machine_below(Path::new(r"C:\proj"), Path::new(r"\\phv-test.invalid\share\m.phv")));
        assert!(!on_another_machine_below(&d, &d.join("data/x.phv")));
        if std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\m.phv", d.join("data/m.phv")).is_ok() {
            assert!(on_another_machine_below(&d, &d.join("data/m.phv")));
            // A relative link that stays below the base is fine.
            std::os::windows::fs::symlink_file(r"..\outside\secret.txt", d.join("data/ok.txt")).unwrap();
            assert!(!on_another_machine_below(&d, &d.join("data/ok.txt")));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Found by the 16th assessment: with the base reached through a link to a
    /// directory, a relative link's target was joined to the base's spelling,
    /// while Windows resolves it against the link's real folder; the folder it
    /// named was missing, which passed as "not on another machine".
    #[cfg(windows)]
    #[test]
    fn a_base_reached_through_a_link_is_looked_at_by_its_real_folder() {
        use std::os::windows::fs::symlink_dir;
        let d = tmp("baselink");
        let proj = d.join("real").join("proj");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        // far -> a share; a -> ..\proj\far; work -> real\proj (the user's way in).
        if symlink_dir(r"\\phv-test.invalid\share\pkg", proj.join("far")).is_err() {
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        symlink_dir(r"..\proj\far", proj.join("a")).unwrap();
        symlink_dir(r"real\proj", d.join("work")).unwrap();
        let start = std::time::Instant::now();
        assert!(on_another_machine_below(&proj, &proj.join(r"a\m.phv")));
        assert!(on_another_machine_below(&d.join("work"), &d.join(r"work\a\m.phv")));
        assert!(on_another_machine_below(&d.join("work"), &d.join(r"work\.\a\m.phv")));
        assert!(on_another_machine(&d.join(r"work\a\m.phv")));
        // A folder that cannot be read after a link is refused; a missing file
        // through a link that stays here is not.
        symlink_dir(r"..\proj\missing\deeper", proj.join("gone")).unwrap();
        symlink_dir(r"..\proj\sub", proj.join("here")).unwrap();
        assert!(on_another_machine_below(&d.join("work"), &d.join(r"work\gone\x\m.phv")));
        assert!(!on_another_machine_below(&d.join("work"), &d.join(r"work\here\m.phv")));
        assert!(!on_another_machine_below(&d.join("work"), &d.join(r"work\sub\m.phv")));
        assert!(!on_another_machine_below(&d.join("work"), &d.join(r"work\missing\m.phv")));
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        for l in [d.join("work"), proj.join("a"), proj.join("far"), proj.join("gone"), proj.join("here")] {
            let _ = std::fs::remove_dir(l);
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

    /// A file rewritten in place with text of the same size, its old write
    /// time then put back, has another stamp: the change time (also on
    /// Windows, where it was left out) sees the rewrite. The loader's memo
    /// took the two for the same file and served the old page.
    /// Gives the existing empty directory `dir` a junction's reparse point
    /// to `target`, in place (what `mklink /J` does after making the
    /// directory): a handle held on the directory does not prevent it.
    #[cfg(windows)]
    fn make_junction_in_place(dir: &Path, target: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                device: *mut std::ffi::c_void,
                code: u32,
                input: *const std::ffi::c_void,
                input_len: u32,
                output: *mut std::ffi::c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        // FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        let Ok(h) = std::fs::OpenOptions::new().write(true).custom_flags(0x0200_0000 | 0x0020_0000).open(dir) else {
            return false;
        };
        let target = target.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let wide = |t: &str| std::ffi::OsStr::new(t).encode_wide().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        let (substitute, print) = (wide(&format!(r"\??\{target}")), wide(&target));
        let mut paths = substitute.clone();
        paths.extend([0, 0]);
        paths.extend(&print);
        paths.extend([0, 0]);
        // IO_REPARSE_TAG_MOUNT_POINT and its data: the lengths and offsets of the two names.
        let mut buf = 0xA000_0003u32.to_le_bytes().to_vec();
        for n in [paths.len() + 8, 0, 0, substitute.len(), substitute.len() + 2, print.len()] {
            buf.extend((n as u16).to_le_bytes());
        }
        buf.extend(&paths);
        let mut returned = 0u32;
        // SAFETY: the buffer is as long as said; the handle is open. FSCTL_SET_REPARSE_POINT.
        let done = unsafe {
            DeviceIoControl(
                h.as_raw_handle().cast(),
                0x900A4,
                buf.as_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        done != 0
    }

    /// A directory that becomes a junction while a guard holds it (an empty
    /// directory can be given a reparse point in place) does not lead any
    /// file function to the junction's target: every entry is opened,
    /// created, listed, moved and deleted through the directory's handle
    /// (21st assessment: files were opened by path, and `mk1` lines were
    /// appended to, and a file read and overwritten, outside the permitted
    /// directory).
    #[cfg(windows)]
    #[test]
    fn a_directory_turned_into_a_junction_does_not_lead_outside() {
        let d = tmp("junction-in-place");
        let (base, outside) = (d.join("data"), d.join("outside").canonicalize().unwrap());
        std::fs::write(outside.join("r.txt"), "secret2").unwrap();
        std::fs::write(outside.join("moved.txt"), "keep").unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        std::fs::write(base.join("other/mine.txt"), "mine").unwrap();
        // The guard is on the real, empty directory; then it becomes a junction.
        let (g, name) = open_parent(&base, Path::new("sub/r.txt")).unwrap();
        let (other, mine) = open_parent(&base, Path::new("other/mine.txt")).unwrap();
        if !make_junction_in_place(&base.join("sub"), &outside) {
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert_eq!(
            std::fs::read(base.join("sub/r.txt")).unwrap(),
            b"secret2",
            "by its path, the junction leads outside"
        );
        let names = |v: &[&str]| v.iter().map(|n| OsString::from(*n)).collect::<Vec<_>>();
        let unknown = names(&["new.txt", "newdir", "moved.txt"]);
        // Nothing of the target is seen through the guard, and nothing is made in it.
        assert!(matches!(g.exists(&name), Err(FsError::Link)), "the outside file is not seen");
        assert!(matches!(g.open(&name, false), Err(FsError::Link)), "read");
        assert!(matches!(g.plain_file_id(&name), Err(FsError::Link)));
        assert!(g.entries().is_ok_and(|e| e.is_empty()), "the outside names are not listed");
        assert!(g.remove(&name).is_err(), "delete");
        let _ = g.open(&name, true).map(|mut f| f.write_all(b"mk1w"));
        let _ = g.open_append(&name).map(|mut f| f.write_all(b"mk1"));
        let _ = g.open(&unknown[0], true).map(|mut f| f.write_all(b"mk1"));
        let _ = g.make_dir(&unknown[1]);
        assert!(other.rename_to(&mine, &g, &unknown[2]).is_err(), "a move into it");
        assert!(g.rename_to(&name, &other, &unknown[2]).is_err(), "a move out of it");
        assert_eq!(std::fs::read(outside.join("r.txt")).unwrap(), b"secret2");
        assert_eq!(std::fs::read(outside.join("moved.txt")).unwrap(), b"keep");
        assert_eq!(std::fs::read(base.join("other/mine.txt")).unwrap(), b"mine");
        let mut left: Vec<_> = std::fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
        left.sort();
        assert_eq!(left, names(&["moved.txt", "r.txt", "secret.txt"]), "nothing was added outside");
        drop((g, other));
        // A guard asked for afterwards refuses the junction.
        assert!(matches!(open_parent(&base, Path::new("sub/r.txt")), Err(FsError::Link)));
        let _ = std::fs::remove_dir(base.join("sub"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// On Unix every entry is reached from its directory's descriptor: a
    /// directory swapped for a link to another one while a guard holds it
    /// leads nothing there (the guard stays on the directory it opened), and
    /// a guard asked for afterwards refuses the link (checked for the 21st
    /// assessment, which found the Windows side opening by path).
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_symlink_does_not_lead_outside() {
        let d = tmp("swapped");
        let (base, outside) = (d.join("data"), d.join("outside"));
        std::fs::write(outside.join("r.txt"), "secret2").unwrap();
        let (g, name) = open_parent(&base, Path::new("sub/r.txt")).unwrap();
        std::fs::rename(base.join("sub"), base.join("was-sub")).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("sub")).unwrap();
        assert_eq!(std::fs::read(base.join("sub/r.txt")).unwrap(), b"secret2", "by its path, the link leads outside");
        assert!(!g.exists(&name).unwrap());
        assert!(g.open(&name, false).is_err());
        assert!(g.entries().unwrap().is_empty());
        g.open(&name, true).unwrap().write_all(b"mk1w").unwrap();
        g.open_append(&name).unwrap().write_all(b"mk1").unwrap();
        g.make_dir(OsStr::new("newdir")).unwrap();
        assert_eq!(std::fs::read(outside.join("r.txt")).unwrap(), b"secret2");
        assert_eq!(std::fs::read(base.join("was-sub/r.txt")).unwrap(), b"mk1wmk1");
        let mut left: Vec<_> = std::fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
        left.sort();
        assert_eq!(left, [OsString::from("r.txt"), OsString::from("secret.txt")], "nothing was added outside");
        drop(g);
        assert!(matches!(open_parent(&base, Path::new("sub/r.txt")), Err(FsError::Link)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The file functions on an ordinary directory, all through its handle:
    /// make, list, move (also over a file, and to another directory), delete.
    #[test]
    fn entries_are_made_listed_moved_and_deleted() {
        let d = tmp("entries");
        let base = d.join("data");
        let (g, a) = open_parent(&base, Path::new("sub/a.txt")).unwrap();
        let (top, b) = open_parent(&base, Path::new("b.txt")).unwrap();
        let os = |n: &str| OsString::from(n);
        assert!(g.entries().unwrap().is_empty());
        g.open(&a, true).unwrap().write_all(b"one").unwrap();
        g.make_dir(&os("inner")).unwrap();
        g.make_dir(&os("inner")).unwrap();
        assert!(g.make_dir(&a).is_err(), "a file of that name");
        assert_eq!(g.entries().unwrap(), ["a.txt", "inner"]);
        // Many entries: more than one buffer of names.
        for i in 0..600 {
            g.open(&os(&format!("file-with-a-rather-long-name-{i:04}.txt")), true).unwrap();
        }
        assert_eq!(g.entries().unwrap().len(), 602);
        for i in 0..600 {
            g.remove(&os(&format!("file-with-a-rather-long-name-{i:04}.txt"))).unwrap();
        }
        // A move to another directory, then over an existing file.
        top.open(&b, true).unwrap().write_all(b"old").unwrap();
        g.rename_to(&a, &top, &os("c.txt")).unwrap();
        assert_eq!(std::fs::read(base.join("c.txt")).unwrap(), b"one");
        top.rename_to(&os("c.txt"), &top, &b).unwrap();
        assert_eq!(std::fs::read(base.join("b.txt")).unwrap(), b"one");
        assert_eq!(top.entries().unwrap(), ["b.txt", "sub"]);
        assert_eq!(g.entries().unwrap(), ["inner"]);
        assert!(g.remove(&a).is_err(), "it is gone");
        assert!(top.remove(&os("sub")).is_err(), "a directory is not deleted as a file");
        top.remove(&b).unwrap();
        assert!(!top.exists(&b).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A name that was a hard link to a file elsewhere when it was opened,
    /// and is removed right after, leaves that file with one link: the count
    /// alone would let the write through (21st assessment). The opened file
    /// must have its one link here, under the name it was opened by.
    #[test]
    fn a_hard_link_removed_after_the_open_is_still_refused() {
        let d = tmp("hard-then-gone");
        let base = d.join("data");
        if std::fs::hard_link(d.join("outside/secret.txt"), base.join("hard.txt")).is_err() {
            return;
        }
        let (g, name) = open_parent(&base, Path::new("hard.txt")).unwrap();
        let f = imp::open_append(&g, &name).unwrap();
        assert!(matches!(g.check_own(&f, &name), Err(FsError::HardLink)), "two links");
        // The link in the permitted directory goes: one link is left, the one outside.
        std::fs::remove_file(base.join("hard.txt")).unwrap();
        assert!(check_plain(&f).is_ok(), "the count alone lets it through");
        assert!(matches!(g.check_own(&f, &name), Err(FsError::HardLink)));
        // Another file takes the name: still not the one that was opened.
        std::fs::write(base.join("hard.txt"), "new").unwrap();
        assert!(matches!(g.check_own(&f, &name), Err(FsError::HardLink)));
        drop(f);
        // A plain file is its own, also under another case of its name on Windows.
        let f = imp::open_append(&g, &name).unwrap();
        assert!(g.check_own(&f, &name).is_ok());
        drop(f);
        g.open_append(&name).unwrap().write_all(b"+more").unwrap();
        assert_eq!(std::fs::read(base.join("hard.txt")).unwrap(), b"new+more");
        // A link made to it elsewhere afterwards is seen.
        if std::fs::hard_link(base.join("hard.txt"), d.join("outside/copy.txt")).is_ok() {
            assert!(matches!(g.open_append(&name), Err(FsError::HardLink)));
            assert!(matches!(g.open(&name, false), Err(FsError::HardLink)));
        }
        assert_eq!(std::fs::read_to_string(d.join("outside/secret.txt")).unwrap(), "secret");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rewrite_with_the_old_write_time_changes_the_stamp() {
        let d = tmp("stamp");
        let path = d.join("data/page.phv");
        std::fs::write(&path, "version-A").unwrap();
        let old = std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = stamp_path(&path).unwrap();
        assert_eq!(stamp_path(&path).unwrap(), before);
        // NTFS keeps the change time to 100 ns, ext4 to 1 ns; other file
        // systems more coarsely.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"version-B").unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let after = stamp_path(&path).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified(), before.modified());
        assert_ne!(after, before, "the same stamp after a rewrite");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Writers appending to one file at once each add their text whole: the
    /// operating system puts each write at the end (found by the 21st
    /// assessment: 16 clients of a page that logs a line lost 1,820 of 4,000
    /// lines and cut 684, the file being positioned at its end once). An
    /// append is checked as any other write: a hard link, a link and a
    /// directory are refused, and nothing already in the file is replaced.
    #[test]
    fn appends_made_at_once_are_whole() {
        let d = tmp("append");
        let base = d.join("data");
        let (g, name) = open_parent(&base, Path::new("sub/log.txt")).unwrap();
        g.open(&name, true).unwrap().write_all(b"first\n").unwrap();
        // A handle opened before others wrote still appends after them.
        let mut early = g.open_append(&name).unwrap();
        g.open_append(&name).unwrap().write_all(b"second\n").unwrap();
        early.write_all(b"third\n").unwrap();
        drop(early);
        assert_eq!(std::fs::read_to_string(base.join("sub/log.txt")).unwrap(), "first\nsecond\nthird\n");
        let (threads, each) = (16, 250);
        std::thread::scope(|scope| {
            for t in 0..threads {
                let base = &base;
                scope.spawn(move || {
                    for i in 0..each {
                        let (g, name) = open_parent(base, Path::new("sub/log.txt")).unwrap();
                        let line = format!("writer {t} line {i} {}\n", "x".repeat(i % 40));
                        g.open_append(&name).unwrap().write_all(line.as_bytes()).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(base.join("sub/log.txt")).unwrap();
        let lines: std::collections::HashSet<&str> = text.lines().collect();
        assert_eq!(text.lines().count(), 3 + threads * each, "a line was lost or cut");
        for t in 0..threads {
            for i in 0..each {
                let line = format!("writer {t} line {i} {}", "x".repeat(i % 40));
                assert!(lines.contains(line.as_str()), "{line:?} is missing");
            }
        }
        // The checks of a write hold for an append.
        let (g, sub) = open_parent(&base, Path::new("sub")).unwrap();
        assert!(matches!(g.open_append(&sub), Err(FsError::NotFile)));
        if std::fs::hard_link(d.join("outside/secret.txt"), base.join("hard.txt")).is_ok() {
            let (g, name) = open_parent(&base, Path::new("hard.txt")).unwrap();
            assert!(matches!(g.open_append(&name), Err(FsError::HardLink)));
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("outside/secret.txt"), base.join("link.txt")).unwrap();
            let (g, name) = open_parent(&base, Path::new("link.txt")).unwrap();
            assert!(matches!(g.open_append(&name), Err(FsError::Link)));
        }
        assert!(matches!(open_parent(&base, Path::new("../outside/secret.txt")), Err(FsError::BadName)));
        assert_eq!(std::fs::read_to_string(d.join("outside/secret.txt")).unwrap(), "secret");
        let _ = std::fs::remove_dir_all(&d);
    }
}
