//! Loads a script with its imports and templates, type-checks it, compiles it
//! to bytecode and runs it.

pub mod bundle;
mod cache;
pub mod config;

pub use cache::compile_cached;
pub use config::{load_config, Config, Csp, DEFAULT_CSP};
use phv_syntax::ast::{Module, StmtKind};
use phv_syntax::{diag, parse_module_within, Diagnostic, SourceMap, Span};
use phv_types::{CheckInput, ModuleInput, ModuleKind};
use phv_vm::{BuiltinFn, Host, HttpCtx, Limits, Program, TraceLine, Vm, VmError};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A compiled program ready to run any number of times.
pub struct Compiled {
    pub program: Arc<Program>,
    pub builtins: Arc<[BuiltinFn]>,
    /// Files the program was built from, as they were when they were read.
    pub deps: Vec<Dep>,
    /// Rendered warnings (empty when there are none).
    pub warnings: String,
    /// Source file id of the entry script.
    pub entry_file: u32,
    /// Native code for the functions the JIT can compile (see `enable_jit`).
    pub jit: Option<Arc<phv_vm::JitTable>>,
    /// Loaded from the on-disk bytecode cache.
    pub from_cache: bool,
    /// Functions marked `#[Test]` or `#[Skip("...")]` (with the reason), by index.
    pub marked_tests: Vec<(u32, Option<String>)>,
    /// Functions marked `#[Before]` (false) or `#[After]` (true), by index.
    pub test_hooks: Vec<(u32, bool)>,
}

impl Compiled {
    /// Compiles eligible functions to native code. Disabled by `PHV_JIT=0`.
    pub fn enable_jit(&mut self) {
        if std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
            return;
        }
        self.jit = phv_jit::build(&self.program).map(Arc::new);
    }

    /// Number of functions compiled to native code.
    pub fn jit_count(&self) -> usize {
        self.jit.as_ref().map(|j| j.entries.iter().filter(|e| e.is_some()).count()).unwrap_or(0)
    }

    /// The tests of the entry file, in the order they are declared: functions
    /// marked `#[Test]` or `#[Skip]`, and those named `test_*` that take no
    /// parameters. Each comes with the reason it is skipped, if it is.
    pub fn tests(&self) -> Vec<(u32, String, Option<String>)> {
        let mut out: Vec<(u32, String, Option<String>)> = self
            .entry_functions("test_")
            .into_iter()
            .map(|(i, name)| {
                let skip = self.marked_tests.iter().find(|(t, _)| *t == i).and_then(|(_, s)| s.clone());
                (i, name, skip)
            })
            .collect();
        for (i, skip) in &self.marked_tests {
            let Some(f) = self.program.funcs.get(*i as usize) else { continue };
            if f.file == self.entry_file && !out.iter().any(|(j, ..)| j == i) {
                out.push((*i, f.name.clone(), skip.clone()));
            }
        }
        out.sort_by_key(|(i, ..)| *i);
        out
    }

    /// The `#[Before]` and `#[After]` functions of the entry file, in the order
    /// they are declared.
    pub fn hooks(&self) -> (Vec<u32>, Vec<u32>) {
        let of_entry = |f: &u32| self.program.funcs.get(*f as usize).is_some_and(|f| f.file == self.entry_file);
        let mut hooks: Vec<(u32, bool)> = self.test_hooks.iter().filter(|(f, _)| of_entry(f)).copied().collect();
        hooks.sort();
        let before = hooks.iter().filter(|(_, after)| !after).map(|(f, _)| *f).collect();
        let after = hooks.iter().filter(|(_, after)| *after).map(|(f, _)| *f).collect();
        (before, after)
    }

    /// Top-level functions of the entry file whose names start with `prefix`
    /// and take no parameters (used by `phv test`).
    pub fn entry_functions(&self, prefix: &str) -> Vec<(u32, String)> {
        self.program
            .funcs
            .iter()
            .enumerate()
            .filter(|(_, f)| f.file == self.entry_file && f.num_params == 0 && f.name.starts_with(prefix))
            .map(|(i, f)| (i as u32, f.name.clone()))
            .collect()
    }
}

impl Compiled {
    /// True if any source file changed since compilation (a quick check: see `Dep::is_fresh`).
    pub fn is_stale(&self) -> bool {
        self.deps.iter().any(|d| !d.is_fresh(false))
    }
}

/// A source file a program was built from, recorded when it was read.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Dep {
    pub path: PathBuf,
    /// The project's root: only the part of `path` below it, and where its
    /// links lead, is looked at for links to another machine (see `stat`).
    root: PathBuf,
    /// Modification time (seconds, nanoseconds) and size, for a quick check.
    mtime: Option<(u64, u32)>,
    len: u64,
    /// SHA-256 of the content that was compiled; None if the file was missing.
    hash: Option<[u8; 32]>,
    /// When the content was last known to be the recorded one (seconds since
    /// the epoch): when it was read, then whenever a check hashes it again.
    recorded: Recorded,
}

/// `Dep::recorded`, moved forward by checks that hash the file (a check of a
/// shared, compiled program).
#[derive(Debug, Default)]
struct Recorded(std::sync::atomic::AtomicU64);

impl Recorded {
    fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Clone for Recorded {
    fn clone(&self) -> Recorded {
        Recorded(self.get().into())
    }
}

impl serde::Serialize for Recorded {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

impl<'de> serde::Deserialize<'de> for Recorded {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Recorded, D::Error> {
        u64::deserialize(d).map(|v| Recorded(v.into()))
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn stat(root: &Path, p: &Path) -> Option<(Option<(u64, u32)>, u64)> {
    // A file that has become a link to another machine, or that is now reached
    // through one (a directory on the way became a link to a share), is not
    // followed: it counts as missing, so the program is loaded (and refused)
    // again. Found by the 16th assessment: only the last component was looked
    // at, so after a directory became such a link the server contacted the
    // host at each check.
    if phv_std::safefs::on_another_machine_below(root, p) {
        return None;
    }
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    Some((t.map(|d| (d.as_secs(), d.subsec_nanos())), m.len()))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(data).into()
}

impl Dep {
    /// Records `path` with the content that was actually read (`None`: missing).
    /// Records `path` of the project in `root`.
    fn new(root: &Path, path: &Path, content: Option<&[u8]>) -> Dep {
        let (mtime, len) = stat(root, path).unwrap_or((None, 0));
        Dep {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
            mtime,
            len,
            hash: content.map(sha256),
            recorded: Recorded(now_secs().into()),
        }
    }

