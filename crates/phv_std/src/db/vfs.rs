//! The SQLite VFS for database files: the default VFS, plus a check of each
//! file it opens by name.
//!
//! `Database::open` checks the database's names once, but SQLite creates its
//! journal (`-journal`, `-wal`) whenever a transaction starts, and the Windows
//! VFS follows links (`SQLITE_OPEN_NOFOLLOW` is only honoured on Unix). So
//! every named file is checked right after it is opened, through its handle
//! (`safefs::check_opened`), and closed again if it is not a plain file
//! reached by its own name. No I/O is changed: the file methods forward to
//! the default VFS's file.
//!
//! The `-shm` file is opened by the default VFS itself, not through `xOpen`,
//! so before the first `xShmMap` it is checked, and on Windows held open
//! meanwhile so its name cannot be replaced (SQLite's own handle then pins it).
//!
//! On Unix SQLite opens every file with `O_NOFOLLOW` and its descriptor is
//! not reachable, so the name is checked for hard links right after the open.

use crate::safefs;
use rusqlite::ffi;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

pub(super) const NAME: &str = "phv";

/// The default VFS that this one wraps.
static BASE: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(ptr::null_mut());

/// Registers the VFS (once). False if SQLite has no default VFS.
pub(super) fn register() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| {
        // Before SQLite is first used (`sqlite3_vfs_find` starts it).
        super::mem::init();
        // SAFETY (this module): calls into SQLite's C API with pointers it
        // handed out. `File` is laid out over memory SQLite allocates
        // (`szOsFile` bytes), followed by the default VFS's own file.
        unsafe {
            let base = ffi::sqlite3_vfs_find(ptr::null());
            if base.is_null() {
                return false;
            }
            BASE.store(base, Ordering::Release);
            // Every other method (and `pAppData`, which they read) stays the default VFS's.
            let mut vfs = *base;
            vfs.zName = c"phv".as_ptr();
            vfs.szOsFile = (size_of::<File>() + (*base).szOsFile as usize) as c_int;
            vfs.pNext = ptr::null_mut();
            vfs.xOpen = Some(open);
            ffi::sqlite3_vfs_register(Box::leak(Box::new(vfs)), 0) == ffi::SQLITE_OK
        }
    })
}

#[repr(C)]
struct File {
    /// What SQLite sees: `pMethods` is one of `METHODS`.
    base: ffi::sqlite3_file,
    /// The default VFS's file, right after this struct.
    real: *mut ffi::sqlite3_file,
    /// The main database's name, for its `-shm` file.
    db: Option<PathBuf>,
    /// Whether the default VFS has the `-shm` file open.
    shm_open: bool,
}

fn check(real: *mut ffi::sqlite3_file, path: &Path) -> Result<(), safefs::FsError> {
    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::{FromRawHandle, RawHandle};
        let mut h: RawHandle = ptr::null_mut();
        let get = (*(*real).pMethods).xFileControl.ok_or(safefs::FsError::NotFile)?;
        if get(real, ffi::SQLITE_FCNTL_WIN32_GET_HANDLE, (&raw mut h).cast()) != ffi::SQLITE_OK || h.is_null() {
            return Err(safefs::FsError::NotFile);
        }
        // The handle stays SQLite's: borrowed here, never closed.
        let f = std::mem::ManuallyDrop::new(std::fs::File::from_raw_handle(h));
        safefs::check_opened(&f, path)
    }
    #[cfg(unix)]
    {
        let _ = real;
        safefs::check_path(path)
    }
}

