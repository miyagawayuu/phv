//! `phv lsp`: a Language Server Protocol server over stdin/stdout.
//! It publishes type errors as you type, shows types on hover, completes
//! names and jumps to definitions; `features` adds references, rename,
//! formatting, signature help, symbols, quick fixes, semantic tokens and
//! inlay hints.

mod features;

/// Diagnostics published for one file at most (errors first); an editor shows no more.
const MAX_DIAGNOSTICS: usize = 1000;

use phv_driver::{analyze, analyze_for_editor, find_root, Analysis};
use phv_syntax::lexer::{lex_with_comments, Mode};
use phv_syntax::token::Tok;
use phv_syntax::{Severity, SourceFile};
use phv_types::index::{Item, ItemKind};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Messages on the stream, with the length of their body: `None` means the
/// stream ended or its framing is broken (nothing more can be read); a message
/// of `None` is well framed but not valid JSON, answered with a parse error.
pub(crate) fn read_message(input: &mut impl BufRead) -> Option<(Option<Value>, usize)> {
    let mut length = None;
    loop {
        let mut line = String::new();
        // Header lines are short; refuse anything else instead of buffering it.
        let n = (&mut *input).take(8192).read_line(&mut line).ok()?;
        if n == 0 || (n == 8192 && !line.ends_with('\n')) {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            length = v.trim().parse::<usize>().ok();
        }
    }
    let n = length?;
    if n > 64 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; n];
    input.read_exact(&mut buf).ok()?;
    Some((serde_json::from_slice(&buf).ok(), n))
}

/// A message from the reader thread (`None` if it is not JSON) and what it costs
/// to hold: its body and the bookkeeping of a small message.
type Incoming = (Option<Value>, usize);

/// What the reader thread holds ahead of the server at most, beyond one message.
const MAX_QUEUED: usize = 64 * 1024 * 1024;

/// Reads messages into `tx` until the stream ends or the server stops listening.
/// The server hands back the cost of each message it takes on `taken`, so that a
/// client writing faster than the server checks waits instead of filling memory.
fn read_messages(input: &mut impl BufRead, tx: &Sender<Incoming>, taken: &Receiver<usize>) {
    let mut queued = 0;
    loop {
        while let Ok(n) = taken.try_recv() {
            queued -= n;
        }
        while queued >= MAX_QUEUED {
            let Ok(n) = taken.recv() else { return };
            queued -= n;
        }
        let Some((msg, n)) = read_message(input) else { return };
        let cost = n + 256;
        queued += cost;
        if tx.send((msg, cost)).is_err() {
            return;
        }
    }
}

/// The server's end of the reader thread.
struct Inbox {
    rx: Receiver<Incoming>,
    taken: Sender<usize>,
}

impl Inbox {
    /// Waits for a message, at most until `until`, then takes every one
    /// already waiting with it. Empty when `until` came first; `None` once the
    /// stream has ended.
    fn batch(&self, until: Option<Instant>) -> Option<Vec<Option<Value>>> {
        let first = match until {
            None => self.rx.recv().ok()?,
            Some(at) => match self.rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(first) => first,
                Err(RecvTimeoutError::Timeout) => return Some(Vec::new()),
                Err(RecvTimeoutError::Disconnected) => return None,
            },
        };
        let mut batch = vec![first];
        batch.extend(self.rx.try_iter());
        Some(
            batch
                .into_iter()
                .map(|(msg, cost)| {
                    let _ = self.taken.send(cost);
                    msg
                })
                .collect(),
        )
    }
}

/// How long after the last change of a document its check waits for the next
/// one: an editor sends a change per keystroke, and checking each would fall
/// behind a fast typist (the check of a half-typed line is not wanted anyway).
/// A document that was opened or saved is checked at once.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Puts `p` on the list of documents to check, at `at`. A change replaces
/// the time (the pending check waits for the next change); an open or a
/// save never delays one.
fn schedule(due: &mut Vec<(PathBuf, Instant)>, p: PathBuf, at: Instant, replace: bool) {
    match due.iter_mut().find(|(d, _)| *d == p) {
        Some((_, t)) => *t = if replace { at } else { (*t).min(at) },
        None => due.push((p, at)),
    }
}

/// Open documents at most; more are ignored (each holds a copy of its text).
const MAX_OPEN: usize = 1000;

/// Cancelled request ids remembered at most (the oldest are forgotten first).
const MAX_CANCELLED: usize = 100;

fn send(out: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

/// The local file a `file:///...` URI names. Anything that could reach another
/// machine or a device is refused: `file://host/share`, UNC paths smuggled in
/// after decoding (`file:///%5C%5Chost%5Cshare`), `\\?\` and `\\.\` paths, and
/// device names such as `CON` or a named pipe.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8().ok()?.into_owned();
    if decoded.contains('\0') {
        return None;
    }
    let path = if cfg!(windows) {
        // file:///C:/x -> C:/x; nothing but a drive letter may start the path.
        let p = decoded.strip_prefix('/')?.replace('\\', "/");
        let b = p.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'/' || p[2..].starts_with("//") {
            return None;
        }
        p
    } else {
        if decoded.starts_with("//") {
            return None;
        }
        decoded
    };
    let path = PathBuf::from(path);
    let names_ok = path.components().all(|c| match c {
        std::path::Component::Normal(n) => n.to_str().is_some_and(phv_std::safefs::valid_name),
        std::path::Component::ParentDir => false,
        _ => true,
    });
    names_ok.then_some(path)
}

pub fn path_to_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches("//?/");
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::CONTROLS.add(b' ').add(b'#').add(b'%').add(b'?').add(b'"').add(b'<').add(b'>');
    let enc = percent_encoding::utf8_percent_encode(s, SET).to_string();
    if enc.starts_with('/') {
        format!("file://{enc}")
    } else {
        format!("file:///{enc}")
    }
}

/// LSP position (line, UTF-16 column) to a byte offset.
fn offset_of(text: &str, line: u64, character: u64) -> u32 {
    let mut offset = 0;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        if i as u64 == line {
            let mut units = 0;
            for (b, ch) in l.char_indices() {
                if units >= character {
                    return (offset + b) as u32;
                }
                units += ch.len_utf16() as u64;
            }
            return (offset + l.trim_end_matches('\n').len()) as u32;
        }
        offset += l.len();
    }
    text.len() as u32
}

/// A byte offset in `text` to an LSP position.
fn text_position(text: &str, offset: usize) -> Value {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let character: usize = before[start..].chars().map(char::len_utf16).sum();
    json!({ "line": line, "character": character })
}

/// `path` with `.` and `..` components removed, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// What is being typed at the cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Want {
    /// After `$`.
    Variable,
    /// After `->`, `?->` or (static) `::`.
    Member { is_static: bool },
    /// A bare name: functions, classes, constants and keywords.
    Name,
}

/// The kind of completion at `offset` and the start of the partial name there.
fn completion_context(text: &str, offset: usize, template: bool) -> Option<(Want, usize)> {
    let before = text.get(..offset)?;
    let partial: usize = before.chars().rev().take_while(|c| is_ident_char(*c)).map(char::len_utf8).sum();
    let start = offset - partial;
    let head = &text[..start];
    let want = if head.ends_with('$') {
        Want::Variable
    } else if head.ends_with("->") {
        Want::Member { is_static: false }
    } else if head.ends_with("::") {
        Want::Member { is_static: true }
    } else if text[start..offset].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    } else {
        Want::Name
    };
    in_code(text, start, template).then_some((want, start))
}

/// Whether `offset` is in PHV code (not in HTML, a string or a comment).
fn in_code(text: &str, offset: usize, template: bool) -> bool {
    let mode = if template { Mode::Html } else { Mode::Code };
    let (toks, comments, _) = lex_with_comments(text, 0, 0, mode);
    let at = offset as u32;
    if comments.iter().any(|c| c.start < at && at <= c.end) {
        return false;
    }
    !toks.iter().any(|t| match t.tok {
        Tok::InlineHtml(_) => t.span.start < at && at <= t.span.end,
        Tok::Str(_) | Tok::Interp(_) => t.span.start < at && at < t.span.end,
        _ => false,
    })
}

const KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "if",
    "implements",
    "import",
    "interface",
    "is",
    "match",
    "new",
    "null",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "static",
    "throw",
    "true",
    "try",
    "use",
    "while",
];

/// Written instead of the partial name, so that `$user->` parses as a member access.
const PLACEHOLDER: &str = "__phv_complete";

fn completion_kind(k: ItemKind) -> u32 {
    match k {
        ItemKind::Function => 3,
        ItemKind::Method => 2,
        ItemKind::Property => 10,
        ItemKind::Class => 7,
        ItemKind::Interface => 8,
        ItemKind::Trait => 7,
        ItemKind::Enum => 13,
        ItemKind::EnumCase => 20,
        ItemKind::Constant => 21,
        ItemKind::Variable => 6,
        ItemKind::Keyword => 14,
    }
}

fn position(file: &SourceFile, offset: u32) -> Value {
    let (line, _) = file.line_col(offset);
    let start = file.line_starts()[line - 1] as usize;
    let end = (offset as usize).min(file.text.len());
    let character: usize = file.text.get(start..end).map(|s| s.chars().map(char::len_utf16).sum()).unwrap_or(0);
    json!({ "line": line - 1, "character": character })
}

/// Open documents larger than source files may be (8 MB) are kept without
/// their text (see `Server::admit`).
pub(super) const MAX_DOCUMENT: usize = 8 << 20;

/// How long an analysis of an open document is used for other requests,
/// when no open document changed meanwhile (files changed on disk by another
/// program are seen after this).
const ANALYSIS_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// An analysis kept for later requests.
struct Kept {
    /// The generation and time it was made.
    generation: u64,
    made: std::time::Instant,
    /// When it was last used (a count of uses, see `Server::uses`).
    used: u64,
    /// Its tokens and bytes of source (all its files together).
    tokens: usize,
    bytes: usize,
    analysis: std::rc::Rc<Analysis>,
}

/// The analyses kept, of all open documents together, have at most this many
/// tokens (see `phv_driver::MAX_PROGRAM_TOKENS`) and bytes of source; the least
/// recently used is dropped first. So the analysis of a program with more is
/// not kept for later requests; the examples of the repository have about
/// 30,000 tokens (found by the 15th assessment for one program, and by the
/// 16th for many open documents: 20 pages held 1,066 MB).
const KEEP_TOKENS: usize = 250_000;
const KEEP_BYTES: usize = 16 << 20;

/// The canonical path of open document `p`. A document behind a link to
/// another machine is not resolved (that would contact the host; found by the
/// 15th assessment), and its text is not used.
fn canonical_doc(p: &Path) -> Option<PathBuf> {
    if phv_std::safefs::on_another_machine_below(&find_root(p), p) {
        return None;
    }
    p.canonicalize().ok()
}

struct Server {
    open: HashMap<PathBuf, String>,
    /// The canonical path of each open document, found when it was opened.
    canonical: HashMap<PathBuf, PathBuf>,
    /// Files we published diagnostics for, per entry document.
    reported: HashMap<PathBuf, HashSet<String>>,
    /// Open documents larger than sources may be: kept without their text, so
    /// no request reads them (found by the 1.0.0 audit).
    too_large: HashSet<PathBuf>,
    /// Counts the changes to open documents: an analysis made before the last
    /// one is not used again.
    generation: u64,
    /// The last analysis (for the editor) of each open document, with the
    /// generation and time it was made: the requests an editor sends after
    /// an edit (tokens, hints, actions, hover...) share it. Within
    /// `KEEP_TOKENS` and `KEEP_BYTES` in all.
    analyses: std::cell::RefCell<HashMap<PathBuf, Kept>>,
    /// Counts the uses of kept analyses (the least recently used is dropped).
    uses: std::cell::Cell<u64>,
    /// The generation and time of the last check of each open document: a
    /// save right after it (no change since, within `ANALYSIS_AGE`) does not
    /// check it again.
    checked: HashMap<PathBuf, (u64, Instant)>,
}

impl Server {
    fn new() -> Server {
        Server {
            open: HashMap::new(),
            canonical: HashMap::new(),
            reported: HashMap::new(),
            too_large: HashSet::new(),
            generation: 0,
            analyses: Default::default(),
            uses: Default::default(),
            checked: HashMap::new(),
        }
    }

    /// The text kept for an open document: none for one larger than sources may
    /// be (it is reported once, by `check`).
    fn admit(&mut self, path: &Path, text: &str) -> String {
        if text.len() > MAX_DOCUMENT {
            self.too_large.insert(path.to_path_buf());
            String::new()
        } else {
            self.too_large.remove(path);
            text.to_string()
        }
    }

    /// Records that open documents changed (cached analyses are dropped).
    fn changed(&mut self) {
        self.generation += 1;
        self.analyses.get_mut().clear();
    }

    fn overlay(&self) -> HashMap<PathBuf, String> {
        self.open
            .iter()
            .filter_map(|(p, t)| {
                let c = self.canonical.get(p).cloned().or_else(|| canonical_doc(p))?;
                Some((c, t.clone()))
            })
            .collect()
    }

    /// The analysis of open document `path` as it is now (shared by the
    /// requests that read it; see `analyses`).
    fn analysis(&self, path: &Path) -> std::rc::Rc<Analysis> {
        let use_ = self.uses.get() + 1;
        self.uses.set(use_);
        {
            let mut analyses = self.analyses.borrow_mut();
            // An analysis that is too old, or from before a change, is dropped (not
            // only passed over).
            analyses.retain(|_, k| k.generation == self.generation && k.made.elapsed() < ANALYSIS_AGE);
            if let Some(k) = analyses.get_mut(path) {
                k.used = use_;
                return k.analysis.clone();
            }
        }
        let text = self.open.get(path).cloned().unwrap_or_default();
        let a = std::rc::Rc::new(self.analyze_text(path, text));
        self.keep(path, &a, use_);
        a
    }

    /// Keeps analysis `a` of `path` for the next requests, dropping the least
    /// recently used ones to stay within `KEEP_TOKENS` and `KEEP_BYTES`. The
    /// analysis of a larger program is not kept after the requests that need
    /// it (found by the 15th assessment: 1.95 GB held between edits).
    fn keep(&self, path: &Path, a: &std::rc::Rc<Analysis>, used: u64) {
        let bytes: usize = a.sources.files.iter().map(|f| f.text.len()).sum();
        let mut analyses = self.analyses.borrow_mut();
        analyses.remove(path);
        if a.tokens > KEEP_TOKENS || bytes > KEEP_BYTES {
            return;
        }
        loop {
            let (tokens, all) = analyses.values().fold((a.tokens, bytes), |(t, b), k| (t + k.tokens, b + k.bytes));
            if tokens <= KEEP_TOKENS && all <= KEEP_BYTES {
                break;
            }
            let Some(oldest) = analyses.iter().min_by_key(|(_, k)| k.used).map(|(p, _)| p.clone()) else { break };
            analyses.remove(&oldest);
        }
        let kept = Kept {
            generation: self.generation,
            made: std::time::Instant::now(),
            used,
            tokens: a.tokens,
            bytes,
            analysis: a.clone(),
        };
        analyses.insert(path.to_path_buf(), kept);
    }

    /// Analyzes `path` as if its text were `text`.
    fn analyze_text(&self, path: &Path, text: String) -> Analysis {
        let mut overlay = self.overlay();
        if let Some(c) = canonical_doc(path) {
            overlay.insert(c, text);
        }
        analyze_for_editor(&find_root(path), path, overlay)
    }

