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
/// trailing dots and spaces (`a.phx.` opens `a.phx`), treats `name:stream` as
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
/// comes from phenix.toml); nothing below it may be.
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
        let f = imp::open_file(self, name, false)?;
        check_plain(&f)
    }

    pub fn remove(&self, name: &OsStr) -> Result<(), FsError> {
        if !self.exists(name)? {
            return Err(FsError::Io(io::ErrorKind::NotFound.into()));
        }
        imp::remove(self, name)
    }
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
}

#[cfg(windows)]
mod imp {
    use super::{DirGuard, FsError};
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
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
        open_dir(&path.join(name), false)
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
        Ok(f)
    }

    pub fn exists(g: &DirGuard, name: &OsStr) -> Result<bool, FsError> {
        match std::fs::symlink_metadata(g.path.join(name)) {
            Ok(m) if m.file_attributes() & ATTR_REPARSE_POINT != 0 => Err(FsError::Link),
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(FsError::Io(e)),
        }
    }

    pub fn remove(g: &DirGuard, name: &OsStr) -> Result<(), FsError> {
        Ok(std::fs::remove_file(g.path.join(name))?)
    }

    pub fn link_count(f: &File, _: &std::fs::Metadata) -> Result<u64, FsError> {
        // SAFETY: `info` is plain data filled by the call; the handle is open.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
            return Err(FsError::Io(io::Error::last_os_error()));
        }
        Ok(info.nNumberOfLinks as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phenix-safefs-{name}-{}", std::process::id()));
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
        for bad in ["a.phx.", "a ", "a:b", "x.txt::$DATA", "CON", "nul.txt", "COM1", "lpt9.log", "a\\b", "", ".."] {
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