    /// Whether the file still has the recorded content. A different time or size
    /// means changed. Equal ones are trusted only if the file was not modified
    /// close to when it was read (the "racy" case: it may have changed again
    /// within the clock's resolution); otherwise, or when `thorough`, the
    /// content is hashed and compared.
    ///
    /// A racy file whose content is found unchanged when its time is no longer
    /// recent is recorded as known then: a later change would give it another
    /// time, so it is not hashed again at every check.
    pub fn is_fresh(&self, thorough: bool) -> bool {
        let Some((mtime, len)) = stat(&self.root, &self.path) else { return self.hash.is_none() };
        if self.hash.is_none() || mtime != self.mtime || len != self.len {
            return false;
        }
        let racy = mtime.is_none_or(|(s, _)| s + 2 >= self.recorded.get());
        if !thorough && !racy {
            return true;
        }
        let checked = now_secs();
        let same = std::fs::read(&self.path).is_ok_and(|bytes| Some(sha256(&bytes)) == self.hash);
        if same && stat(&self.root, &self.path) == Some((mtime, len)) {
            self.recorded.0.fetch_max(checked, std::sync::atomic::Ordering::Relaxed);
        }
        same
    }
}

pub struct CompileError {
    pub sources: SourceMap,
    pub diags: Vec<Diagnostic>,
    /// Errors without a source location (e.g. the entry file is missing).
    pub messages: Vec<String>,
}

impl CompileError {
    pub fn render(&self, color: bool) -> String {
        let mut out = String::new();
        for m in &self.messages {
            out.push_str("error: ");
            out.push_str(&phv_syntax::diag::printable_text(m));
            out.push('\n');
        }
        let mut diags = self.diags.clone();
        diags.sort_by_key(|d| (d.span.file, d.span.start));
        out.push_str(&diag::render(&self.sources, &diags, color));
        let n = self.diags.iter().filter(|d| d.is_error()).count() + self.messages.len();
        out.push_str(&format!("{n} error{} found\n", if n == 1 { "" } else { "s" }));
        out
    }
}

/// The project root: the nearest directory containing `phv.toml`, or the
/// directory of the entry file.
pub fn find_root(entry: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    find_root_in(&cwd, entry)
}

/// `find_root` with relative paths resolved against `cwd`.
fn find_root_in(cwd: &Path, entry: &Path) -> PathBuf {
    let start = match entry.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut dir = start.clone();
    loop {
        if trusted_config(&cwd.join(&dir)) {
            return dir;
        }
        match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => dir = p.to_path_buf(),
            _ => break,
        }
    }
    // A relative path runs out before the current directory and the ones above
    // it (`public/index.phv` stops at `public`), so go on along the absolute path.
    if entry.is_relative() {
        if let Ok(abs) = resolve_here(&cwd.join(&start)) {
            if let Some(dir) = abs.ancestors().find(|d| trusted_config(d)) {
                return dir.to_path_buf();
            }
        }
    }
    start
}

/// Largest source file (module, template or entry script) that is read.
const MAX_SOURCE: u64 = 8 << 20;

/// Writes `bytes` to `path` without following a link there: into a new file
/// with a random name beside it (created only if nothing has that name), then
/// renamed over `path`, which replaces a link rather than writing to its target.
pub fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| Error::new(ErrorKind::InvalidInput, "no file name"))?;
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).map_err(|e| Error::other(e.to_string()))?;
    let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join(format!(".{}.{hex}.tmp", name.to_string_lossy()));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Reads a source file: only a regular file (not a pipe or a device, which could
/// block forever), of at most `MAX_SOURCE` bytes.
pub fn read_source(path: &Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    if final_link_to_another_machine(path) {
        return Err(Error::new(ErrorKind::PermissionDenied, "it is a link to another machine"));
    }
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
    }
    if meta.len() > MAX_SOURCE {
        return Err(Error::new(ErrorKind::InvalidData, format!("larger than {} MB", MAX_SOURCE >> 20)));
    }
    let mut text = String::new();
    file.take(MAX_SOURCE + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SOURCE {
        return Err(Error::new(ErrorKind::InvalidData, format!("larger than {} MB", MAX_SOURCE >> 20)));
    }
    Ok(text)
}

/// The canonical form of `path`, or, when a link on the way leads to another
/// machine, its absolute spelling: resolving such a link would contact the
/// host (see `safefs::on_another_machine`).
fn resolve_here(path: &Path) -> std::io::Result<PathBuf> {
    if phv_std::safefs::on_another_machine(path) {
        std::path::absolute(path)
    } else {
        path.canonicalize()
    }
}

/// Whether `path` itself is a link or junction that leads to another machine
/// (see `safefs::on_another_machine`): the link is read, not followed.
fn final_link_to_another_machine(path: &Path) -> bool {
    let parent = path.parent().unwrap_or(Path::new(""));
    phv_std::safefs::on_another_machine_below(parent, path)
}

/// Whether `dir` holds a `phv.toml` that may be used when it is found by
/// walking up from a script. A project file decides what scripts may read,
/// write and connect to, so one that another user could have put there is
/// skipped: at the root of a filesystem (on Windows any user may create files
/// at the root of a drive), in the shared temporary directory, and on Unix one
/// not owned by the current user or root.
fn trusted_config(dir: &Path) -> bool {
    let file = dir.join("phv.toml");
    let Ok(meta) = std::fs::symlink_metadata(&file) else { return false };
    if !meta.is_file() {
        return false;
    }
    let abs = resolve_here(dir).unwrap_or_else(|_| dir.to_path_buf());
    if abs.parent().is_none() {
        return false;
    }
    if std::env::temp_dir().canonicalize().is_ok_and(|t| t == abs) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = current_uid();
        if meta.uid() != me && meta.uid() != 0 {
            return false;
        }
    }
    true
}

/// The current user's id, taken from the owner of the home directory.
#[cfg(unix)]
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    static UID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UID.get_or_init(|| {
        // The home directory belongs to the user; fall back to the current directory.
        std::env::var_os("HOME")
            .and_then(|h| std::fs::metadata(h).ok())
            .or_else(|| std::fs::metadata(".").ok())
            .map_or(u32::MAX, |m| m.uid())
    })
}