    fn completion(&self, path: &Path, line: u64, character: u64, invoked: bool) -> Value {
        let Some(text) = self.open.get(path) else {
            return Value::Null;
        };
        let template = path.to_string_lossy().ends_with(".phv.html");
        let offset = offset_of(text, line, character) as usize;
        let Some((want, start)) = completion_context(text, offset, template) else {
            return json!([]);
        };
        // The name being edited, up to its end (the cursor may be in the middle of it).
        let end = offset + text[offset..].chars().take_while(|c| is_ident_char(*c)).map(char::len_utf8).sum::<usize>();
        if want == Want::Name && start == offset && !invoked {
            return json!([]);
        }
        let items: Vec<Item> = match want {
            Want::Member { .. } => {
                // Try the line as it is, then with a `;` in case the statement is unfinished.
                let mut found = None;
                for tail in ["", ";"] {
                    let patched = format!("{}{PLACEHOLDER}{tail}{}", &text[..start], &text[end..]);
                    let a = self.analyze_text(path, patched);
                    let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { continue };
                    if let Some(items) = ix.members_at(file, start as u32) {
                        found = Some(items);
                        break;
                    }
                }
                found.unwrap_or_default()
            }
            Want::Variable => {
                // Use the first version in which the checker reached `$__phv_complete`
                // (it reports it as undefined), so its function is known.
                let mut found = Vec::new();
                for tail in ["", ";"] {
                    let patched = format!("{}{PLACEHOLDER}{tail}{}", &text[..start], &text[end..]);
                    let a = self.analyze_text(path, patched);
                    let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { continue };
                    let dollar = start as u32 - 1;
                    found = ix.variables_at(file, start as u32);
                    if a.diags.iter().any(|d| d.span.file == file && d.span.start == dollar) {
                        break;
                    }
                }
                found
            }
            Want::Name => {
                let a = self.analysis(path);
                let mut items = a.index.as_ref().map(|ix| ix.globals.clone()).unwrap_or_default();
                items.extend(KEYWORDS.iter().map(|k| Item {
                    name: k.to_string(),
                    kind: ItemKind::Keyword,
                    detail: String::new(),
                    def: None,
                }));
                items
            }
        };
        let range = json!({ "start": text_position(text, start), "end": text_position(text, end) });
        let call_follows = text[end..].starts_with('(');
        let out: Vec<Value> = items
            .into_iter()
            .filter(|i| i.name != PLACEHOLDER)
            .map(|i| {
                let callable = matches!(i.kind, ItemKind::Function | ItemKind::Method);
                let (new_text, snippet) = if callable && !call_follows {
                    if i.detail.starts_with("()") {
                        (format!("{}()", i.name), false)
                    } else {
                        (format!("{}($1)$0", i.name), true)
                    }
                } else {
                    (i.name.clone(), false)
                };
                let label = if i.kind == ItemKind::Variable { format!("${}", i.name) } else { i.name.clone() };
                json!({
                    "label": label,
                    "kind": completion_kind(i.kind),
                    "detail": i.detail,
                    "filterText": i.name,
                    "insertTextFormat": if snippet { 2 } else { 1 },
                    "textEdit": { "range": range, "newText": new_text },
                })
            })
            .collect();
        json!(out)
    }

    fn definition(&self, path: &Path, line: u64, character: u64) -> Value {
        let Some(text) = self.open.get(path) else {
            return Value::Null;
        };
        let offset = offset_of(text, line, character);
        let a = self.analysis(path);
        let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { return Value::Null };
        let Some(def) = ix.definition(file, offset) else { return Value::Null };
        let src = a.sources.get(def.file);
        // Built-in modules have no file to open.
        let Some(p) = &src.path else { return Value::Null };
        // Without `.` and `..` (`dir/../x` is a URI the editor could not send back), but
        // otherwise spelled as the editor knows it (not resolved through links or short names).
        let p = normalize(p);
        json!({
            "uri": path_to_uri(&p),
            "range": { "start": position(src, def.start), "end": position(src, def.end) },
        })
    }

    fn check(&mut self, path: &Path, out: &mut impl Write) {
        if self.too_large.contains(path) {
            let message = format!("this document is larger than {} MB and is not checked", MAX_DOCUMENT >> 20);
            send(
                out,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": path_to_uri(path), "diagnostics": [{
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                        "severity": 1, "source": "phv", "message": message,
                    }] },
                }),
            );
            return;
        }
        if path.to_string_lossy().ends_with(".phv.html") {
            return; // templates are checked through the pages that render them
        }
        // Checked already as it is now (a save right after the check of the
        // last change): the diagnostics published stand.
        if self.checked.get(path).is_some_and(|(g, at)| *g == self.generation && at.elapsed() < ANALYSIS_AGE) {
            return;
        }
        let root = find_root(path);
        let a = analyze(&root, path, self.overlay());
        self.checked.insert(path.to_path_buf(), (self.generation, Instant::now()));
        let mut by_file: HashMap<String, Vec<Value>> = HashMap::new();
        let entry_uri = path_to_uri(path);
        by_file.entry(entry_uri.clone()).or_default();
        for m in &a.messages {
            by_file.entry(entry_uri.clone()).or_default().push(json!({
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 1, "source": "phv", "message": m,
            }));
        }
        // Per file: errors first, at most MAX_DIAGNOSTICS, and their positions found
        // in one pass over the text (one long line of many warnings was quadratic).
        let mut per_file: HashMap<u32, Vec<&phv_syntax::Diagnostic>> = HashMap::new();
        for d in &a.diags {
            per_file.entry(d.span.file).or_default().push(d);
        }
        for (fid, mut diags) in per_file {
            let file = a.sources.get(fid);
            let Some(fpath) = &file.path else { continue };
            diags.sort_by_key(|d| (d.severity != Severity::Error, d.span.start));
            diags.truncate(MAX_DIAGNOSTICS);
            let offsets = diags.iter().flat_map(|d| [d.span.start, d.span.end.max(d.span.start)]);
            let at = features::positions_of(&file.text, offsets);
            let pos = |o: u32| {
                at.get(&o).map_or(json!({ "line": 0, "character": 0 }), |(l, c)| json!({ "line": l, "character": c }))
            };
            let list = by_file.entry(path_to_uri(fpath)).or_default();
            for d in diags {
                let mut message = d.message.clone();
                for n in &d.notes {
                    message.push_str("\nnote: ");
                    message.push_str(n);
                }
                list.push(json!({
                    "range": { "start": pos(d.span.start), "end": pos(d.span.end.max(d.span.start)) },
                    "severity": if d.severity == Severity::Error { 1 } else { 2 },
                    "source": "phv",
                    "message": message,
                }));
            }
        }
        let previous = self.reported.remove(path).unwrap_or_default();
        for uri in previous {
            by_file.entry(uri).or_default();
        }
        let now: HashSet<String> = by_file.iter().filter(|(_, v)| !v.is_empty()).map(|(k, _)| k.clone()).collect();
        for (uri, diagnostics) in by_file {
            send(
                out,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": diagnostics },
                }),
            );
        }
        self.reported.insert(path.to_path_buf(), now);
    }

    fn hover(&self, path: &Path, line: u64, character: u64) -> Value {
        // Only documents the editor opened; other files are not read on request.
        let Some(text) = self.open.get(path).cloned() else {
            return Value::Null;
        };
        let a = self.analysis(path);
        let (Some(hir), Some(file)) = (&a.hir, a.entry_file) else { return Value::Null };
        let offset = offset_of(&text, line, character);
        match phv_types::query::type_at(hir, file, offset) {
            Some((span, ty)) => {
                let src = a.sources.get(file);
                json!({
                    "contents": { "kind": "markdown", "value": format!("```phv\n{ty}\n```") },
                    "range": { "start": position(src, span.start), "end": position(src, span.end) },
                })
            }
            None => Value::Null,
        }
    }
}

pub fn run() {
    let (tx, rx) = mpsc::channel();
    let (taken, taken_rx) = mpsc::channel();
    std::thread::spawn(move || read_messages(&mut std::io::stdin().lock(), &tx, &taken_rx));
    let stdout = std::io::stdout();
    if let Some(code) = serve(&Inbox { rx, taken }, &mut stdout.lock()) {
        std::process::exit(code);
    }
}

/// Runs `f`, answering `id` with an internal error if it panics: a bug in the
/// checker must not take the editor's language server down.
fn guarded<W: Write, T>(out: &mut W, id: Option<&Value>, f: impl FnOnce(&mut W) -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut *out))) {
        Ok(v) => Some(v),
        Err(_) => {
            if let Some(id) = id {
                send(
                    out,
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": "internal error" } }),
                );
            }
            None
        }
    }
}

fn doc_path(params: &Value) -> Option<PathBuf> {
    params["textDocument"]["uri"].as_str().and_then(uri_to_path)
}

