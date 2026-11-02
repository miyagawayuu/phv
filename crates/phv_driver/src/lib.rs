//! Loads a script with its imports and templates, type-checks it, compiles it
//! to bytecode and runs it.

mod cache;
pub mod config;

pub use cache::compile_cached;
pub use config::{load_config, Config, Csp, DEFAULT_CSP};
use phv_syntax::ast::{Module, StmtKind};
use phv_syntax::{diag, parse_module, Diagnostic, SourceMap, Span};
use phv_types::{CheckInput, ModuleInput, ModuleKind};
use phv_vm::{BuiltinFn, Host, HttpCtx, Limits, Program, TraceLine, Vm, VmError};
use std::collections::HashMap;
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
    /// Native code for int/bool-only functions (see `enable_jit`).
    pub jit: Option<Arc<phv_vm::JitTable>>,
    /// Loaded from the on-disk bytecode cache.
    pub from_cache: bool,
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
    /// Modification time (seconds, nanoseconds) and size, for a quick check.
    mtime: Option<(u64, u32)>,
    len: u64,
    /// SHA-256 of the content that was compiled; None if the file was missing.
    hash: Option<[u8; 32]>,
    /// When the record was made (seconds since the epoch).
    recorded: u64,
}

fn stat(p: &Path) -> Option<(Option<(u64, u32)>, u64)> {
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
    fn new(path: &Path, content: Option<&[u8]>) -> Dep {
        let (mtime, len) = stat(path).unwrap_or((None, 0));
        let recorded = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Dep { path: path.to_path_buf(), mtime, len, hash: content.map(sha256), recorded }
    }

    /// Whether the file still has the recorded content. A different time or size
    /// means changed. Equal ones are trusted only if the file was not modified
    /// close to when it was read (the "racy" case: it may have changed again
    /// within the clock's resolution); otherwise, or when `thorough`, the
    /// content is hashed and compared.
    pub fn is_fresh(&self, thorough: bool) -> bool {
        let Some((mtime, len)) = stat(&self.path) else { return self.hash.is_none() };
        if self.hash.is_none() || mtime != self.mtime || len != self.len {
            return false;
        }
        let racy = mtime.is_none_or(|(s, _)| s + 2 >= self.recorded);
        if !thorough && !racy {
            return true;
        }
        std::fs::read(&self.path).is_ok_and(|bytes| Some(sha256(&bytes)) == self.hash)
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
            out.push_str(m);
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
        if cwd.join(&dir).join("phv.toml").is_file() {
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
        if let Ok(abs) = cwd.join(&start).canonicalize() {
            if let Some(dir) = abs.ancestors().find(|d| d.join("phv.toml").is_file()) {
                return dir.to_path_buf();
            }
        }
    }
    start
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

struct Loader {
    root: PathBuf,
    /// Canonical directories sources may be loaded from: the project and packages.
    allowed: Vec<PathBuf>,
    /// Package name to the directory its modules are loaded from.
    packages: HashMap<String, PathBuf>,
    /// Unsaved editor buffers (for the language server), by canonical path.
    overlay: HashMap<PathBuf, String>,
    sources: SourceMap,
    modules: Vec<(Module, ModuleKind, String)>,
    diags: Vec<Diagnostic>,
    imports: HashMap<String, usize>,
    templates: HashMap<String, usize>,
    by_path: HashMap<PathBuf, usize>,
    deps: Vec<Dep>,
}

impl Loader {
    fn add(&mut self, name: String, path: Option<PathBuf>, text: String, kind: ModuleKind) -> usize {
        if let Some(p) = &path {
            self.deps.push(Dep::new(p, Some(text.as_bytes())));
        }
        let file = self.sources.add(name.clone(), path.clone(), text);
        let (module, mut diags) = parse_module(&self.sources.get(file).text, file, kind == ModuleKind::Template);
        self.diags.append(&mut diags);
        self.modules.push((module, kind, name));
        let idx = self.modules.len() - 1;
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
        // Symlinks and junctions must not pull in files from outside the project.
        if let Ok(c) = path.canonicalize() {
            if !self.allowed.iter().any(|a| c.starts_with(a)) {
                let shown = display_name(&self.root, &path);
                self.diags
                    .push(Diagnostic::error(format!("{what} {shown} resolves to a file outside the project"), span));
                return None;
            }
        }
        let overlaid = path.canonicalize().ok().and_then(|c| self.overlay.get(&c).cloned());
        match overlaid.map(Ok).unwrap_or_else(|| std::fs::read_to_string(&path)) {
            Ok(text) => {
                let name = display_name(&self.root, &path);
                Some(self.add(name, Some(path), text, kind))
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
            if self.imports.contains_key(&name) {
                continue;
            }
            if name.starts_with("std.") || name == "std" {
                match phv_std::std_module(&name) {
                    Some(src) => {
                        let file = format!("<std>/{}.phv", name.trim_start_matches("std."));
                        // Reserve the name first so import cycles terminate.
                        self.imports.insert(name.clone(), usize::MAX);
                        let i = self.add(file, None, src.to_string(), ModuleKind::Std);
                        self.imports.insert(name, i);
                    }
                    None => self.diags.push(Diagnostic::error(format!("unknown standard module `{name}`"), span)),
                }
                continue;
            }
            let segments: Vec<&str> = name.split('.').collect();
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
    deps: Vec<Dep>,
}

/// Directory of each package listed in `[dependencies]`.
pub fn package_dirs(root: &Path, config: &Config) -> HashMap<String, PathBuf> {
    config
        .dependencies
        .iter()
        .map(|(name, dep)| {
            let dir = match &dep.path {
                Some(p) => root.join(p),
                None => root.join("vendor").join(name),
            };
            (name.clone(), dir)
        })
        .collect()
}

/// Loads and type-checks `entry`, using `overlay` (canonical path to text)
/// instead of files on disk where given.
pub fn analyze(root: &Path, entry: &Path, overlay: HashMap<PathBuf, String>) -> Analysis {
    let mut messages = Vec::new();
    let config = load_config(root).unwrap_or_else(|e| {
        messages.push(e);
        Config::default_for(root)
    });
    let packages = package_dirs(root, &config);
    let mut allowed: Vec<PathBuf> = packages.values().filter_map(|d| d.canonicalize().ok()).collect();
    allowed.extend(root.canonicalize().ok());
    let mut loader = Loader {
        root: root.to_path_buf(),
        allowed,
        packages,
        overlay,
        sources: SourceMap::default(),
        modules: Vec::new(),
        diags: Vec::new(),
        imports: HashMap::new(),
        templates: HashMap::new(),
        by_path: HashMap::new(),
        deps: Vec::new(),
    };
    let toml = root.join("phv.toml");
    loader.deps.push(Dep::new(&toml, std::fs::read(&toml).ok().as_deref()));
    loader.add("<std>/prelude.phv".into(), None, phv_std::PRELUDE.to_string(), ModuleKind::Prelude);
    let overlaid = entry.canonicalize().ok().and_then(|c| loader.overlay.get(&c).cloned());
    let text = match overlaid.map(Ok).unwrap_or_else(|| std::fs::read_to_string(entry)) {
        Ok(t) => t,
        Err(e) => {
            messages.push(format!("cannot read {}: {e}", entry.display()));
            return Analysis {
                sources: loader.sources,
                diags: Vec::new(),
                hir: None,
                entry_file: None,
                messages,
                deps: Vec::new(),
            };
        }
    };
    let name = display_name(root, entry);
    let entry_module = loader.add(name, Some(entry.to_path_buf()), text, ModuleKind::Entry);
    let entry_file = Some(loader.modules[entry_module].0.file);
    if loader.diags.iter().any(|d| d.is_error()) {
        return Analysis {
            sources: loader.sources,
            diags: loader.diags,
            hir: None,
            entry_file,
            messages,
            deps: loader.deps,
        };
    }
    let input = CheckInput {
        modules: loader
            .modules
            .iter()
            .map(|(ast, kind, name)| ModuleInput { ast, kind: *kind, name: name.clone() })
            .collect(),
        imports: loader.imports.clone(),
        templates: loader.templates.clone(),
    };
    let (hir, diags) = phv_types::check(&input);
    drop(input);
    loader.diags.extend(diags);
    if config.csp.allows_inline_code() {
        loader.diags.retain(|d| d.code != Some(phv_syntax::diag::CSP_INLINE));
    }
    Analysis { sources: loader.sources, diags: loader.diags, hir: Some(hir), entry_file, messages, deps: loader.deps }
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
    let a = analyze(root, entry, overlay);
    if !a.messages.is_empty() || a.diags.iter().any(|d| d.is_error()) || a.hir.is_none() {
        return Err(CompileError { sources: a.sources, diags: a.diags, messages: a.messages });
    }
    let hir = a.hir.unwrap();
    let mut warnings: Vec<Diagnostic> = a.diags.iter().filter(|d| !d.is_error()).cloned().collect();
    warnings.sort_by_key(|d| (d.span.file, d.span.start));
    let warnings = diag::render(&a.sources, &warnings, false);
    let program = phv_compiler::compile(&hir, &a.sources);
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
}

pub struct RunOutcome {
    pub output: String,
    pub error: Option<RuntimeError>,
    pub http: HttpCtx,
}

/// Runs a compiled program in a fresh VM (a fresh heap).
pub fn run(c: &Compiled, opts: RunOptions) -> RunOutcome {
    run_func(c, c.program.main, opts)
}

/// Runs one function of a compiled program (with no arguments) in a fresh VM.
pub fn run_func(c: &Compiled, func: u32, opts: RunOptions) -> RunOutcome {
    let mut vm = Vm::new(c.program.clone(), c.builtins.clone(), opts.html, opts.limits);
    if let Some(h) = opts.http {
        vm.http = h;
    }
    if let Some(host) = opts.host {
        vm.host = host;
    }
    vm.jit = c.jit.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.call_function(func, &[]).map(|_| ())));
    let error = match result {
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
    };
    let output = vm.take_output();
    let http = std::mem::take(&mut vm.http);
    RunOutcome { output, error, http }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