fn display_name(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// A relative path that stays inside the project (no `..`, not absolute).
fn safe_relative(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// A parsed module, the diagnostics of parsing it, and its number of tokens.
type Parsed = (Arc<Module>, Arc<Vec<Diagnostic>>, usize);

/// The most tokens of all the files of one program together (its pages,
/// modules, templates and packages, and std). Each file has at most
/// `lexer::MAX_TOKENS`; without this a program of many such files took
/// gigabytes to check (found by the 15th assessment: 24 files, 4.7 GB). The
/// examples of the repository have about 30,000 together.
pub const MAX_PROGRAM_TOKENS: usize = 2_000_000;

/// The most bytes of source text of one program (see `MAX_PROGRAM_TOKENS`):
/// a file of one long comment or string has few tokens.
pub const MAX_PROGRAM_TEXT: usize = 32 << 20;

/// The most files one program may load (std's included).
pub const MAX_PROGRAM_FILES: usize = 2_000;

/// Modules parsed in this process, by file id, kind and content, and the
/// bytes of their text: a module loaded again with the same text and id (std
/// and unchanged files, in the language server, `phv test` and a server
/// compiling its pages) is not parsed again. Spans hold the file id, so it is
/// part of the key.
#[allow(clippy::type_complexity)]
static PARSED: std::sync::LazyLock<std::sync::Mutex<(HashMap<(u32, bool, [u8; 32]), Parsed>, usize)>> =
    std::sync::LazyLock::new(Default::default);

/// The most memory the kept modules may take (all forgotten at once when it
/// would be passed). What a module takes is measured as it is parsed: its
/// syntax tree is many times its text (found by the 1.0.0 audit: bounding the
/// text let the language server keep gigabytes).
const PARSED_KEEP: usize = 64 << 20;

/// Sources longer than this are parsed each time, never kept.
const PARSED_TEXT_MAX: usize = 1 << 20;

/// Parses a module of at most `limit` tokens: None when it has more and
/// `limit` is below `lexer::MAX_TOKENS` (the program's budget would be passed;
/// above that, the file itself is too large, which its diagnostics report).
fn parsed(text: &str, file: u32, template: bool, limit: usize) -> Option<Parsed> {
    let keep = text.len() <= PARSED_TEXT_MAX;
    let key = (file, template, sha256(text.as_bytes()));
    if keep {
        if let Some(p) = PARSED.lock().unwrap_or_else(|e| e.into_inner()).0.get(&key) {
            return (p.2 <= limit).then(|| p.clone());
        }
    }
    let before = phv_vm::alloc::thread_allocated();
    let (module, diags, tokens) = parse_module_within(text, file, template, limit);
    let tokens = match tokens {
        Some(n) => n,
        None if limit < phv_syntax::lexer::MAX_TOKENS => return None,
        // Too large on its own: nothing of it was kept, only the error.
        None => 0,
    };
    let p: Parsed = (Arc::new(module), Arc::new(diags), tokens);
    // Without the counting allocator (a program that embeds this crate), a
    // generous estimate.
    let measured = (phv_vm::alloc::thread_allocated() - before).max(0) as usize;
    let size = if measured > 0 { measured } else { text.len().saturating_mul(64) };
    if !keep || size > PARSED_KEEP / 4 {
        return Some(p);
    }
    let mut cache = PARSED.lock().unwrap_or_else(|e| e.into_inner());
    if cache.1 + size > PARSED_KEEP {
        *cache = Default::default();
    }
    if cache.0.insert(key, p.clone()).is_none() {
        cache.1 += size;
    }
    Some(p)
}

struct Loader {
    root: PathBuf,
    /// Canonical directories sources may be loaded from: the project and packages.
    allowed: Vec<PathBuf>,
    /// Package name to the directory its modules are loaded from (the
    /// project's packages and theirs).
    packages: HashMap<String, PathBuf>,
    /// The packages each package may import, and those the project may.
    package_deps: HashMap<String, Vec<String>>,
    root_deps: Vec<String>,
    /// Canonical package directories, to tell which package a file belongs to.
    package_roots: Vec<(String, PathBuf)>,
    /// The package each module belongs to (None: the project itself or std).
    owners: Vec<Option<String>>,
    /// Unsaved editor buffers (for the language server), by canonical path.
    overlay: HashMap<PathBuf, String>,
    sources: SourceMap,
    modules: Vec<(Arc<Module>, ModuleKind, String)>,
    diags: Vec<Diagnostic>,
    imports: HashMap<String, usize>,
    templates: HashMap<String, usize>,
    by_path: HashMap<PathBuf, usize>,
    deps: Vec<Dep>,
    /// Tokens and bytes of source of the files loaded so far (see `MAX_PROGRAM_TOKENS`).
    tokens: usize,
    text: usize,
    /// Set when the program passed a budget: nothing more is loaded, and it is not checked.
    too_large: bool,
}

impl Loader {
    /// Refuses the program for passing a budget (`what` it has more of), at `span`.
    fn refuse_too_large(&mut self, what: String, span: Span) {
        if !self.too_large {
            self.too_large = true;
            self.diags.push(Diagnostic::error(format!("this program is too large: {what}"), span).note(
                "a program is a page or script with every file it imports or renders; split it into smaller ones",
            ));
        }
    }

    /// Whether `path` is on another machine (a network share or drive, or a
    /// link to one), reported at `span`. Such a file is not opened or resolved:
    /// Windows would contact its host with the user's credentials (found by the
    /// 15th assessment: a link to a share was followed before the check that
    /// it is inside the project). Only the part below the project is looked at.
    fn remote(&mut self, path: &Path, span: Span, what: &str) -> bool {
        if !phv_std::safefs::on_another_machine_below(&self.root, path) {
            return false;
        }
        let shown = display_name(&self.root, path);
        self.diags.push(
            Diagnostic::error(
                format!("{what} {shown} resolves to a file outside the project, on another machine"),
                span,
            )
            .note("it is a network share or drive, or a link to one; sources are read only from this machine"),
        );
        true
    }

    fn add(
        &mut self,
        name: String,
        path: Option<PathBuf>,
        text: String,
        kind: ModuleKind,
        span: Option<Span>,
    ) -> usize {
        if let Some(p) = &path {
            self.deps.push(Dep::new(&self.root, p, Some(text.as_bytes())));
        }
        self.text += text.len();
        let file = self.sources.add(name.clone(), path.clone(), text);
        let limit = MAX_PROGRAM_TOKENS.saturating_sub(self.tokens);
        let template = kind == ModuleKind::Template;
        let (module, diags, tokens) = match parsed(&self.sources.get(file).text, file, template, limit) {
            Some(p) => p,
            None => {
                let at = span.unwrap_or(Span::new(file, 0, 0));
                self.refuse_too_large(format!("its files have more than {MAX_PROGRAM_TOKENS} tokens in all"), at);
                let empty = Module { file, is_template: template, stmts: Vec::new(), template_refs: Vec::new() };
                (Arc::new(empty), Arc::new(Vec::new()), 0)
            }
        };
        self.tokens += tokens;
        self.diags.extend(diags.iter().cloned());
        self.modules.push((module, kind, name));
        let idx = self.modules.len() - 1;
        let owner = path.as_ref().and_then(|p| {
            let canonical = p.canonicalize().ok()?;
            self.package_roots
                .iter()
                .filter(|(_, dir)| canonical.starts_with(dir))
                .max_by_key(|(_, dir)| dir.as_os_str().len())
                .map(|(name, _)| name.clone())
        });
        self.owners.push(owner);
        if let Some(p) = path {
            self.by_path.insert(p, idx);
        }
        self.load_deps(idx);
        idx
    }

    fn load_file(&mut self, path: PathBuf, kind: ModuleKind, span: Span, what: &str) -> Option<usize> {
        if let Some(&i) = self.by_path.get(&path) {
            return Some(i);
        }
        if self.too_large {
            return None;
        }
        if self.modules.len() >= MAX_PROGRAM_FILES {
            self.refuse_too_large(format!("it has more than {MAX_PROGRAM_FILES} files"), span);
            return None;
        }
        if self.remote(&path, span, what) {
            return None;
        }
        // Symlinks and junctions must not pull in files from outside the project. A path
        // that cannot be resolved (missing, or a device name such as `CON`) is not read.
        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                let shown = display_name(&self.root, &path);
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{what} not found (looked for {shown})")
                } else {
                    format!("cannot read {what} at {shown}")
                };
                self.diags.push(Diagnostic::error(msg, span));
                return None;
            }
        };
        if !self.allowed.iter().any(|a| canonical.starts_with(a)) {
            let shown = display_name(&self.root, &path);
            self.diags.push(Diagnostic::error(format!("{what} {shown} resolves to a file outside the project"), span));
            return None;
        }
        // Text from the editor has the limit of files read from disk (found by
        // the 1.0.0 audit: an open document of 40 MB was analysed).
        let overlaid = self.overlay.get(&canonical).cloned().map(|t| {
            if t.len() as u64 > MAX_SOURCE {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("larger than {} MB", MAX_SOURCE >> 20),
                ))
            } else {
                Ok(t)
            }
        });
        match overlaid.unwrap_or_else(|| read_source(&path)) {
            Ok(text) if self.text + text.len() > MAX_PROGRAM_TEXT => {
                let mb = MAX_PROGRAM_TEXT >> 20;
                self.refuse_too_large(format!("its files have more than {mb} MB of source in all"), span);
                None
            }
            Ok(text) => {
                let name = display_name(&self.root, &path);
                Some(self.add(name, Some(path), text, kind, Some(span)))
            }
            Err(e) => {
                let shown = display_name(&self.root, &path);
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{what} not found (looked for {shown})")
                } else {
                    format!("cannot read {what} at {shown}: {e}")
                };
                self.diags.push(Diagnostic::error(msg, span));
                None
            }
        }
    }

    fn load_deps(&mut self, idx: usize) {
        let imports: Vec<(String, Span)> = self.modules[idx]
            .0
            .stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Import(d) => Some((d.module.join("."), d.span)),
                _ => None,
            })
            .collect();
        for (name, span) in imports {
            if self.too_large {
                return;
            }
            if self.imports.contains_key(&name) {
                continue;
            }
            if name.starts_with("std.") || name == "std" {
                match phv_std::std_module(&name) {
                    Some(src) => {
                        let file = format!("<std>/{}.phv", name.trim_start_matches("std."));
                        // Reserve the name first so import cycles terminate.
                        self.imports.insert(name.clone(), usize::MAX);
                        let i = self.add(file, None, src.to_string(), ModuleKind::Std, Some(span));
                        self.imports.insert(name, i);
                    }
                    None => self.diags.push(Diagnostic::error(format!("unknown standard module `{name}`"), span)),
                }
                continue;
            }
            let segments: Vec<&str> = name.split('.').collect();
            // A module imports from its own package, from the packages it
            // depends on, and (the project's own modules) from the project.
            let owner = self.owners.get(idx).cloned().flatten();
            let is_package = self.packages.contains_key(segments[0]);
            let refused = match &owner {
                None if is_package && !self.root_deps.iter().any(|d| d == segments[0]) => Some(
                    Diagnostic::error(format!("package `{}` is not a dependency of this project", segments[0]), span)
                        .note("add it to [dependencies] in phv.toml to use it here"),
                ),
                Some(p)
                    if p != segments[0]
                        && !self.package_deps.get(p).is_some_and(|d| d.iter().any(|x| x == segments[0])) =>
                {
                    Some(Diagnostic::error(
                        if is_package {
                            format!("package `{p}` does not depend on `{}`", segments[0])
                        } else {
                            format!("modules of package `{p}` import its own modules as `{p}.<module>`, its dependencies and `std`")
                        },
                        span,
                    ))
                }
                _ => None,
            };
            if let Some(d) = refused {
                self.diags.push(d);
                continue;
            }
            let (base, rest) = match self.packages.get(segments[0]) {
                Some(dir) if segments.len() > 1 => (dir.clone(), &segments[1..]),
                Some(dir) => {
                    if !dir.is_dir() {
                        self.diags.push(
                            Diagnostic::error(format!("package `{name}` is not installed"), span)
                                .note("run `phv install`"),
                        );
                        continue;
                    }
                    (dir.clone(), &segments[..0])
                }
                None => (self.root.clone(), &segments[..]),
            };
            if rest.is_empty() {
                self.diags.push(Diagnostic::error(
                    format!("`{name}` is a package; import one of its modules, e.g. `import {name}.main.{{...}}`"),
                    span,
                ));
                continue;
            }
            if !base.is_dir() {
                self.diags.push(
                    Diagnostic::error(format!("package `{}` is not installed", segments[0]), span)
                        .note("run `phv install`"),
                );
                continue;
            }
            let rel: PathBuf = rest.iter().collect::<PathBuf>().with_extension("phv");
            let path = base.join(rel);
            if self.remote(&path, span, &format!("module `{name}`")) {
                continue;
            }
            // A package's module must really be inside the package (not a link out of it).
            let root_of = self.package_roots.iter().find(|(n, _)| n == segments[0]).map(|(_, d)| d.clone());
            if let (Some(pr), Ok(c)) = (root_of, path.canonicalize()) {
                if !c.starts_with(&pr) {
                    self.diags.push(Diagnostic::error(
                        format!("module `{name}` is outside the package `{}`", segments[0]),
                        span,
                    ));
                    continue;
                }
            }
            self.imports.insert(name.clone(), usize::MAX);
            match self.load_file(path, ModuleKind::User, span, &format!("module `{name}`")) {
                Some(i) => {
                    self.imports.insert(name, i);
                }
                None => {
                    self.imports.remove(&name);
                }
            }
        }
        let refs = self.modules[idx].0.template_refs.clone();
        for (rel, span) in refs {
            if self.too_large {
                return;
            }
            if self.templates.contains_key(&rel) {
                continue;
            }
            if !safe_relative(&rel) {
                self.diags.push(
                    Diagnostic::error("template paths must be relative to the project and stay inside it", span)
                        .note("`..` and absolute paths are not allowed"),
                );
                continue;
            }
            if !rel.to_ascii_lowercase().ends_with(".phv.html") {
                self.diags.push(Diagnostic::error("templates must be `.phv.html` files", span));
                continue;
            }
            let path = self.root.join(&rel);
            self.templates.insert(rel.clone(), usize::MAX);
            match self.load_file(path, ModuleKind::Template, span, "template") {
                Some(i) => {
                    self.templates.insert(rel, i);
                }
                None => {
                    self.templates.remove(&rel);
                }
            }
        }
    }
}