unsafe extern "C" fn open(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    unsafe {
        let base = BASE.load(Ordering::Acquire);
        let real = file.cast::<u8>().add(size_of::<File>()).cast::<ffi::sqlite3_file>();
        // SQLite calls xClose only when pMethods is set.
        (*file).pMethods = ptr::null();
        (*real).pMethods = ptr::null();
        let close_real = move || {
            if let Some(close) = (*real).pMethods.as_ref().and_then(|m| m.xClose) {
                close(real);
            }
        };
        let rc = (*base).xOpen.unwrap()(base, name, real, flags, out_flags);
        if rc != ffi::SQLITE_OK {
            close_real();
            return rc;
        }
        // Temporary files have no name (and are deleted on close).
        let path = if name.is_null() {
            None
        } else {
            let path = CStr::from_ptr(name).to_str().ok().map(PathBuf::from);
            if !path.as_deref().is_some_and(|p| check(real, p).is_ok()) {
                close_real();
                return ffi::SQLITE_CANTOPEN;
            }
            path
        };
        let version = (*(*real).pMethods).iVersion.clamp(1, 3);
        ptr::write(
            file.cast::<File>(),
            File {
                base: ffi::sqlite3_file { pMethods: &METHODS[version as usize - 1] },
                real,
                db: path.filter(|_| flags & ffi::SQLITE_OPEN_MAIN_DB != 0),
                shm_open: false,
            },
        );
        ffi::SQLITE_OK
    }
}

/// The file methods of each version (the default VFS's file decides which).
static METHODS: [ffi::sqlite3_io_methods; 3] = [methods(1), methods(2), methods(3)];

const fn methods(version: c_int) -> ffi::sqlite3_io_methods {
    ffi::sqlite3_io_methods {
        iVersion: version,
        xClose: Some(close),
        xRead: Some(read),
        xWrite: Some(write),
        xTruncate: Some(truncate),
        xSync: Some(sync),
        xFileSize: Some(file_size),
        xLock: Some(lock),
        xUnlock: Some(unlock),
        xCheckReservedLock: Some(check_reserved_lock),
        xFileControl: Some(file_control),
        xSectorSize: Some(sector_size),
        xDeviceCharacteristics: Some(device_characteristics),
        xShmMap: if version >= 2 { Some(shm_map) } else { None },
        xShmLock: if version >= 2 { Some(shm_lock) } else { None },
        xShmBarrier: if version >= 2 { Some(shm_barrier) } else { None },
        xShmUnmap: if version >= 2 { Some(shm_unmap) } else { None },
        xFetch: if version >= 3 { Some(fetch) } else { None },
        xUnfetch: if version >= 3 { Some(unfetch) } else { None },
    }
}

/// Defines a method that calls the same method of the default VFS's file.
macro_rules! forward {
    ($name:ident, $method:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file $(, $arg: $ty)*) -> $ret {
            unsafe {
                let real = (*file.cast::<File>()).real;
                (*(*real).pMethods).$method.unwrap()(real $(, $arg)*)
            }
        }
    };
}

forward!(read, xRead, (buf: *mut c_void, n: c_int, offset: i64) -> c_int);
forward!(write, xWrite, (buf: *const c_void, n: c_int, offset: i64) -> c_int);
forward!(truncate, xTruncate, (size: i64) -> c_int);
forward!(sync, xSync, (flags: c_int) -> c_int);
forward!(file_size, xFileSize, (size: *mut i64) -> c_int);
forward!(lock, xLock, (level: c_int) -> c_int);
forward!(unlock, xUnlock, (level: c_int) -> c_int);
forward!(check_reserved_lock, xCheckReservedLock, (out: *mut c_int) -> c_int);
forward!(file_control, xFileControl, (op: c_int, arg: *mut c_void) -> c_int);
forward!(sector_size, xSectorSize, () -> c_int);
forward!(device_characteristics, xDeviceCharacteristics, () -> c_int);
forward!(shm_lock, xShmLock, (offset: c_int, n: c_int, flags: c_int) -> c_int);
forward!(shm_barrier, xShmBarrier, () -> ());
forward!(fetch, xFetch, (offset: i64, n: c_int, out: *mut *mut c_void) -> c_int);
forward!(unfetch, xUnfetch, (offset: i64, p: *mut c_void) -> c_int);
forward!(base_close, xClose, () -> c_int);
forward!(base_shm_map, xShmMap, (region: c_int, size: c_int, extend: c_int, out: *mut *mut c_void) -> c_int);
forward!(base_shm_unmap, xShmUnmap, (delete: c_int) -> c_int);

unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let rc = base_close(file);
        ptr::drop_in_place(file.cast::<File>());
        rc
    }
}

unsafe extern "C" fn shm_map(
    file: *mut ffi::sqlite3_file,
    region: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        let f = file.cast::<File>();
        if (*f).shm_open {
            return base_shm_map(file, region, size, extend, out);
        }
        // The default VFS is about to open `<db>-shm` by name.
        let Some(db) = &(*f).db else { return ffi::SQLITE_IOERR_SHMOPEN };
        let mut shm = db.clone().into_os_string();
        shm.push("-shm");
        let shm = PathBuf::from(shm);
        #[cfg(windows)]
        let Ok(_held) = safefs::hold(&shm) else {
            return ffi::SQLITE_IOERR_SHMOPEN;
        };
        #[cfg(unix)]
        match safefs::check_path(&shm) {
            Err(safefs::FsError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return ffi::SQLITE_IOERR_SHMOPEN,
            Ok(()) => {}
        }
        let rc = base_shm_map(file, region, size, extend, out);
        (*f).shm_open = rc == ffi::SQLITE_OK;
        rc
    }
}

unsafe extern "C" fn shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        (*file.cast::<File>()).shm_open = false;
        base_shm_unmap(file, delete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, OpenFlags};

    /// Opens `data/app.db` in a fresh directory the way `Database::open` does.
    fn setup(tag: &str) -> (PathBuf, safefs::DirGuard, Connection) {
        let d = std::env::temp_dir().join(format!("phv-vfs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/secret.txt"), "secret").unwrap();
        let (g, name) = safefs::open_parent(&d.join("data"), Path::new("app.db")).unwrap();
        assert!(register());
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let c = Connection::open_with_flags_and_vfs(g.path().join(name), flags, NAME).unwrap();
        // The journal is created and deleted again.
        c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1)").unwrap();
        (d, g, c)
    }

    fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        return std::os::windows::fs::symlink_file(target, link);
    }

    /// Plants each kind of link at `suffix` and runs `write`, which must fail
    /// without touching the link's target; with the link gone it works again.
    fn refuses_links_at(tag: &str, suffix: &str, wal: bool) {
        for kind in ["symlink", "hard link"] {
            let (d, g, c) = setup(&format!("{tag}-{}", kind.len()));
            if wal {
                c.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())).unwrap();
            }
            drop(c);
            let secret = d.join("outside/secret.txt");
            let at = g.path().join(format!("app.db{suffix}"));
            let _ = std::fs::remove_file(&at);
            let planted = match kind {
                "symlink" => symlink(&secret, &at),
                _ => std::fs::hard_link(&secret, &at),
            };
            if let Err(e) = planted {
                eprintln!("skipped: cannot create a {kind} ({e}); on Windows enable Developer Mode");
                continue;
            }
            let c = Connection::open_with_flags_and_vfs(g.path().join("app.db"), OpenFlags::default(), NAME).unwrap();
            let r = c.execute("INSERT INTO t VALUES (2)", []);
            assert!(r.is_err(), "{kind} at app.db{suffix} was used");
            assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret", "{kind} at app.db{suffix}");
            std::fs::remove_file(&at).unwrap();
            c.execute("INSERT INTO t VALUES (3)", []).unwrap();
            drop((c, g));
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn refuses_links_at_the_journal() {
        refuses_links_at("journal", "-journal", false);
    }

    #[test]
    fn refuses_links_at_the_wal_index() {
        refuses_links_at("shm", "-shm", true);
    }
}
