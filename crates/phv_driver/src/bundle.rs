//! `phv build`: a project compiled ahead of time and appended to a copy of the
//! phv executable, which then runs it without the sources or a compiler.
//!
//! The executable is followed by the bundle (postcard), its SHA-256, its
//! length (8 bytes, little-endian) and `MAGIC`. At startup phv looks for the
//! trailer; a bundle that is damaged, or was built by another version of phv,
//! is refused.

use crate::{compile, read_source, CompileError, Compiled};
use phv_vm::Program;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"PHVBNDL1";
/// SHA-256, length and magic.
const TRAILER: u64 = 32 + 8 + 8;
/// Static files larger than this are not bundled (the server would not serve them either).
const MAX_STATIC: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Bundle {
    /// The phv version that built it.
    pub version: String,
    /// The project's phv.toml, if it has one.
    pub config: Option<String>,
    /// A site: its pages (paths under `public/`, such as `blog/post.phv`) and
    /// static files. A script: the one entry, with an empty path.
    pub pages: Vec<Page>,
    pub statics: Vec<(String, Vec<u8>)>,
    pub site: bool,
    /// A site's jobs (`[jobs] dir`), each with its name as the path.
    pub jobs: Vec<Page>,
}

#[derive(Serialize, Deserialize)]
pub struct Page {
    pub path: String,
    pub program: Program,
    pub entry_file: u32,
}

impl Page {
    /// The page ready to run (built-in functions linked, native code compiled).
    pub fn into_compiled(self) -> Result<Compiled, String> {
        let builtins = phv_std::link(&self.program.builtin_names)?;
        let mut c = Compiled {
            program: Arc::new(self.program),
            builtins: builtins.into(),
            deps: Vec::new(),
            warnings: String::new(),
            entry_file: self.entry_file,
            jit: None,
            from_cache: false,
            marked_tests: Vec::new(),
            test_hooks: Vec::new(),
        };
        c.enable_jit();
        Ok(c)
    }
}

fn page(root: &Path, file: &Path, path: String) -> Result<Page, CompileError> {
    let c = compile(root, file)?;
    if !c.warnings.is_empty() {
        eprint!("{}", c.warnings);
    }
    let program = Arc::try_unwrap(c.program).unwrap_or_else(|p| crate::cache::clone_program(&p));
    Ok(Page { path, program, entry_file: c.entry_file })
}

fn config_text(root: &Path) -> Result<Option<String>, String> {
    match read_source(&root.join("phv.toml")) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read phv.toml: {e}")),
    }
}

/// Compiles the script `entry` of the project at `root`.
pub fn build_script(root: &Path, entry: &Path) -> Result<Bundle, String> {
    let p = page(root, entry, String::new()).map_err(|e| e.render(false))?;
    let config = config_text(root)?;
    Ok(Bundle {
        version: env!("CARGO_PKG_VERSION").into(),
        config,
        pages: vec![p],
        statics: Vec::new(),
        site: false,
        jobs: Vec::new(),
    })
}

/// Compiles every page of the site at `root` (`public/**/*.phv`) and takes
/// its static files. Names starting with `.` or `_` are skipped, as the server
/// never serves them; templates are compiled into the pages that render them.
pub fn build_site(root: &Path) -> Result<Bundle, String> {
    let public = root.join("public");
    let mut files = Vec::new();
    collect(&public, String::new(), &mut files)?;
    let mut pages = Vec::new();
    let mut statics = Vec::new();
    let mut errors = String::new();
    for (rel, path) in files {
        let lower = rel.to_ascii_lowercase();
        if lower.ends_with(".phv.html") {
            continue;
        }
        if lower.ends_with(".phv") {
            match page(root, &path, rel) {
                Ok(p) => pages.push(p),
                Err(e) => errors.push_str(&e.render(false)),
            }
            continue;
        }
        let meta = std::fs::metadata(&path).map_err(|e| format!("{rel}: {e}"))?;
        if meta.len() > MAX_STATIC {
            eprintln!("warning: {rel} is larger than 64 MB and is not included");
            continue;
        }
        statics.push((rel, std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let config = config_text(root)?;
    // The jobs of `[jobs] dir`, compiled like the pages.
    let mut jobs = Vec::new();
    let parsed = crate::config::parse_config(root, config.as_deref().unwrap_or(""))?;
    if let Some(j) = &parsed.jobs {
        for name in crate::config::scan_jobs(root, &j.dir).unwrap_or_default() {
            let file = root.join(&j.dir).join(format!("{name}.phv"));
            match page(root, &file, name) {
                Ok(p) => jobs.push(p),
                Err(e) => errors.push_str(&e.render(false)),
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Bundle { version: env!("CARGO_PKG_VERSION").into(), config, pages, statics, site: true, jobs })
}

/// The regular files under `dir` (no links), as `/`-separated paths.
fn collect(dir: &Path, prefix: String, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        // Private files are left out, except the pages the server runs itself.
        let special = matches!(name.as_str(), "_middleware.phv" | "_404.phv" | "_500.phv");
        if name.starts_with('.') || (name.starts_with('_') && !special) {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect(&e.path(), rel, out)?;
        } else if ft.is_file() {
            out.push((rel, e.path()));
        }
    }
    Ok(())
}

/// The length of the executable at `bytes` without a bundle at its end.
fn program_len(file: &mut std::fs::File) -> std::io::Result<Option<(u64, u64)>> {
    let size = file.metadata()?.len();
    if size < TRAILER {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(size - 16))?;
    let mut tail = [0u8; 16];
    file.read_exact(&mut tail)?;
    if &tail[8..] != MAGIC {
        return Ok(None);
    }
    let len = u64::from_le_bytes(tail[..8].try_into().unwrap());
    let start = size.checked_sub(TRAILER).and_then(|s| s.checked_sub(len));
    Ok(start.map(|s| (s, len)))
}

/// Writes the running phv executable (without a bundle of its own) followed by `bundle` to `out`.
pub fn write(bundle: &Bundle, out: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the phv executable: {e}"))?;
    let mut f = std::fs::File::open(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
    let end = match program_len(&mut f).map_err(|e| e.to_string())? {
        Some((start, _)) => start,
        None => f.metadata().map_err(|e| e.to_string())?.len(),
    };
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(end as usize);
    f.take(end).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let body = postcard::to_allocvec(bundle).map_err(|e| format!("cannot encode the bundle: {e}"))?;
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(&Sha256::digest(&body));
    bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
    bytes.extend_from_slice(MAGIC);
    // Never through a link planted at the output's name.
    crate::write_new_file(out, &bytes).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// The bundle at the end of the running executable: None for a plain phv,
/// an error for a damaged bundle or one built by another version.
pub fn read_own() -> Option<Result<Bundle, String>> {
    let exe = std::env::current_exe().ok()?;
    let mut f = std::fs::File::open(exe).ok()?;
    let (start, len) = program_len(&mut f).ok()??;
    Some((|| {
        f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let mut body = vec![0u8; len as usize];
        f.read_exact(&mut body).map_err(|e| e.to_string())?;
        let mut sum = [0u8; 32];
        f.read_exact(&mut sum).map_err(|e| e.to_string())?;
        if Sha256::digest(&body)[..] != sum {
            return Err("the program bundled in this executable is damaged".to_string());
        }
        let b: Bundle = postcard::from_bytes(&body).map_err(|_| "the bundled program cannot be read".to_string())?;
        if b.version != env!("CARGO_PKG_VERSION") {
            return Err(format!(
                "the bundled program was built by phv {}, not {}",
                b.version,
                env!("CARGO_PKG_VERSION")
            ));
        }
        Ok(b)
    })())
}