/// The result of loading and type-checking a program (without compiling it).
pub struct Analysis {
    pub sources: SourceMap,
    pub diags: Vec<Diagnostic>,
    /// Present when parsing succeeded (it may still contain type errors).
    pub hir: Option<phv_types::hir::HProgram>,
    pub entry_file: Option<u32>,
    pub messages: Vec<String>,
    /// Only from `analyze_for_editor`.
    pub index: Option<phv_types::index::EditorIndex>,
    /// The tokens of all its files (see `MAX_PROGRAM_TOKENS`).
    pub tokens: usize,
    deps: Vec<Dep>,
}

/// Directory of each package listed in `[dependencies]`.
pub fn package_dirs(root: &Path, config: &Config) -> HashMap<String, PathBuf> {
    package_graph(root, config).0
}

/// Every package the project uses (its own and theirs, which `phv install`
/// puts in `vendor/`), with the names each package depends on. Only the
/// `[dependencies]` of a package's phv.toml are read.
pub fn package_graph(root: &Path, config: &Config) -> (HashMap<String, PathBuf>, HashMap<String, Vec<String>>) {
    let mut dirs: HashMap<String, PathBuf> = HashMap::new();
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue: Vec<String> = Vec::new();
    for (name, dep) in &config.dependencies {
        let dir = match &dep.path {
            Some(p) => root.join(p),
            None => root.join("vendor").join(name),
        };
        dirs.insert(name.clone(), dir);
        queue.push(name.clone());
    }
    while let Some(name) = queue.pop() {
        if deps.contains_key(&name) {
            continue;
        }
        let names = package_dependencies(root, &dirs[&name]);
        for n in &names {
            if !dirs.contains_key(n) {
                dirs.insert(n.clone(), root.join("vendor").join(n));
                queue.push(n.clone());
            }
        }
        deps.insert(name, names);
    }
    (dirs, deps)
}

