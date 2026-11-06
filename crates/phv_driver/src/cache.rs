//! On-disk bytecode cache (`.phv-cache/` in the project root), so a script
//! is not re-parsed and re-checked when nothing changed.
//!
//! The cache must never run code that is not in the source next to it, even
//! if the project came from someone else (a cloned repository can contain a
//! `.phv-cache/` of its own). So an entry is used only if:
//! - it carries a valid HMAC made with this user's key, which lives outside
//!   every project (`%LOCALAPPDATA%\phv\cache.key`, `~/.cache/phv/cache.key`);
//! - it was built for this entry file in this project, by this `phv` binary;
//! - every source file still has the content (SHA-256) it was compiled from.
//!
//! Files are read and written through `safefs`, so links planted in
//! `.phv-cache/` are not followed.

use crate::{compile, CompileError, Compiled, Dep};
use hmac::{Hmac, Mac};
use phv_std::safefs;
use phv_vm::Program;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

/// A cache file. It is written from a borrowed program (`P` = `&Program`):
/// the same bytes, without copying the program first.
#[derive(Serialize, Deserialize)]
struct Entry<P = Program> {
    stamp: String,
    /// Canonical paths of the entry script and the project root.
    entry: String,
    root: String,
    deps: Vec<Dep>,
    warnings: String,
    entry_file: u32,
    marked_tests: Vec<(u32, Option<String>)>,
    #[serde(default)]
    test_hooks: Vec<(u32, bool)>,
    program: P,
}

/// The first fields of an `Entry`, read first: an entry another binary (or
/// for another script) wrote is not decoded whole.
#[derive(Deserialize)]
struct Head {
    stamp: String,
    entry: String,
    root: String,
}

/// Cache files larger than this are not read.
const MAX_ENTRY: u64 = 256 * 1024 * 1024;
const MAC_LEN: usize = 32;

/// Identifies the running binary, so a rebuilt `phv` never reads old bytecode.
fn stamp() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
    let mtime = exe.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    format!("{}-{}-{mtime:?}", env!("CARGO_PKG_VERSION"), exe.map(|m| m.len()).unwrap_or(0))
}

fn key_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("phv").join("cache.key"))
}

/// This user's secret for signing cache entries, created on first use. None
/// (no usable location) turns the disk cache off.
fn key() -> Option<&'static [u8; 32]> {
    static KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    KEY.get_or_init(|| {
        let path = key_path()?;
        let read = |p: &Path| -> Option<[u8; 32]> {
            let meta = std::fs::symlink_metadata(p).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // Readable by others would let them sign entries.
                if meta.mode() & 0o077 != 0 {
                    return None;
                }
            }
            if !meta.is_file() || meta.len() != 32 {
                return None;
            }
            std::fs::read(p).ok()?.try_into().ok()
        };
        if let Some(k) = read(&path) {
            return Some(k);
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).ok()?;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        match o.open(&path) {
            Ok(mut f) => f.write_all(&k).ok().map(|_| k),
            // Another process created it first.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(&path),
            Err(_) => None,
        }
    })
    .as_ref()
}

fn mac(key: &[u8; 32], data: &[u8]) -> [u8; MAC_LEN] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(b"phv-cache-v1:");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn file_name(entry: &Path) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf()).hash(&mut h);
    format!("{:016x}.bin", h.finish())
}

fn canonical(p: &Path) -> Option<String> {
    p.canonicalize().ok().map(|c| c.to_string_lossy().into_owned())
}

fn load(root: &Path, entry: &Path, key: &[u8; 32], stamp: &str) -> Option<Compiled> {
    let name = file_name(entry);
    let (dir, file) = safefs::open_parent(root, &Path::new(".phv-cache").join(&name)).ok()?;
    let f = dir.open(&file, false).ok()?;
    let mut bytes = Vec::new();
    f.take(MAX_ENTRY + MAC_LEN as u64).read_to_end(&mut bytes).ok()?;
    let body_len = bytes.len().checked_sub(MAC_LEN)?;
    let (body, tag) = bytes.split_at(body_len);
    if !phv_std::constant_time_eq(&mac(key, body), tag) {
        return None;
    }
    let (head, _) = postcard::take_from_bytes::<Head>(body).ok()?;
    if head.stamp != stamp {
        return None;
    }
    let e: Entry = postcard::from_bytes(body).ok()?;
    // A file that has become a link to another machine (or lies behind one) is
    // not resolved or read: that would contact the host. It is compiled again,
    // which refuses it.
    if e.deps.iter().any(|d| phv_std::safefs::on_another_machine_below(root, &d.path)) {
        return None;
    }
    let here = (canonical(entry)?, canonical(root)?);
    if (head.entry.as_str(), head.root.as_str()) != (here.0.as_str(), here.1.as_str()) {
        return None;
    }
    // The entry script must be among the dependencies, and every dependency
    // must still have the content it was compiled from.
    if !e.deps.iter().any(|d| canonical(&d.path).as_deref() == Some(here.0.as_str())) {
        return None;
    }
    if !e.deps.iter().all(|d| d.is_fresh(true)) {
        return None;
    }
    let builtins = phv_std::link(&e.program.builtin_names).ok()?;
    Some(Compiled {
        program: Arc::new(e.program),
        builtins: builtins.into(),
        deps: e.deps,
        warnings: e.warnings,
        entry_file: e.entry_file,
        jit: None,
        from_cache: true,
        marked_tests: e.marked_tests,
        test_hooks: e.test_hooks,
    })
}