/// Answers messages until the stream ends (`None`) or `exit` arrives (its exit code).
/// Every message already waiting is taken at once: text changes first, then the
/// rest in the order they came (so requests see the newest text), then the checks
/// that are due: one per document that was opened or saved, and one per changed
/// document `DEBOUNCE` after its last change, however many edits arrived
/// meanwhile. The wait for the next message ends when a check comes due.
fn serve(inbox: &Inbox, out: &mut impl Write) -> Option<i32> {
    let mut server = Server::new();
    let mut shutting_down = false;
    let mut cancelled: VecDeque<Value> = VecDeque::new();
    // Documents to check and when, in the order they were put there.
    let mut due: Vec<(PathBuf, Instant)> = Vec::new();
    loop {
        let batch = inbox.batch(due.iter().map(|(_, at)| *at).min())?;
        for msg in batch.iter().flatten() {
            let params = &msg["params"];
            guarded(out, None, |_| match msg["method"].as_str().unwrap_or("") {
                "textDocument/didOpen" => {
                    if let Some(p) =
                        doc_path(params).filter(|p| server.open.contains_key(p) || server.open.len() < MAX_OPEN)
                    {
                        let text = params["textDocument"]["text"].as_str().unwrap_or("");
                        if let Some(c) = canonical_doc(&p) {
                            server.canonical.insert(p.clone(), c);
                        }
                        let text = server.admit(&p, text);
                        server.open.insert(p.clone(), text);
                        server.changed();
                        schedule(&mut due, p, Instant::now(), false);
                    }
                }
                "textDocument/didChange" => {
                    if let Some(p) = doc_path(params).filter(|p| server.open.contains_key(p)) {
                        if let Some(text) =
                            params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str())
                        {
                            let text = server.admit(&p, text);
                            server.open.insert(p.clone(), text);
                        }
                        server.changed();
                        schedule(&mut due, p, Instant::now() + DEBOUNCE, true);
                    }
                }
                "textDocument/didClose" => {
                    if let Some(p) = doc_path(params) {
                        server.open.remove(&p);
                        server.too_large.remove(&p);
                        server.canonical.remove(&p);
                        server.reported.remove(&p);
                        server.checked.remove(&p);
                        server.changed();
                        due.retain(|(d, _)| *d != p);
                    }
                }
                "$/cancelRequest" => {
                    let id = &params["id"];
                    if (id.is_number() || id.is_string()) && !cancelled.contains(id) {
                        if cancelled.len() == MAX_CANCELLED {
                            cancelled.pop_front();
                        }
                        cancelled.push_back(id.clone());
                    }
                }
                _ => {}
            });
        }
        for msg in &batch {
            let Some(msg) = msg else {
                send(
                    out,
                    &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }),
                );
                continue;
            };
            let method = msg["method"].as_str().unwrap_or("");
            let id = msg.get("id");
            let params = &msg["params"];
            match method {
                "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didClose" | "$/cancelRequest" => {
                    continue
                }
                "exit" => return Some(if shutting_down { 0 } else { 1 }),
                _ => {}
            }
            if let Some(i) = id.and_then(|id| cancelled.iter().position(|c| c == id)) {
                cancelled.remove(i);
                send(
                    out,
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32800, "message": "request cancelled" } }),
                );
                continue;
            }
            let position = || {
                (
                    params["position"]["line"].as_u64().unwrap_or(0),
                    params["position"]["character"].as_u64().unwrap_or(0),
                )
            };
            let result = guarded(out, id, |out| match method {
                "initialize" => Some(json!({
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 1, "save": { "includeText": false } },
                        "hoverProvider": true,
                        "completionProvider": { "triggerCharacters": ["$", ">", ":"] },
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": { "prepareProvider": true },
                        "documentFormattingProvider": true,
                        "signatureHelpProvider": { "triggerCharacters": ["(", ","], "retriggerCharacters": [","] },
                        "documentSymbolProvider": true,
                        "workspaceSymbolProvider": true,
                        "codeActionProvider": { "codeActionKinds": ["quickfix"] },
                        "semanticTokensProvider": { "legend": features::token_legend(), "full": true },
                        "inlayHintProvider": true,
                    },
                    "serverInfo": { "name": "phv", "version": env!("CARGO_PKG_VERSION") },
                })),
                "shutdown" => {
                    shutting_down = true;
                    Some(Value::Null)
                }
                "textDocument/didSave" => {
                    // Only documents the editor opened are checked.
                    if let Some(p) = doc_path(params).filter(|p| server.open.contains_key(p)) {
                        schedule(&mut due, p, Instant::now(), false);
                    }
                    None
                }
                "textDocument/completion" => Some(match doc_path(params) {
                    Some(p) => {
                        let (line, character) = position();
                        server.completion(&p, line, character, params["context"]["triggerKind"].as_u64() == Some(1))
                    }
                    None => Value::Null,
                }),
                "textDocument/definition" => Some(match doc_path(params) {
                    Some(p) => {
                        let (line, character) = position();
                        server.definition(&p, line, character)
                    }
                    None => Value::Null,
                }),
                "textDocument/hover" => Some(match doc_path(params) {
                    Some(p) => {
                        let (line, character) = position();
                        server.hover(&p, line, character)
                    }
                    None => Value::Null,
                }),
                "textDocument/references" => Some(match doc_path(params) {
                    Some(p) => {
                        let (line, character) = position();
                        let decl = params["context"]["includeDeclaration"].as_bool().unwrap_or(true);
                        server.references(&p, line, character, decl)
                    }
                    None => Value::Null,
                }),
                "textDocument/prepareRename" | "textDocument/rename" => {
                    let (line, character) = position();
                    let answer = match doc_path(params) {
                        None => Err("the document is not open".to_string()),
                        Some(p) if method == "textDocument/prepareRename" => server.prepare_rename(&p, line, character),
                        Some(p) => server.rename(&p, line, character, params["newName"].as_str().unwrap_or("")),
                    };
                    match answer {
                        Ok(v) => Some(v),
                        Err(message) => {
                            send(
                                out,
                                &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32803, "message": message } }),
                            );
                            None
                        }
                    }
                }
                "textDocument/formatting" => Some(doc_path(params).map_or(Value::Null, |p| server.formatting(&p))),
                "textDocument/signatureHelp" => Some(match doc_path(params) {
                    Some(p) => {
                        let (line, character) = position();
                        server.signature_help(&p, line, character)
                    }
                    None => Value::Null,
                }),
                "textDocument/documentSymbol" => {
                    Some(doc_path(params).map_or(Value::Null, |p| server.document_symbols(&p)))
                }
                "workspace/symbol" => Some(server.workspace_symbols(params["query"].as_str().unwrap_or(""))),
                "textDocument/codeAction" => {
                    Some(doc_path(params).map_or(json!([]), |p| server.code_actions(&p, &params["range"])))
                }
                "textDocument/semanticTokens/full" => {
                    Some(doc_path(params).map_or(Value::Null, |p| server.semantic_tokens(&p)))
                }
                "textDocument/inlayHint" => {
                    Some(doc_path(params).map_or(json!([]), |p| server.inlay_hints(&p, &params["range"])))
                }
                _ => {
                    if id.is_some() {
                        send(
                            out,
                            &json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601, "message": format!("method not found: {method}") },
                            }),
                        );
                    }
                    None
                }
            });
            if let (Some(id), Some(Some(result))) = (id, result) {
                send(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
        }
        let now = Instant::now();
        let mut later = Vec::new();
        for (p, at) in std::mem::take(&mut due) {
            if at <= now {
                guarded(out, None, |out| server.check(&p, out));
            } else {
                later.push((p, at));
            }
        }
        due = later;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPES: &str = "export class Shape {\n    private int $secret = 0;\n\
        \x20   public function __construct(public string $name) {}\n\
        \x20   public function area(): float { return 0.0; }\n\
        \x20   public static function unit(): Shape { return new Shape(\"unit\"); }\n}\n\
        export enum Color { case Red; case Green; }\n\
        export function make(string $name): Shape { return new Shape($name); }\n";

    /// A project in a temporary directory with `lib/shapes.phv` and a template;
    /// each call opens `text` (`|` marks the cursor) as `page.phv` or `view.phv.html`.
    struct Project {
        dir: PathBuf,
    }

    impl Project {
        fn new(name: &str) -> Project {
            let dir = std::env::temp_dir().join(format!("phv-lsp-{name}-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("lib")).unwrap();
            std::fs::write(dir.join("phv.toml"), "[project]\nname = \"t\"\n").unwrap();
            std::fs::write(dir.join("lib/shapes.phv"), SHAPES).unwrap();
            std::fs::write(dir.join("page.phv"), "").unwrap();
            std::fs::write(dir.join("view.phv.html"), "").unwrap();
            Project { dir }
        }

        fn open(&self, file: &str, text: &str) -> (Server, PathBuf, u64, u64) {
            let cursor = text.find('|').unwrap();
            let text = text.replacen('|', "", 1);
            let before = &text[..cursor];
            let line = before.matches('\n').count() as u64;
            let character = before[before.rfind('\n').map_or(0, |i| i + 1)..].encode_utf16().count() as u64;
            let path = self.dir.join(file);
            let mut server = Server::new();
            server.open.insert(path.clone(), text);
            (server, path, line, character)
        }

        fn complete(&self, file: &str, text: &str) -> Vec<String> {
            let (server, path, line, character) = self.open(file, text);
            let result = server.completion(&path, line, character, true);
            result.as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap().to_string()).collect()
        }

        /// The file name and the text of the definition found at the cursor.
        fn definition(&self, file: &str, text: &str) -> Option<(String, String)> {
            let (server, path, line, character) = self.open(file, text);
            let loc = server.definition(&path, line, character);
            let target = uri_to_path(loc["uri"].as_str()?)?;
            let content = server.open.get(&target).cloned().or_else(|| std::fs::read_to_string(&target).ok())?;
            let start = offset_of(
                &content,
                loc["range"]["start"]["line"].as_u64()?,
                loc["range"]["start"]["character"].as_u64()?,
            );
            let end =
                offset_of(&content, loc["range"]["end"]["line"].as_u64()?, loc["range"]["end"]["character"].as_u64()?);
            let name = target.file_name()?.to_string_lossy().into_owned();
            Some((name, content[start as usize..end as usize].to_string()))
        }
    }

    /// What the server writes when `msgs` all arrive before it reads the first.
    fn serve_batch(msgs: Vec<Value>) -> Vec<Value> {
        let (tx, rx) = mpsc::channel();
        for msg in msgs {
            tx.send((Some(msg), 0)).unwrap();
        }
        drop(tx);
        let (taken, _) = mpsc::channel();
        let mut out = Vec::new();
        assert_eq!(serve(&Inbox { rx, taken }, &mut out), None);
        let mut input = &out[..];
        std::iter::from_fn(|| read_message(&mut input).map(|(msg, _)| msg.unwrap())).collect()
    }

    fn notification(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    }

    /// What the server writes, one frame per message, with the time of each.
    struct Timed {
        buf: Vec<u8>,
        frames: Vec<(Instant, Vec<u8>)>,
    }

    impl Write for Timed {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if !self.buf.is_empty() {
                self.frames.push((Instant::now(), std::mem::take(&mut self.buf)));
            }
            Ok(())
        }
    }

    /// Runs the server on its own thread and sends it `msgs`, each after its
    /// delay (from the one before), then ends the stream `settle` after the
    /// last: what it wrote, with the time of each message, and when each
    /// message was sent.
    fn serve_timed(msgs: Vec<(Duration, Value)>, settle: Duration) -> (Vec<(Instant, Value)>, Vec<Instant>) {
        let (tx, rx) = mpsc::channel();
        let (taken, _) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut out = Timed { buf: Vec::new(), frames: Vec::new() };
            assert_eq!(serve(&Inbox { rx, taken }, &mut out), None);
            out.frames
        });
        let mut sent = Vec::new();
        for (delay, msg) in msgs {
            std::thread::sleep(delay);
            sent.push(Instant::now());
            tx.send((Some(msg), 0)).unwrap();
        }
        std::thread::sleep(settle);
        drop(tx);
        let frames = server.join().unwrap();
        let got =
            frames.into_iter().map(|(at, bytes)| (at, read_message(&mut &bytes[..]).unwrap().0.unwrap())).collect();
        (got, sent)
    }

    /// A burst of changes (20, 10 ms apart, as typing) is checked once,
    /// `DEBOUNCE` after the last one and within a second of it, and a save
    /// right after a check does not check the document again.
    #[test]
    fn checks_a_burst_of_edits_once() {
        let p = Project::new("burst");
        let uri = path_to_uri(&p.dir.join("page.phv"));
        let mut msgs = vec![(
            Duration::ZERO,
            notification(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": "phv", "version": 0, "text": "" } }),
            ),
        )];
        for i in 1..=20 {
            msgs.push((
                Duration::from_millis(10),
                notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": i },
                        "contentChanges": [{ "text": format!("<?phv\necho $missing{i};\n") }],
                    }),
                ),
            ));
        }
        msgs.push((
            Duration::from_millis(600),
            notification("textDocument/didSave", json!({ "textDocument": { "uri": uri } })),
        ));
        let (got, sent) = serve_timed(msgs, Duration::from_millis(400));
        let published: Vec<&(Instant, Value)> =
            got.iter().filter(|(_, m)| m["method"] == "textDocument/publishDiagnostics").collect();
        // The open at once, the burst once (a gap that a loaded machine stretched
        // past DEBOUNCE adds a check), and nothing for the save.
        let stretched = sent[1..21].windows(2).filter(|w| w[1] - w[0] >= DEBOUNCE).count();
        assert_eq!(published.len(), 2 + stretched, "{got:?}");
        // (That the open is checked at once, not after DEBOUNCE, is seen in
        // `answers_cancelled_requests_with_an_error`: the stream ends right away.)
        let (opened, _) = published[0];
        let (at, msg) = published.last().unwrap();
        assert_eq!(msg["params"]["uri"], uri);
        let diags = msg["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0]["message"].as_str().unwrap().contains("$missing20"), "{diags:?}");
        let last_change = sent[20];
        assert!(*at >= last_change + DEBOUNCE, "checked {:?} after the last change", *at - last_change);
        // Within a second (a debug build on a loaded machine: a few checks' worth).
        let one_check = *opened - sent[0];
        assert!(*at - last_change < Duration::from_secs(1).max(one_check * 4), "took {:?}", *at - last_change);
        assert!(*at < sent[21], "the save came before the check");
    }

    #[test]
    fn answers_cancelled_requests_with_an_error() {
        let p = Project::new("cancel");
        let uri = path_to_uri(&p.dir.join("page.phv"));
        let complete = |id: u64| {
            json!({
                "jsonrpc": "2.0", "id": id, "method": "textDocument/completion",
                "params": { "textDocument": { "uri": uri }, "position": { "line": 1, "character": 2 },
                            "context": { "triggerKind": 1 } },
            })
        };
        let got = serve_batch(vec![
            notification(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": "phv", "version": 0, "text": "<?phv\nma\n" } }),
            ),
            complete(1),
            complete(2),
            notification("$/cancelRequest", json!({ "id": 1 })),
        ]);
        let reply = |id: u64| got.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(reply(1)["error"]["code"], -32800);
        let labels = reply(2)["result"].as_array().unwrap();
        assert!(labels.iter().any(|i| i["label"] == "match"), "{labels:?}");
        // The two answers come first, then the diagnostics of the opened document.
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[2]["method"], "textDocument/publishDiagnostics");
    }

    /// A file whose traits reach one trait along 2^40 paths (40 levels of
    /// `trait Ai { use Ai-1, Bi-1; }`, 2 KB) is checked at once: with it open,
    /// a hover in another file is answered within the deadline. Each path was
    /// walked, and the server answered nothing for days.
    #[test]
    fn answers_with_a_diamond_of_traits_open() {
        let p = Project::new("diamond");
        let mut bomb = String::from("<?phv\ntrait A0 {}\ntrait B0 {}\n");
        for i in 1..=40 {
            bomb += &format!(
                "trait A{i} {{ use A{}, B{}; }}\ntrait B{i} {{ use A{}, B{}; }}\n",
                i - 1,
                i - 1,
                i - 1,
                i - 1
            );
        }
        bomb += "class K { use A40; }\n";
        let bomb_uri = path_to_uri(&p.dir.join("bomb.phv"));
        let uri = path_to_uri(&p.dir.join("page.phv"));
        let open = |uri: &str, text: &str| {
            notification(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": "phv", "version": 0, "text": text } }),
            )
        };
        let msgs = vec![
            open(&bomb_uri, &bomb),
            open(&uri, "<?phv\n$name = \"x\";\necho $name;\n"),
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "textDocument/hover",
                "params": { "textDocument": { "uri": uri }, "position": { "line": 2, "character": 6 } },
            }),
        ];
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let start = Instant::now();
            let got = serve_batch(msgs);
            let _ = tx.send((got, start.elapsed()));
        });
        let (got, took) = rx.recv_timeout(Duration::from_secs(30)).expect("no answer in 30 s");
        assert!(took < Duration::from_secs(10), "took {took:?}");
        let hover = got.iter().find(|m| m["id"] == 1).unwrap();
        assert!(hover["result"]["contents"]["value"].as_str().unwrap_or_default().contains("string"), "{hover}");
    }

    #[test]
    fn reads_on_its_own_thread_until_exit() {
        let mut stream = Vec::new();
        for msg in [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
            json!({ "jsonrpc": "2.0", "method": "exit" }),
        ] {
            send(&mut stream, &msg);
        }
        stream.extend_from_slice(b"Content-Length: 3\r\n\r\n{\"x");
        let (tx, rx) = mpsc::channel();
        let (taken, taken_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || read_messages(&mut &stream[..], &tx, &taken_rx));
        let mut out = Vec::new();
        assert_eq!(serve(&Inbox { rx, taken }, &mut out), Some(0));
        reader.join().unwrap();
        let mut output = &out[..];
        let got: Vec<Value> = std::iter::from_fn(|| read_message(&mut output).map(|(m, _)| m.unwrap())).collect();
        assert_eq!(got.iter().map(|m| m["id"].clone()).collect::<Vec<_>>(), [json!(1), json!(2)]);
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn completes_members() {
        let p = Project::new("members");
        let import = "<?phv\nimport lib.shapes.{Shape, Color, make};\n";
        // The statement is unfinished: the next line follows without a `;`.
        let got = p.complete("page.phv", &format!("{import}$s = make(\"a\");\n$s->|\necho 1;\n"));
        assert_eq!(got, ["name", "area"]);
        let got = p.complete("page.phv", &format!("{import}echo make(\"a\")->na|;\n"));
        assert_eq!(got, ["name", "area"]);
        let got = p.complete("page.phv", &format!("{import}$c = Color::|"));
        assert_eq!(got, ["Red", "Green"]);
        let got = p.complete("page.phv", &format!("{import}$u = Shape::|"));
        assert_eq!(got, ["unit"]);
        let got = p.complete("page.phv", "<?phv\n$t = \"x\";\necho $t->|;\n");
        assert!(got.contains(&"trim".to_string()) && got.contains(&"len".to_string()), "{got:?}");
        // Private members are listed inside the class only.
        let got = p.complete(
            "page.phv",
            "class A {\n    private int $x = 1;\n    public function f(): int { return $this->|; }\n}\n",
        );
        assert_eq!(got, ["x", "f"]);
    }

    /// Members of a generic class show the receiver's type arguments.
    #[test]
    fn completes_members_of_generic_classes() {
        let p = Project::new("generic");
        let src = "<?phv\nclass Box<T> {\n    public function __construct(public T $value) {}\n    public function get(): T { return $this->value; }\n}\nclass Named extends Box<string> {}\n$b = new Box(1);\n$n = new Named(\"x\");\n";
        let (server, path, line, character) = p.open("page.phv", &format!("{src}$b->|\necho 1;\n"));
        let items = server.completion(&path, line, character, true);
        let detail = |items: &Value, name: &str| {
            let item = items.as_array().unwrap().iter().find(|i| i["label"] == name).cloned();
            item.map(|i| i["detail"].as_str().unwrap_or_default().to_string()).unwrap_or_default()
        };
        assert_eq!(detail(&items, "get"), "(): int");
        assert_eq!(detail(&items, "value"), "int");
        let (server, path, line, character) = p.open("page.phv", &format!("{src}$n->|\necho 1;\n"));
        let items = server.completion(&path, line, character, true);
        assert_eq!(detail(&items, "get"), "(): string");
    }

    #[test]
    fn completes_variables_in_scope() {
        let p = Project::new("vars");
        let got = p.complete(
            "page.phv",
            "<?phv\n$a = 1;\nfunction f(string $param): int {\n    $local = 2;\n    return $|;\n}\n$b = 2;\n",
        );
        assert_eq!(got, ["$param", "$local"]);
        let got = p.complete("page.phv", "<?phv\n$a = 1;\necho $|\n$later = 2;\n");
        assert_eq!(got, ["$a"]);
        // Arrow functions see the variables around them.
        let got = p.complete("page.phv", "<?phv\n$outer = 1;\n$xs = [1, 2]->map(fn(int $x) => $x + $|);\n");
        assert!(got.starts_with(&["$x".to_string(), "$outer".to_string()]), "{got:?}");
        // A template opened on its own knows its parameters.
        let got = p.complete("view.phv.html", "<?phv params(string $title, int $count) ?>\n<h1><?= $| ?></h1>\n");
        assert_eq!(got, ["$title", "$count"]);
    }

    #[test]
    fn completes_names_only_in_code() {
        let p = Project::new("names");
        let got = p.complete("page.phv", "<?phv\nimport lib.shapes.{make};\nma|\n");
        assert!(got.contains(&"make".to_string()) && got.contains(&"match".to_string()), "{got:?}");
        assert!(p.complete("page.phv", "<?phv ?>\n<p>He|llo</p>\n").is_empty());
        assert!(p.complete("page.phv", "<?phv\n$s = \"ab|c\";\n").is_empty());
        assert!(p.complete("page.phv", "<?phv\n// a com|ment\n").is_empty());
        assert!(p.complete("view.phv.html", "<p>Hel|lo</p>\n").is_empty());
    }

    #[test]
    fn finds_definitions() {
        let p = Project::new("defs");
        let import = "<?phv\nimport lib.shapes.{Shape, Color, make};\n";
        let def = |text: &str| p.definition("page.phv", &format!("{import}{text}"));
        let shapes = |s: &str| Some(("shapes.phv".to_string(), s.to_string()));
        assert_eq!(def("$s = ma|ke(\"a\");\n"), shapes("make"));
        assert_eq!(def("$s = new Sh|ape(\"a\");\n"), shapes("Shape"));
        assert_eq!(def("echo make(\"a\")->ar|ea();\n"), shapes("area"));
        assert_eq!(def("echo make(\"a\")->na|me;\n"), shapes("public string $name"));
        assert_eq!(def("$c = Color::Gr|een;\n"), shapes("case Green;"));
        assert_eq!(def("$u = Shape::un|it();\n"), shapes("unit"));
        assert_eq!(
            p.definition("page.phv", "<?phv\n$total = 1;\n$total = 2;\necho $to|tal;\n"),
            Some(("page.phv".into(), "$total".into()))
        );
        assert_eq!(
            p.definition("page.phv", "<?phv\necho render(\"vi|ew.phv.html\");\n"),
            Some(("view.phv.html".into(), String::new()))
        );
        assert_eq!(def("echo 1|;\n"), None);
    }

    /// The members a class takes from a trait are completed through the class
    /// (private ones inside it only), and lead to their declaration in the
    /// trait; so does the trait's name in `use`. Hover gives the type the
    /// member has in the class.
    #[test]
    fn completes_and_finds_trait_members_through_the_class() {
        let p = Project::new("traits");
        let named = "export trait Named {\n    public string $label = \"x\";\n    public function title(): string {\n        return $this->label;\n    }\n    private function hidden(): int {\n        return 1;\n    }\n}\n";
        std::fs::write(p.dir.join("lib/named.phv"), named).unwrap();
        let src = "<?phv\nimport lib.named.Named;\nclass Page {\n    use Named;\n    public function own(): int {\n        return 2;\n    }\n}\n$p = new Page();\n";
        let got = p.complete("page.phv", &format!("{src}$p->|\necho 1;\n"));
        assert_eq!(got, ["label", "own", "title"]);
        let inside = "<?phv\nimport lib.named.Named;\nclass Page {\n    use Named;\n    public function own(): int {\n        return $this->|;\n    }\n}\n";
        assert_eq!(p.complete("page.phv", inside), ["label", "hidden", "own", "title"]);
        let named_at = |s: &str| Some(("named.phv".to_string(), s.to_string()));
        assert_eq!(p.definition("page.phv", &format!("{src}echo $p->ti|tle();\n")), named_at("title"));
        assert_eq!(
            p.definition("page.phv", &format!("{src}echo $p->lab|el;\n")),
            named_at("public string $label = \"x\";")
        );
        let use_at = src.replace("use Named;", "use Na|med;");
        assert_eq!(p.definition("page.phv", &use_at), named_at("Named"));
        let (server, path, line, character) = p.open("page.phv", &format!("{src}$t = $p->tit|le();\n"));
        let hover = server.hover(&path, line, character);
        assert!(hover["contents"]["value"].as_str().unwrap_or_default().contains("string"), "{hover}");
    }

    #[test]
    fn converts_positions() {
        let text = "ab\nこんにちは😀x\n";
        assert_eq!(offset_of(text, 1, 0), 3);
        assert_eq!(offset_of(text, 1, 5), 3 + 15);
        assert_eq!(offset_of(text, 1, 7), 3 + 15 + 4);
        let p = PathBuf::from(if cfg!(windows) { "C:/a b/x.phv" } else { "/a b/x.phv" });
        assert_eq!(uri_to_path(&path_to_uri(&p)).unwrap(), p);
        let dotted = PathBuf::from(if cfg!(windows) { "C:/a b/./c/../x.phv" } else { "/a b/./c/../x.phv" });
        assert_eq!(normalize(&dotted), p);
    }

    #[test]
    fn refuses_remote_and_device_uris() {
        for uri in [
            "file://host/share/x.phv",
            "file:///%5C%5Chost%5Cshare%5Cx.phv",
            "file:///\\\\host\\share\\x.phv",
            "file:////host/share/x.phv",
            "file:///%5C%5C.%5Cpipe%5Cx",
            "file:///%5C%5C%3F%5CC:%5Cx.phv",
            "file:///C:/dir/CON",
            "file:///C:/dir/../x.phv",
            "http://example.com/x.phv",
        ] {
            assert!(uri_to_path(uri).is_none(), "{uri}");
        }
    }

    /// The text each edit of a rename would replace, by file name, sorted.
    fn edits(result: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (uri, list) in result["changes"].as_object().unwrap() {
            let path = uri_to_path(uri).unwrap();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            for e in list.as_array().unwrap() {
                let r = &e["range"];
                let s =
                    offset_of(&text, r["start"]["line"].as_u64().unwrap(), r["start"]["character"].as_u64().unwrap());
                let t = offset_of(&text, r["end"]["line"].as_u64().unwrap(), r["end"]["character"].as_u64().unwrap());
                out.push((
                    name.clone(),
                    format!("{}->{}", &text[s as usize..t as usize], e["newText"].as_str().unwrap()),
                ));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn renames_across_files_and_overrides() {
        let p = Project::new("rename");
        let page = "<?phv\nimport lib.shapes.{Shape, make};\n$s = ma|ke(\"a\");\necho $s->area(), make(\"b\")->name;\n";
        std::fs::write(p.dir.join("page.phv"), page.replace('|', "")).unwrap();
        let (server, path, line, character) = p.open("page.phv", page);
        let refs = server.references(&path, line, character, true);
        assert_eq!(refs.as_array().unwrap().len(), 4, "{refs}"); // the definition, the import and two calls
        let got = edits(&server.rename(&path, line, character, "build").unwrap());
        assert_eq!(
            got,
            [
                ("page.phv".to_string(), "make->build".to_string()),
                ("page.phv".into(), "make->build".into()),
                ("page.phv".into(), "make->build".into()),
                ("shapes.phv".into(), "make->build".into()),
            ]
        );
        // A method is renamed with the methods that override it.
        std::fs::write(
            p.dir.join("lib/more.phv"),
            "import lib.shapes.Shape;\nexport class Square extends Shape {\n    public function area(): float { return 1.0; }\n}\n",
        )
        .unwrap();
        let page = "<?phv\nimport lib.more.Square;\n$q = new Square(\"q\");\necho $q->ar|ea();\n";
        std::fs::write(p.dir.join("page.phv"), page.replace('|', "")).unwrap();
        let (server, path, line, character) = p.open("page.phv", page);
        let got = edits(&server.rename(&path, line, character, "size").unwrap());
        let files: Vec<&str> = got.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(files, ["more.phv", "page.phv", "shapes.phv"], "{got:?}");
        // Variables, and the standard library, which cannot be renamed.
        let (server, path, line, character) = p.open("page.phv", "<?phv\n$cou|nt = 1;\necho $count + 1;\n");
        assert_eq!(edits(&server.rename(&path, line, character, "$n").unwrap()).len(), 2);
        let (server, path, line, character) = p.open("page.phv", "<?phv\necho strl|en(\"x\");\n");
        assert!(server.prepare_rename(&path, line, character).is_err());
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    #[test]
    fn formats_helps_with_signatures_and_lists_symbols() {
        let p = Project::new("tools");
        let (server, path, _, _) = p.open("page.phv", "<?phv\nfunction f(int $a): int {\nreturn $a;   \n}|\n");
        let edits = server.formatting(&path);
        assert_eq!(edits[0]["newText"], "<?phv\nfunction f(int $a): int {\n    return $a;\n}\n", "{edits}");
        let (server, path, line, character) = p.open(
            "page.phv",
            "<?phv\nimport lib.shapes.make;\nfunction g(int $a, string $b): int { return $a; }\ng(1, |);\n",
        );
        let help = server.signature_help(&path, line, character);
        assert_eq!(help["signatures"][0]["label"], "g(int $a, string $b): int", "{help}");
        assert_eq!(help["activeParameter"], 1);
        let (server, path, line, character) = p.open("page.phv", "<?phv\necho str_repeat(\"a\", |);\n");
        assert_eq!(server.signature_help(&path, line, character)["activeParameter"], 1);
        let (server, path, _, _) = p.open("page.phv", &format!("{}|", SHAPES.replace("export ", "")));
        let symbols = server.document_symbols(&path);
        let names: Vec<&str> = symbols.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Shape", "Color", "make"]);
        let shape: Vec<&str> =
            symbols[0]["children"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(shape, ["$secret", "$name", "__construct", "area", "unit"]);
        let found = server.workspace_symbols("mak");
        assert!(found.as_array().unwrap().iter().any(|s| s["name"] == "make"), "{found}");
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    #[test]
    fn offers_quick_fixes() {
        let p = Project::new("fixes");
        let all = json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 99, "character": 0 } });
        let titles = |text: &str| {
            let (server, path, _, _) = p.open("page.phv", text);
            let actions = server.code_actions(&path, &all);
            actions.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(titles("<?phv\n$total = 1;\necho $totl;|\n"), ["Change to `$total`", "Rename to `$_total`"]);
        assert!(titles("<?phv\necho strlenn(\"x\");|\n").contains(&"Change to `strlen`".to_string()));
        assert!(titles("<?phv\necho mt_rand(1, 6);|\n").contains(&"Use `random_int`".to_string()));
        assert_eq!(titles("<?phv\nfunction f(): void {\n    $unused = 1;\n}|\n"), ["Rename to `$_unused`"]);
        assert_eq!(
            titles("<?phv\nimport lib.shapes.{make, Color};\necho make(\"x\")->name;|\n"),
            ["Remove `Color` from the import"]
        );
        let (server, path, _, _) = p.open("page.phv", "<?phv\nimport lib.shapes.make;\necho make(\"x\")->aera();|\n");
        let a = server.code_actions(&path, &all);
        assert_eq!(a[0]["title"], "Change to `area`", "{a}");
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    #[test]
    fn colours_names_and_shows_inferred_types() {
        let p = Project::new("tokens");
        let text = "<?phv\nimport lib.shapes.make;\n$s = make(\"a\");\nint $n = 2;\necho $s->area() + $n;|\n";
        let (server, path, _, _) = p.open("page.phv", text);
        let data = server.semantic_tokens(&path)["data"].as_array().unwrap().clone();
        assert_eq!(data.len() % 5, 0);
        // `make` (import and call), `$s` twice, `$n` twice, `area`: types 4, 7, 5.
        let kinds: Vec<u64> = data.chunks(5).map(|t| t[3].as_u64().unwrap()).collect();
        assert!(kinds.contains(&4) && kinds.contains(&5) && kinds.contains(&7), "{kinds:?}");
        let all = json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 99, "character": 0 } });
        let hints = server.inlay_hints(&path, &all);
        let labels: Vec<&str> = hints.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["Shape "], "{hints}"); // `$n` already has its type
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Files over 8 MB are not read for workspace symbols (the v0.9.0 audit read
    /// a 2 GB file of NUL bytes into memory).
    #[test]
    fn workspace_symbols_skip_huge_files() {
        let p = Project::new("huge");
        let mut big = String::from("export function hidden(): int { return 1; }\n// ");
        big.push_str(&"x".repeat(9 << 20));
        std::fs::write(p.dir.join("lib/big.phv"), big).unwrap();
        let (server, _, _, _) = p.open("page.phv", "<?phv\n|");
        let found = server.workspace_symbols("");
        let names: Vec<&str> = found.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"make") && !names.contains(&"hidden"), "{names:?}");
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// An open document over 8 MB is kept without its text, and nothing edits
    /// from that empty text (the 1.0.0 audit: formatting added a blank line).
    #[test]
    fn documents_over_the_limit_are_not_edited() {
        let p = Project::new("oversized");
        let (mut server, page, line, character) = p.open("page.phv", "<?phv\n$cou|nt = 1;\necho $count;\n");
        let big = p.dir.join("big.phv");
        let text = server.admit(&big, &format!("<?phv\n{}", "1;\n".repeat(3 << 20)));
        assert!(text.is_empty());
        server.open.insert(big.clone(), text);
        let all = json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 99, "character": 0 } });
        assert_eq!(server.formatting(&big), Value::Null);
        assert_eq!(server.code_actions(&big, &all), json!([]));
        assert_eq!(server.document_symbols(&big), Value::Null);
        // A rename anywhere would miss its uses there.
        assert!(server.prepare_rename(&page, line, character).is_err());
        assert!(server.rename(&page, line, character, "$n").is_err());
        server.too_large.remove(&big);
        assert!(server.rename(&page, line, character, "$n").is_ok());
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Found by the 1.0.0 audit: a document within the size limit but of
    /// millions of list items took about 1.5 GB to check. Past the limit of
    /// tokens, it is refused before it is parsed.
    #[test]
    fn documents_with_too_many_tokens_are_not_parsed() {
        use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak};
        let p = Project::new("token-limit");
        let text = format!("<?phv\n$x = [{}0];\n", "0,".repeat(3 << 20));
        let (mut server, page, _, _) = p.open("page.phv", "|");
        let text = server.admit(&page, &text);
        assert!(!text.is_empty(), "the document is within the size limit");
        server.open.insert(page.clone(), text);
        let base = thread_allocated();
        reset_thread_peak();
        let a = server.analysis(&page);
        let peak = (thread_peak() - base).max(0) as usize;
        let messages: Vec<&str> = a.diags.iter().map(|d| d.message.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("more than 1000000 tokens")), "{messages:?} {:?}", a.messages);
        // Its 6 million tokens alone would take about 300 MB, and their tree far more.
        assert!(peak < 128 << 20, "held {} MB", peak >> 20);
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Found by the 15th assessment: a page that imports many modules of just
    /// under the limit of tokens took 4.73 GB to analyse, and 1.95 GB stayed
    /// kept between edits. Past the budget of a program it is not checked, and
    /// the analysis of a large program is not kept.
    #[test]
    fn programs_past_the_budget_are_not_checked_or_kept() {
        use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak};
        let p = Project::new("program-budget");
        let mut page = String::from("<?phv\n");
        for i in 0..8 {
            let body = "$a=$a+1;".repeat(166_600);
            let text = format!("<?phv\nexport function f{i}(): int {{ $a = 0; {body} return $a; }}\n");
            std::fs::write(p.dir.join(format!("lib/m{i}.phv")), text).unwrap();
            page.push_str(&format!("import lib.m{i}.{{f{i}}};\necho f{i}();\n"));
        }
        let (mut server, path, _, _) = p.open("page.phv", "|");
        server.open.insert(path.clone(), page);
        let base = thread_allocated();
        reset_thread_peak();
        let a = server.analysis(&path);
        let peak = (thread_peak() - base).max(0) as usize;
        let messages: Vec<&str> = a.diags.iter().map(|d| d.message.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("this program is too large")), "{messages:?}");
        assert!(peak < 600 << 20, "held {} MB", peak >> 20);
        assert!(server.analyses.borrow().is_empty());
        drop(a);
        // A small program's analysis is kept for the next requests.
        server.open.insert(path.clone(), "<?phv\necho 1;\n".into());
        server.changed();
        let _ = server.analysis(&path);
        assert_eq!(server.analyses.borrow().len(), 1);
    }

    /// Found by the 16th assessment: each open document's analysis was kept
    /// (within the budget of one program each), so 20 pages held 1,066 MB.
    /// The kept analyses have one budget in all, and old ones are dropped.
    #[test]
    fn kept_analyses_have_one_budget_and_old_ones_are_dropped() {
        use phv_vm::alloc::thread_allocated;
        let p = Project::new("kept-budget");
        let body = "$a=$a+1;".repeat(10_000);
        let module = format!("<?phv\nexport function big(): int {{ $a = 0; {body} return $a; }}\n");
        std::fs::write(p.dir.join("lib/big.phv"), module).unwrap();
        let (mut server, _, _, _) = p.open("page.phv", "|");
        let pages: Vec<PathBuf> = (0..12)
            .map(|i| {
                let path = p.dir.join(format!("page{i}.phv"));
                let text = format!("<?phv\nimport lib.big.{{big}};\necho big() + {i};\n");
                std::fs::write(&path, &text).unwrap();
                server.open.insert(path.clone(), text);
                path
            })
            .collect();
        let base = thread_allocated();
        let a = server.analysis(&pages[0]);
        assert!(a.diags.is_empty(), "{:?} {:?}", a.diags.iter().map(|d| &d.message).collect::<Vec<_>>(), a.messages);
        assert!(a.tokens > 50_000, "{}", a.tokens);
        drop(a);
        let one = thread_allocated() - base;
        for page in &pages[1..] {
            let _ = server.analysis(page);
        }
        let held = thread_allocated() - base;
        {
            let kept = server.analyses.borrow();
            let tokens: usize = kept.values().map(|k| k.tokens).sum();
            assert!(tokens <= KEEP_TOKENS, "{tokens} tokens kept");
            assert!(kept.len() < pages.len() / 2, "{} kept", kept.len());
            // The most recently used is kept.
            assert!(kept.contains_key(pages.last().unwrap()));
        }
        // Before, all 12 were kept.
        assert!(held < one * 6, "held {} MB, one analysis {} MB", held >> 20, one >> 20);
        // An analysis older than ANALYSIS_AGE is dropped, not only passed over.
        for k in server.analyses.get_mut().values_mut() {
            k.made = k.made.checked_sub(ANALYSIS_AGE).unwrap_or(k.made);
        }
        let _ = server.analysis(&pages[0]);
        assert_eq!(server.analyses.borrow().len(), 1);
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Found by the 15th assessment: an open document was resolved (which
    /// opens a link's target) before anything looked at where it leads.
    /// Making a link needs Developer Mode or the privilege: skipped without.
    #[cfg(windows)]
    #[test]
    fn documents_behind_a_link_to_a_share_are_not_resolved() {
        let p = Project::new("remote-doc");
        let link = p.dir.join("far.phv");
        if std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\far.phv", &link).is_err() {
            return;
        }
        assert_eq!(canonical_doc(&link), None);
        assert!(canonical_doc(&p.dir.join("page.phv")).is_some());
        let (mut server, _, _, _) = p.open("page.phv", "|");
        server.open.insert(link.clone(), "<?phv\necho 1;\n".into());
        let start = std::time::Instant::now();
        let a = server.analysis(&link);
        assert!(a.messages.iter().any(|m| m.contains("on another machine")), "{:?}", a.messages);
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
    }
}