/// The names in the `[dependencies]` of the package in `dir` (none if it has
/// no phv.toml, or is on another machine: see `Loader::remote`).
fn package_dependencies(root: &Path, dir: &Path) -> Vec<String> {
    let manifest = dir.join("phv.toml");
    if phv_std::safefs::on_another_machine_below(root, &manifest) {
        return Vec::new();
    }
    let Ok(text) = read_source(&manifest) else { return Vec::new() };
    let Ok(t) = toml::from_str::<toml::Table>(&text) else { return Vec::new() };
    let Some(table) = t.get("dependencies").and_then(|d| d.as_table()) else { return Vec::new() };
    table.keys().filter(|k| config::valid_package_name(k)).cloned().collect()
}

/// Loads and type-checks `entry`, using `overlay` (canonical path to text)
/// instead of files on disk where given.
pub fn analyze(root: &Path, entry: &Path, overlay: HashMap<PathBuf, String>) -> Analysis {
    analyze_with(root, entry, overlay, false)
}

/// Like `analyze`, also building the editor index. Code with syntax errors is
/// still checked (the parser skips the broken statements), so completion
/// works while a line is half written.
pub fn analyze_for_editor(root: &Path, entry: &Path, overlay: HashMap<PathBuf, String>) -> Analysis {
    analyze_with(root, entry, overlay, true)
}

fn analyze_with(root: &Path, entry: &Path, overlay: HashMap<PathBuf, String>, editor: bool) -> Analysis {
    let mut messages = Vec::new();
    let config = load_config(root).unwrap_or_else(|e| {
        messages.push(e);
        Config::default_for(root)
    });
    let (mut packages, package_deps) = package_graph(root, &config);
    // A package on another machine is not used (see `Loader::remote`).
    let remote = |p: &Path| phv_std::safefs::on_another_machine_below(root, p);
    let mut far: Vec<String> = packages.iter().filter(|(_, d)| remote(d)).map(|(n, _)| n.clone()).collect();
    far.sort();
    for n in far {
        packages.remove(&n);
        messages.push(format!(
            "the package `{n}` is on another machine (a network share or drive, or a link to one); \
             packages are read only from this machine"
        ));
    }
    let package_roots: Vec<(String, PathBuf)> =
        packages.iter().filter_map(|(n, d)| Some((n.clone(), d.canonicalize().ok()?))).collect();
    // Imports are known by name in the whole program, so a package may not
    // share its name with a directory or module of the project (`import
    // lib.posts` must mean one thing), unless it is that directory.
    let mut clashes: Vec<&String> = packages
        .iter()
        .filter(|(n, dir)| {
            let own = root.join(n.as_str());
            let module = root.join(format!("{n}.phv"));
            if remote(&own) || remote(&module) {
                return true;
            }
            let same = own.canonicalize().ok().is_some_and(|o| dir.canonicalize().ok() == Some(o));
            (own.is_dir() && !same) || module.is_file()
        })
        .map(|(n, _)| n)
        .collect();
    clashes.sort();
    for n in clashes {
        messages.push(format!(
            "the package `{n}` has the name of a directory or module of this project; rename one of them"
        ));
    }
    let mut allowed: Vec<PathBuf> = package_roots.iter().map(|(_, d)| d.clone()).collect();
    allowed.extend(root.canonicalize().ok());
    let mut loader = Loader {
        root: root.to_path_buf(),
        allowed,
        packages,
        package_deps,
        root_deps: config.dependencies.keys().cloned().collect(),
        package_roots,
        owners: Vec::new(),
        overlay,
        sources: SourceMap::default(),
        modules: Vec::new(),
        diags: Vec::new(),
        imports: HashMap::new(),
        templates: HashMap::new(),
        by_path: HashMap::new(),
        deps: Vec::new(),
        tokens: 0,
        text: 0,
        too_large: false,
    };
    let toml = root.join("phv.toml");
    // Bounded like every source file (a huge phv.toml would be read whole on each keystroke).
    loader.deps.push(Dep::new(root, &toml, read_source(&toml).ok().as_ref().map(|t| t.as_bytes())));
    // The packages' own [dependencies] decide what their modules may import.
    let mut manifests: Vec<PathBuf> = loader.packages.values().map(|d| d.join("phv.toml")).collect();
    manifests.sort();
    for m in manifests {
        if let Ok(text) = read_source(&m) {
            loader.deps.push(Dep::new(root, &m, Some(text.as_bytes())));
        }
    }
    loader.add("<std>/prelude.phv".into(), None, phv_std::PRELUDE.to_string(), ModuleKind::Prelude, None);
    // The entry is looked at like the files it imports (see `Loader::remote`).
    let read = if remote(entry) {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "it is on another machine (a network share or drive, or a link to one)",
        ))
    } else {
        // The editor's text of the document has the limit of files read from disk
        // (found by the 1.0.0 audit).
        let overlaid = entry.canonicalize().ok().and_then(|c| loader.overlay.get(&c).cloned()).map(|t| {
            if t.len() as u64 > MAX_SOURCE {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("larger than {} MB", MAX_SOURCE >> 20),
                ))
            } else {
                Ok(t)
            }
        });
        overlaid.unwrap_or_else(|| read_source(entry))
    };
    let text = match read {
        Ok(t) => t,
        Err(e) => {
            messages.push(format!("cannot read {}: {e}", entry.display()));
            return Analysis {
                sources: loader.sources,
                diags: Vec::new(),
                hir: None,
                entry_file: None,
                messages,
                index: None,
                deps: Vec::new(),
                tokens: 0,
            };
        }
    };
    let name = display_name(root, entry);
    // An editor can open a template on its own; it is checked as a template.
    let template = editor && entry.to_string_lossy().ends_with(".phv.html");
    let kind = if template { ModuleKind::Template } else { ModuleKind::Entry };
    let entry_module = loader.add(name, Some(entry.to_path_buf()), text, kind, None);
    let entry_file = Some(loader.modules[entry_module].0.file);
    // A program past a budget is not checked: checking is what takes the most
    // memory (found by the 15th assessment). Its trees are dropped first.
    if loader.too_large {
        drop(std::mem::take(&mut loader.modules));
        return Analysis {
            sources: loader.sources,
            diags: loader.diags,
            hir: None,
            entry_file,
            messages,
            index: None,
            deps: loader.deps,
            tokens: loader.tokens,
        };
    }
    // With syntax or loading errors the program cannot run, but the files that
    // loaded cleanly are still checked, so their type errors show up as well.
    let broken = !editor && loader.diags.iter().any(|d| d.is_error());
    let input = CheckInput {
        modules: loader
            .modules
            .iter()
            .map(|(ast, kind, name)| ModuleInput { ast, kind: *kind, name: name.clone() })
            .collect(),
        imports: loader.imports.clone(),
        templates: loader.templates.clone(),
    };
    let (hir, diags, index) = if editor {
        let (hir, diags, index) = phv_types::check_for_editor(&input);
        (hir, diags, Some(index))
    } else {
        let (hir, diags) = phv_types::check(&input);
        (hir, diags, None)
    };
    drop(input);
    let mut diags = diags;
    if broken {
        // A file with a syntax error is checked as the parser recovered it, and a
        // file using it sees what that left: their errors could be made up.
        let tainted = tainted_files(&loader);
        diags.retain(|d| !tainted.contains(&d.span.file));
    }
    loader.diags.extend(diags);
    if config.csp.allows_inline_code() {
        loader.diags.retain(|d| d.code != Some(phv_syntax::diag::CSP_INLINE));
    }
    if !editor && loader.diags.iter().any(|d| d.is_error()) {
        loader.diags.retain(|d| d.code != Some(phv_syntax::diag::UNUSED));
    }
    Analysis {
        sources: loader.sources,
        diags: loader.diags,
        hir: if broken { None } else { Some(hir) },
        entry_file,
        messages,
        index,
        deps: loader.deps,
        tokens: loader.tokens,
    }
}

