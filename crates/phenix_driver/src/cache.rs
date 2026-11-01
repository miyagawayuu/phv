//! On-disk bytecode cache (`.phenix-cache/` in the project root), so a script
//! is not re-parsed and re-checked when nothing changed.
//!
//! An entry is used only if it was written by the same `phenix` binary and
//! every source file still has the same modification time. The cache has the
//! same trust level as the source files next to it.

use crate::{compile, CompileError, Compiled};
use phenix_vm::Program;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Entry {
    stamp: String,
    deps: Vec<(String, Option<(u64, u32)>)>,
    warnings: String,
    entry_file: u32,
    program: Program,
}

fn to_pair(t: Option<SystemTime>) -> Option<(u64, u32)> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| (d.as_secs(), d.subsec_nanos()))
}

/// Identifies the running binary, so a rebuilt `phenix` never reads old bytecode.
fn stamp() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
    let (len, mtime) = exe.map(|m| (m.len(), to_pair(m.modified().ok()))).unwrap_or((0, None));
    format!("{}-{len}-{mtime:?}", env!("CARGO_PKG_VERSION"))
}

fn cache_path(root: &Path, entry: &Path) -> PathBuf {
    let canonical = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut h);
    root.join(".phenix-cache").join(format!("{:016x}.bin", h.finish()))
}

fn load(path: &Path, stamp: &str) -> Option<Compiled> {
    let bytes = std::fs::read(path).ok()?;
    let e: Entry = postcard::from_bytes(&bytes).ok()?;
    if e.stamp != stamp {
        return None;
    }
    let mtime = |p: &str| to_pair(std::fs::metadata(p).and_then(|m| m.modified()).ok());
    if e.deps.iter().any(|(p, t)| mtime(p) != *t) {
        return None;
    }
    let builtins = phenix_std::link(&e.program.builtin_names).ok()?;
    let deps = e
        .deps
        .iter()
        .map(|(p, t)| (PathBuf::from(p), t.map(|(s, n)| UNIX_EPOCH + std::time::Duration::new(s, n))))
        .collect();
    Some(Compiled {
        program: Arc::new(e.program),
        builtins: builtins.into(),
        deps,
        warnings: e.warnings,
        entry_file: e.entry_file,
        jit: None,
        from_cache: true,
    })
}

/// Like `compile`, but reuses (and refreshes) the on-disk cache.
pub fn compile_cached(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    let path = cache_path(root, entry);
    let stamp = stamp();
    if let Some(c) = load(&path, &stamp) {
        return Ok(c);
    }
    let c = compile(root, entry)?;
    let e = Entry {
        stamp,
        deps: c.deps.iter().map(|(p, t)| (p.to_string_lossy().into_owned(), to_pair(*t))).collect(),
        warnings: c.warnings.clone(),
        entry_file: c.entry_file,
        program: clone_program(&c.program),
    };
    if let Ok(bytes) = postcard::to_allocvec(&e) {
        // Best effort: write to a temporary file, then rename into place.
        if std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
            let _ = std::fs::write(path.parent().unwrap().join(".gitignore"), "*\n");
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    Ok(c)
}

fn clone_program(p: &Program) -> Program {
    // Program is serializable but not Clone; round-trip through the encoder.
    postcard::from_bytes(&postcard::to_allocvec(p).expect("encode")).expect("decode")
}
