//! Loads a script with its imports and templates, type-checks it, compiles it
//! to bytecode and runs it.

use phenix_syntax::ast::{Module, StmtKind};
use phenix_syntax::{diag, parse_module, Diagnostic, SourceMap, Span};
use phenix_types::{CheckInput, ModuleInput, ModuleKind};
use phenix_vm::{BuiltinFn, HttpCtx, Limits, Program, TraceLine, Vm, VmError};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A compiled program ready to run any number of times.
pub struct Compiled {
    pub program: Arc<Program>,
    pub builtins: Arc<[BuiltinFn]>,
    /// Files the program was built from, with their modification times.
    pub deps: Vec<(PathBuf, Option<SystemTime>)>,
    /// Rendered warnings (empty when there are none).
    pub warnings: String,
}

impl Compiled {
    /// True if any source file changed since compilation.
    pub fn is_stale(&self) -> bool {
        self.deps.iter().any(|(p, t)| mtime(p) != *t)
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

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The project root: the nearest directory containing `phenix.toml`, or the
/// directory of the entry file.
pub fn find_root(entry: &Path) -> PathBuf {
    let start = entry.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let mut dir = start.clone();
    loop {
        if dir.join("phenix.toml").is_file() {
            return dir;
        }
        match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => dir = p.to_path_buf(),
            _ => return start,
        }
    }
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
    sources: SourceMap,
    modules: Vec<(Module, ModuleKind, String)>,
    diags: Vec<Diagnostic>,
    imports: HashMap<String, usize>,
    templates: HashMap<String, usize>,
    by_path: HashMap<PathBuf, usize>,
    deps: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Loader {
    fn add(&mut self, name: String, path: Option<PathBuf>, text: String, kind: ModuleKind) -> usize {
        if let Some(p) = &path {
            self.deps.push((p.clone(), mtime(p)));
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
        match std::fs::read_to_string(&path) {
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
                match phenix_std::std_module(&name) {
                    Some(src) => {
                        let file = format!("<std>/{}.phx", name.trim_start_matches("std."));
                        // Reserve the name first so import cycles terminate.
                        self.imports.insert(name.clone(), usize::MAX);
                        let i = self.add(file, None, src.to_string(), ModuleKind::Std);
                        self.imports.insert(name, i);
                    }
                    None => self.diags.push(Diagnostic::error(format!("unknown standard module `{name}`"), span)),
                }
                continue;
            }
            let rel: PathBuf = name.split('.').collect::<PathBuf>().with_extension("phx");
            let path = self.root.join(rel);
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

/// Loads, checks and compiles `entry` (and everything it uses).
pub fn compile(root: &Path, entry: &Path) -> Result<Compiled, CompileError> {
    let mut loader = Loader {
        root: root.to_path_buf(),
        sources: SourceMap::default(),
        modules: Vec::new(),
        diags: Vec::new(),
        imports: HashMap::new(),
        templates: HashMap::new(),
        by_path: HashMap::new(),
        deps: Vec::new(),
    };
    loader.add("<std>/prelude.phx".into(), None, phenix_std::PRELUDE.to_string(), ModuleKind::Prelude);
    let text = match std::fs::read_to_string(entry) {
        Ok(t) => t,
        Err(e) => {
            return Err(CompileError {
                sources: loader.sources,
                diags: Vec::new(),
                messages: vec![format!("cannot read {}: {e}", entry.display())],
            })
        }
    };
    let name = display_name(root, entry);
    loader.add(name, Some(entry.to_path_buf()), text, ModuleKind::Entry);

    let fail = |loader: Loader| CompileError { sources: loader.sources, diags: loader.diags, messages: Vec::new() };
    if loader.diags.iter().any(|d| d.is_error()) {
        return Err(fail(loader));
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
    let (hir, diags) = phenix_types::check(&input);
    drop(input);
    loader.diags.extend(diags);
    if loader.diags.iter().any(|d| d.is_error()) {
        return Err(fail(loader));
    }
    let mut warnings: Vec<Diagnostic> = loader.diags.iter().filter(|d| !d.is_error()).cloned().collect();
    warnings.sort_by_key(|d| (d.span.file, d.span.start));
    let warnings = diag::render(&loader.sources, &warnings, false);
    let program = phenix_compiler::compile(&hir, &loader.sources);
    let builtins = phenix_std::link(&program.builtin_names).map_err(|m| CompileError {
        sources: SourceMap::default(),
        diags: Vec::new(),
        messages: vec![m],
    })?;
    Ok(Compiled { program: Arc::new(program), builtins: builtins.into(), deps: loader.deps, warnings })
}

pub struct RunOptions {
    /// Escape plain strings written by `echo` (web pages).
    pub html: bool,
    pub limits: Limits,
    pub http: Option<HttpCtx>,
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
    let mut vm = Vm::new(c.program.clone(), c.builtins.clone(), opts.html, opts.limits);
    if let Some(h) = opts.http {
        vm.http = h;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run_main()));
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