/// Files with a loading or syntax error, and the files that import or render
/// them (directly or through other files).
fn tainted_files(loader: &Loader) -> HashSet<u32> {
    let mut tainted: HashSet<u32> = loader.diags.iter().filter(|d| d.is_error()).map(|d| d.span.file).collect();
    // The files that use each file. A use that did not load taints the user,
    // which then also has an error of its own.
    let mut users: HashMap<u32, HashSet<u32>> = HashMap::new();
    for (module, _, _) in &loader.modules {
        let imports: HashSet<String> = module
            .stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Import(d) => Some(d.module.join(".")),
                _ => None,
            })
            .collect();
        let used = imports
            .iter()
            .map(|name| loader.imports.get(name))
            .chain(module.template_refs.iter().map(|(rel, _)| loader.templates.get(rel)));
        for target in used {
            match target.filter(|i| **i != usize::MAX) {
                Some(i) => {
                    users.entry(loader.modules[*i].0.file).or_default().insert(module.file);
                }
                None => {
                    tainted.insert(module.file);
                }
            }
        }
    }
    let mut work: Vec<u32> = tainted.iter().copied().collect();
    while let Some(file) = work.pop() {
        for user in users.get(&file).into_iter().flatten() {
            if tainted.insert(*user) {
                work.push(*user);
            }
        }
    }
    tainted
}

/// Loads, checks and compiles `entry` (and everything it uses).
pub fn compile(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    compile_with_overlay(root, entry, HashMap::new())
}

/// Like `compile`, reading the files in `overlay` (canonical path to text) from memory.
pub fn compile_with_overlay(
    root: &Path,
    entry: &Path,
    overlay: HashMap<PathBuf, String>,
) -> Result<Compiled, CompileError> {
    compile_full(root, entry, overlay, Default::default())
}

/// Like `compile`; with `coverage`, the program records which statements run
/// (`run_test`), and the JIT does not compile it.
pub fn compile_for_tests(root: &Path, entry: &Path, coverage: bool) -> Result<Compiled, CompileError> {
    let opts = phv_compiler::Options { statement_markers: coverage, ..Default::default() };
    compile_full(root, entry, HashMap::new(), opts)
}

/// Like `compile`, counting the calls of each function for `run_profiled`.
pub fn compile_for_profile(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    compile_full(root, entry, HashMap::new(), phv_compiler::Options { call_counts: true, ..Default::default() })
}

fn compile_full(
    root: &Path,
    entry: &Path,
    overlay: HashMap<PathBuf, String>,
    opts: phv_compiler::Options,
) -> Result<Compiled, CompileError> {
    let a = analyze(root, entry, overlay);
    if !a.messages.is_empty() || a.diags.iter().any(|d| d.is_error()) || a.hir.is_none() {
        return Err(CompileError { sources: a.sources, diags: a.diags, messages: a.messages });
    }
    let hir = a.hir.unwrap();
    let mut warnings: Vec<Diagnostic> = a.diags.iter().filter(|d| !d.is_error()).cloned().collect();
    warnings.sort_by_key(|d| (d.span.file, d.span.start));
    let warnings = diag::render(&a.sources, &warnings, false);
    let program = match phv_compiler::compile_with(&hir, &a.sources, opts) {
        Ok(p) => p,
        Err(d) => return Err(CompileError { sources: a.sources, diags: vec![d], messages: Vec::new() }),
    };
    let marked_tests = hir.tests.iter().map(|(f, skip)| (*f, skip.clone())).collect();
    let test_hooks = hir.hooks.clone();
    let builtins = phv_std::link(&program.builtin_names).map_err(|m| CompileError {
        sources: SourceMap::default(),
        diags: Vec::new(),
        messages: vec![m],
    })?;
    Ok(Compiled {
        program: Arc::new(program),
        builtins: builtins.into(),
        deps: a.deps,
        warnings,
        entry_file: a.entry_file.unwrap_or(0),
        jit: None,
        from_cache: false,
        marked_tests,
        test_hooks,
    })
}

pub struct RunOptions {
    /// Escape plain strings written by `echo` (web pages).
    pub html: bool,
    pub limits: Limits,
    pub http: Option<HttpCtx>,
    /// Project root and permissions (from `phv.toml`).
    pub host: Option<Arc<Host>>,
}

pub struct RuntimeError {
    pub message: String,
    pub trace: Vec<TraceLine>,
}

