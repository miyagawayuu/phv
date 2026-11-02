//! `phv lsp`: a Language Server Protocol server over stdin/stdout.
//! It publishes type errors as you type, shows types on hover, completes
//! names and jumps to definitions.

use phv_driver::{analyze, analyze_for_editor, find_root, Analysis};
use phv_syntax::lexer::{lex_with_comments, Mode};
use phv_syntax::token::Tok;
use phv_syntax::{Severity, SourceFile};
use phv_types::index::{Item, ItemKind};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

/// Messages on the stream: `None` means the stream ended or its framing is
/// broken (nothing more can be read); `Some(None)` is a well-framed message
/// that is not valid JSON, answered with a parse error.
fn read_message(input: &mut impl BufRead) -> Option<Option<Value>> {
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
    Some(serde_json::from_slice(&buf).ok())
}

/// Open documents at most; more are ignored (each holds a copy of its text).
const MAX_OPEN: usize = 1000;

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

struct Server {
    open: HashMap<PathBuf, String>,
    /// Files we published diagnostics for, per entry document.
    reported: HashMap<PathBuf, HashSet<String>>,
}

impl Server {
    fn overlay(&self) -> HashMap<PathBuf, String> {
        self.open.iter().filter_map(|(p, t)| p.canonicalize().ok().map(|c| (c, t.clone()))).collect()
    }

    /// Analyzes `path` as if its text were `text`.
    fn analyze_text(&self, path: &Path, text: String) -> Analysis {
        let mut overlay = self.overlay();
        if let Ok(c) = path.canonicalize() {
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
                let a = self.analyze_text(path, text.clone());
                let mut items = a.index.map(|ix| ix.globals).unwrap_or_default();
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
        let a = self.analyze_text(path, text.clone());
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
        if path.to_string_lossy().ends_with(".phv.html") {
            return; // templates are checked through the pages that render them
        }
        let root = find_root(path);
        let a = analyze(&root, path, self.overlay());
        let mut by_file: HashMap<String, Vec<Value>> = HashMap::new();
        let entry_uri = path_to_uri(path);
        by_file.entry(entry_uri.clone()).or_default();
        for m in &a.messages {
            by_file.entry(entry_uri.clone()).or_default().push(json!({
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 1, "source": "phv", "message": m,
            }));
        }
        for d in &a.diags {
            let file = a.sources.get(d.span.file);
            let Some(fpath) = &file.path else { continue };
            let mut message = d.message.clone();
            for n in &d.notes {
                message.push_str("\nnote: ");
                message.push_str(n);
            }
            by_file.entry(path_to_uri(fpath)).or_default().push(json!({
                "range": { "start": position(file, d.span.start), "end": position(file, d.span.end.max(d.span.start)) },
                "severity": if d.severity == Severity::Error { 1 } else { 2 },
                "source": "phv",
                "message": message,
            }));
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
        let root = find_root(path);
        let a = analyze(&root, path, self.overlay());
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
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut server = Server { open: HashMap::new(), reported: HashMap::new() };
    let mut shutting_down = false;
    while let Some(msg) = read_message(&mut input) {
        let Some(msg) = msg else {
            send(
                &mut out,
                &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }),
            );
            continue;
        };
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        let doc_path = || params["textDocument"]["uri"].as_str().and_then(uri_to_path);
        // A bug in the checker must not take the editor's language server down.
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match method.as_str() {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": { "openClose": true, "change": 1, "save": { "includeText": false } },
                    "hoverProvider": true,
                    "completionProvider": { "triggerCharacters": ["$", ">", ":"] },
                    "definitionProvider": true,
                },
                "serverInfo": { "name": "phv", "version": env!("CARGO_PKG_VERSION") },
            })),
            "shutdown" => {
                shutting_down = true;
                Some(Value::Null)
            }
            "exit" => std::process::exit(if shutting_down { 0 } else { 1 }),
            "textDocument/didOpen" => {
                if let Some(p) = doc_path().filter(|p| server.open.contains_key(p) || server.open.len() < MAX_OPEN) {
                    let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
                    server.open.insert(p.clone(), text);
                    server.check(&p, &mut out);
                }
                None
            }
            "textDocument/didChange" => {
                if let Some(p) = doc_path().filter(|p| server.open.contains_key(p)) {
                    if let Some(text) =
                        params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str())
                    {
                        server.open.insert(p.clone(), text.to_string());
                    }
                    server.check(&p, &mut out);
                }
                None
            }
            "textDocument/didSave" => {
                // Only documents the editor opened are checked.
                if let Some(p) = doc_path().filter(|p| server.open.contains_key(p)) {
                    server.check(&p, &mut out);
                }
                None
            }
            "textDocument/didClose" => {
                if let Some(p) = doc_path() {
                    server.open.remove(&p);
                    server.reported.remove(&p);
                }
                None
            }
            "textDocument/completion" => match doc_path() {
                Some(p) => Some(server.completion(
                    &p,
                    params["position"]["line"].as_u64().unwrap_or(0),
                    params["position"]["character"].as_u64().unwrap_or(0),
                    params["context"]["triggerKind"].as_u64() == Some(1),
                )),
                None => Some(Value::Null),
            },
            "textDocument/definition" => match doc_path() {
                Some(p) => Some(server.definition(
                    &p,
                    params["position"]["line"].as_u64().unwrap_or(0),
                    params["position"]["character"].as_u64().unwrap_or(0),
                )),
                None => Some(Value::Null),
            },
            "textDocument/hover" => match doc_path() {
                Some(p) => Some(server.hover(
                    &p,
                    params["position"]["line"].as_u64().unwrap_or(0),
                    params["position"]["character"].as_u64().unwrap_or(0),
                )),
                None => Some(Value::Null),
            },
            _ => {
                if id.is_some() {
                    send(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32601, "message": format!("method not found: {method}") },
                        }),
                    );
                }
                None
            }
        }));
        let Ok(result) = handled else {
            if id.is_some() {
                send(
                    &mut out,
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": "internal error" } }),
                );
            }
            continue;
        };
        if let (Some(id), Some(result)) = (id, result) {
            send(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
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
            let mut server = Server { open: HashMap::new(), reported: HashMap::new() };
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
}