/// Best effort: a temporary file in `.phv-cache/`, then renamed into place.
fn store(root: &Path, entry: &Path, key: &[u8; 32], e: &Entry<&Program>) -> Option<()> {
    // An entry `load` would not read is not written (found by the 1.2.0 audit: a
    // 1 GB entry was written at every run); encoding stops past the size.
    let body = postcard::to_io(e, Limited(Vec::new(), MAX_ENTRY)).ok()?.0;
    let dir_path = root.join(".phv-cache");
    match std::fs::symlink_metadata(&dir_path) {
        Ok(m) if !m.is_dir() || m.file_type().is_symlink() => return None,
        Ok(_) => {}
        Err(_) => std::fs::create_dir(&dir_path).ok()?,
    }
    let (dir, ignore) = safefs::open_parent(root, Path::new(".phv-cache/.gitignore")).ok()?;
    if !dir.exists(&ignore).ok()? {
        dir.open(&ignore, true).ok()?.write_all(b"*\n").ok()?;
    }
    let name = file_name(entry);
    let tmp = format!(".{name}.{}.tmp", phv_std::random_hex(8));
    let mut f = dir.open(std::ffi::OsStr::new(&tmp), true).ok()?;
    let ok = f.write_all(&body).and_then(|_| f.write_all(&mac(key, &body))).is_ok();
    drop(f);
    let (from, to) = (dir.path().join(&tmp), dir.path().join(&name));
    if !ok || std::fs::rename(&from, &to).is_err() {
        let _ = std::fs::remove_file(&from);
    }
    Some(())
}

/// A buffer that refuses to grow past a number of bytes.
struct Limited(Vec<u8>, u64);

impl Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if (self.0.len() + buf.len()) as u64 > self.1 {
            return Err(std::io::Error::other("cache entry too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Like `compile`, but reuses (and refreshes) the on-disk cache.
pub fn compile_cached(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    let Some(key) = key() else { return compile(root, entry) };
    // An entry behind a link to another machine is not resolved here; `compile` refuses it.
    if phv_std::safefs::on_another_machine_below(root, entry) {
        return compile(root, entry);
    }
    let stamp = stamp();
    if let Some(c) = load(root, entry, key, &stamp) {
        return Ok(c);
    }
    let c = compile(root, entry)?;
    if let (Some(entry_c), Some(root_c)) = (canonical(entry), canonical(root)) {
        let e = Entry {
            stamp,
            entry: entry_c,
            root: root_c,
            deps: c.deps.clone(),
            warnings: c.warnings.clone(),
            entry_file: c.entry_file,
            marked_tests: c.marked_tests.clone(),
            test_hooks: c.test_hooks.clone(),
            program: c.program.as_ref(),
        };
        store(root, entry, key, &e);
    }
    Ok(c)
}

pub(crate) fn clone_program(p: &Program) -> Program {
    // Program is serializable but not Clone; round-trip through the encoder.
    postcard::from_bytes(&postcard::to_allocvec(p).expect("encode")).expect("decode")
}

#[cfg(test)]
mod tests {
    /// An entry larger than `load` reads is not encoded whole (the 1.2.0 audit:
    /// a 1 GB entry was written at every run, and never read).
    #[test]
    fn entries_past_the_size_are_not_encoded() {
        let big = vec![7u8; 5000];
        assert!(postcard::to_io(&big, super::Limited(Vec::new(), 4096)).is_err());
        let small = vec![7u8; 4000];
        let out = postcard::to_io(&small, super::Limited(Vec::new(), 4096)).unwrap();
        assert_eq!(out.0, postcard::to_allocvec(&small).unwrap());
    }
}