impl RuntimeError {
    pub fn render(&self) -> String {
        let mut out = self.message.clone();
        out.push('\n');
        for t in &self.trace {
            out.push_str(&format!("  at {} ({}:{})\n", t.func, t.file, t.line));
        }
        out
    }

    /// The message and trace for a log: each part on its own line, with what
    /// a script or a visitor put in it escaped (see `diag::log_escape`).
    pub fn render_for_log(&self) -> String {
        use phv_syntax::diag::log_escape;
        let mut out = log_escape(&self.message);
        out.push('\n');
        for t in &self.trace {
            out.push_str(&format!("  at {} ({}:{})\n", log_escape(&t.func), log_escape(&t.file), t.line));
        }
        out
    }
}

pub struct RunOutcome {
    pub output: String,
    pub error: Option<RuntimeError>,
    pub http: HttpCtx,
    /// The rest of the run's VM; dropping it ends the run (see `Teardown`).
    /// `None` once taken. Dropping the outcome drops it too.
    pub teardown: Option<Teardown>,
}

/// What is left of a run once its output and `http` were taken: the VM's
/// registers and heap, and the connections the script still held (a database
/// handle in a variable, say). Dropping it ends the run: the objects are torn
/// down, SQLite connections roll back and go back to the kept pool, and pooled
/// PostgreSQL, MySQL and Redis connections are reset and returned (round trips
/// to the server). That is the slow part of a request's end, so a server hands
/// the response over first and drops this after.
///
/// Invariant: a `Teardown` is dropped on the thread that ran the VM, before
/// that thread runs another script. The counting allocator (`phv_vm::alloc`)
/// keeps a count per thread, and a run measures its memory against the count
/// at its start: dropped on another thread, the run's memory would stay
/// counted against the thread that ran it (and the other's count would go
/// below zero); dropped after the next run on the thread began, it would be
/// taken off that run's use, and the run could exceed its limit by that much.
/// (The VM is not `Send`, so the first cannot happen by accident.)
pub struct Teardown {
    /// Only dropped (its registers, heap and resources are the run's leftovers).
    _vm: Box<Vm>,
}

/// Runs a compiled program in a fresh VM (a fresh heap).
pub fn run(c: &Compiled, opts: RunOptions) -> RunOutcome {
    run_func(c, c.program.main, opts)
}

/// Runs one function of a compiled program (with no arguments) in a fresh VM.
pub fn run_func(c: &Compiled, func: u32, opts: RunOptions) -> RunOutcome {
    let mut vm = new_vm(c, opts);
    let error = call_caught(&mut vm, func);
    outcome(vm, error)
}

/// The outcome of a run of `vm` that ended with `error`: its output and
/// `http` are taken; the VM itself goes into the outcome's teardown.
fn outcome(mut vm: Vm, error: Option<RuntimeError>) -> RunOutcome {
    let output = vm.take_output();
    let http = std::mem::take(&mut vm.http);
    RunOutcome { output, error, http, teardown: Some(Teardown { _vm: Box::new(vm) }) }
}

/// Runs test `test` with the `before` functions first and the `after` ones
/// last (also when the test fails), in one VM. With `coverage`, the
/// statements that ran (as file and source offset) are added to it.
pub fn run_test(
    c: &Compiled,
    before: &[u32],
    test: u32,
    after: &[u32],
    opts: RunOptions,
    coverage: Option<&mut std::collections::HashSet<(u32, u32)>>,
) -> RunOutcome {
    let mut vm = new_vm(c, opts);
    if coverage.is_some() {
        vm.coverage = Some(std::collections::HashSet::new());
    }
    let mut error = None;
    for f in before.iter().chain(std::iter::once(&test)) {
        error = call_caught(&mut vm, *f);
        if error.is_some() {
            break;
        }
    }
    for f in after {
        let e = call_caught(&mut vm, *f);
        error = error.or(e);
    }
    if let (Some(all), Some(ran)) = (coverage, vm.coverage.take()) {
        all.extend(ran);
    }
    outcome(vm, error)
}

/// Like `compile`, with a marker before each statement for the debugger (and
/// no JIT, which would skip the markers).
pub fn compile_for_debug(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    compile_full(root, entry, HashMap::new(), phv_compiler::Options { statement_markers: true, ..Default::default() })
}

/// Runs the program with `hook` called before each statement (`phv dap`).
pub fn run_debug(c: &Compiled, opts: RunOptions, hook: Box<dyn phv_vm::vm::DebugHook>) -> RunOutcome {
    let mut vm = new_vm(c, opts);
    vm.jit = None;
    vm.debug = Some(hook);
    let error = call_caught(&mut vm, c.program.main);
    vm.debug = None;
    outcome(vm, error)
}

/// Runs the program as `run` does, measuring the calls and time of each function
/// (`phv run --profile`). `c` comes from `compile_for_profile`; the JIT is not
/// used, so every call is seen.
pub fn run_profiled(c: &Compiled, opts: RunOptions) -> (RunOutcome, phv_vm::vm::Profile) {
    let mut vm = new_vm(c, opts);
    vm.jit = None;
    vm.profile = Some(phv_vm::vm::Profile::new(c.program.funcs.len()));
    let error = call_caught(&mut vm, c.program.main);
    let profile = vm.profile.take().unwrap_or_default();
    (outcome(vm, error), profile)
}

fn new_vm(c: &Compiled, opts: RunOptions) -> Vm {
    let mut vm = Vm::new(c.program.clone(), c.builtins.clone(), opts.html, opts.limits);
    if let Some(h) = opts.http {
        vm.http = h;
    }
    if let Some(host) = opts.host {
        vm.host = host;
    }
    vm.jit = c.jit.clone();
    vm
}

/// Calls function `func` with no arguments; the error it ended with, if any.
fn call_caught(vm: &mut Vm, func: u32) -> Option<RuntimeError> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.call_function(func, &[]).map(|_| ())));
    match result {
        Ok(Ok(())) => None,
        Ok(Err(VmError::Exception(e))) => {
            Some(RuntimeError { message: format!("Uncaught {}", vm.describe_exception(&e)), trace: e.trace })
        }
        Ok(Err(VmError::Fatal(m))) => Some(RuntimeError { message: format!("Fatal error: {m}"), trace: Vec::new() }),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown".into());
            Some(RuntimeError { message: format!("Internal error: {msg}"), trace: Vec::new() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file saved just before it was read is hashed at each check ("racy"),
    /// until a check finds it unchanged once its time is no longer recent.
    #[test]
    fn racy_files_are_hashed_until_they_settle() {
        let tmp = std::env::temp_dir().join(format!("phv-racy-{}.phv", std::process::id()));
        std::fs::write(&tmp, "echo 1;").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&tmp).unwrap().set_modified(old).unwrap();
        let dep = Dep::new(tmp.parent().unwrap(), &tmp, Some(b"echo 1;"));
        // As if it had been read in the second it was saved.
        let mtime = dep.mtime.unwrap().0;
        dep.recorded.0.store(mtime, std::sync::atomic::Ordering::Relaxed);
        assert!(dep.is_fresh(false));
        assert!(dep.recorded.get() > mtime + 2, "the check records that the content was known later");
        // A change of the same size and time is still seen while it is racy.
        let racy = Dep::new(tmp.parent().unwrap(), &tmp, Some(b"echo 2;"));
        racy.recorded.0.store(mtime, std::sync::atomic::Ordering::Relaxed);
        assert!(!racy.is_fresh(false));
        let _ = std::fs::remove_file(&tmp);
    }

    /// Found by the 16th assessment: the freshness check looked only at the
    /// file itself, so after `lib/` became a directory link to a share it read
    /// the file's time and size over the network at each check (and a running
    /// server kept the page as fresh). Making a link needs Developer Mode or
    /// the privilege: skipped without.
    #[cfg(windows)]
    #[test]
    fn freshness_checks_do_not_go_through_a_directory_link_to_another_machine() {
        let root = std::env::temp_dir().join(format!("phv-fresh-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let file = root.join("lib").join("m.phv");
        std::fs::write(&file, "echo 1;").unwrap();
        let dep = Dep::new(&root, &file, Some(b"echo 1;"));
        assert!(dep.is_fresh(true));
        std::fs::rename(root.join("lib"), root.join("libreal")).unwrap();
        // This machine by a share's name (when its administrative share is
        // there, the old check read the file through it), and a host that
        // does not exist.
        let real = root.join("libreal").to_string_lossy().into_owned();
        let local = format!(r"\\localhost\{}$\{}", &real[..1], &real[3..]);
        for target in [local.as_str(), r"\\phv-test.invalid\share\lib", r"\\?\UNC\phv-test.invalid\share\lib"] {
            let _ = std::fs::remove_dir(root.join("lib"));
            if std::os::windows::fs::symlink_dir(target, root.join("lib")).is_err() {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            let start = std::time::Instant::now();
            assert_eq!(stat(&root, &file), None, "{target}");
            assert!(!dep.is_fresh(false), "{target}");
            assert!(!dep.is_fresh(true), "{target}");
            assert!(start.elapsed() < std::time::Duration::from_secs(2), "{target}: {:?}", start.elapsed());
        }
        // A link that stays on this machine is followed as before.
        let _ = std::fs::remove_dir(root.join("lib"));
        std::os::windows::fs::symlink_dir(root.join("libreal"), root.join("lib")).unwrap();
        assert!(stat(&root, &file).is_some());
        assert!(dep.is_fresh(true));
        let _ = std::fs::remove_dir(root.join("lib"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_root_from_relative_paths() {
        let tmp = std::env::temp_dir().join(format!("phv-root-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("public/admin")).unwrap();
        std::fs::write(tmp.join("phv.toml"), "").unwrap();
        let site = tmp.canonicalize().unwrap();
        let public = site.join("public");

        // From the project root: the path as written reaches `.` only with `./`.
        assert_eq!(find_root_in(&site, Path::new("./public/index.phv")), PathBuf::from("."));
        assert_eq!(find_root_in(&site, Path::new("index.phv")), PathBuf::from("."));
        assert_eq!(find_root_in(&site, Path::new("public/index.phv")), site);
        // From a directory inside the project.
        assert_eq!(find_root_in(&public, Path::new("index.phv")), site);
        assert_eq!(find_root_in(&public, Path::new("admin/index.phv")), site);
        assert_eq!(find_root_in(&public.join("admin"), Path::new("../index.phv")), site);
        // Absolute paths do not depend on the current directory.
        assert_eq!(find_root_in(Path::new("/"), &public.join("index.phv")), public.parent().unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Found by the 15th assessment: a source was resolved (`canonicalize`,
    /// which opens a link's target) before the check that it is inside the
    /// project, so a link to a share made `phv check` contact the host. Making
    /// a link needs Developer Mode or the privilege: skipped without.
    #[cfg(windows)]
    #[test]
    fn sources_behind_a_link_to_a_share_are_refused_unresolved() {
        let dir = std::env::temp_dir().join(format!("phv-remote-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        std::fs::write(dir.join("phv.toml"), "[project]\nname = \"t\"\n").unwrap();
        let page = dir.join("page.phv");
        let refused = |src: &str| {
            std::fs::write(&page, src).unwrap();
            let start = std::time::Instant::now();
            let a = analyze(&dir, &page, HashMap::new());
            let messages: Vec<String> = a.diags.iter().map(|d| d.message.clone()).chain(a.messages).collect();
            (messages.join("\n"), start.elapsed())
        };
        let share = r"\\phv-test.invalid\share\m.phv";
        if std::os::windows::fs::symlink_file(share, dir.join("lib/m.phv")).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        // An import, a template, an import through a link to a folder on the
        // share, and the same in the object manager's spelling.
        std::os::windows::fs::symlink_dir(r"\\phv-test.invalid\share", dir.join("far")).unwrap();
        std::os::windows::fs::symlink_file(r"\GLOBAL??\UNC\phv-test.invalid\share\n.phv", dir.join("lib/n.phv"))
            .unwrap();
        std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\v.phv.html", dir.join("v.phv.html")).unwrap();
        for (src, shown) in [
            ("<?phv\nimport lib.m.{f};\necho f();\n", "lib/m.phv"),
            ("<?phv\nimport far.m.{f};\necho f();\n", "far/m.phv"),
            ("<?phv\nimport lib.n.{f};\necho f();\n", "lib/n.phv"),
            ("<?phv\necho render(\"v.phv.html\", []);\n", "v.phv.html"),
        ] {
            let (messages, took) = refused(src);
            assert!(
                messages.contains(&format!("{shown} resolves to a file outside the project, on another machine")),
                "{messages}"
            );
            // Resolving the name of a host that does not exist takes seconds.
            assert!(took < std::time::Duration::from_secs(2), "{shown}: {took:?}");
        }
        // The entry itself, a package folder, and a phv.toml that are such links.
        std::os::windows::fs::symlink_file(share, dir.join("link.phv")).unwrap();
        let a = analyze(&dir, &dir.join("link.phv"), HashMap::new());
        assert!(a.messages.iter().any(|m| m.contains("on another machine")), "{:?}", a.messages);
        std::os::windows::fs::symlink_dir(r"\\phv-test.invalid\share", dir.join("vendor/far")).unwrap();
        std::fs::write(dir.join("phv.toml"), "[project]\nname = \"t\"\n[dependencies]\nfar = \"1\"\n").unwrap();
        let (messages, _) = refused("<?phv\nimport far.main.{f};\necho f();\n");
        assert!(messages.contains("the package `far` is on another machine"), "{messages}");
        std::fs::remove_file(dir.join("phv.toml")).unwrap();
        std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\phv.toml", dir.join("phv.toml")).unwrap();
        let e = load_config(&dir).err().unwrap_or_default();
        assert!(e.contains("link to another machine"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
