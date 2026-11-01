//! `phenix lsp`: a Language Server Protocol server over stdin/stdout.
//! It publishes type errors as you type and shows types on hover.

use phenix_driver::{analyze, find_root};
use phenix_syntax::{Severity, SourceFile};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

fn read_message(input: &mut impl BufRead) -> Option<Value> {
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
    serde_json::from_slice(&buf).ok()
}

fn send(out: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    // Only local files (`file:///...`), never `file://host/share`.
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy().into_owned();
    // file:///C:/x -> C:/x on Windows
    let path = if cfg!(windows) { decoded.trim_start_matches('/').to_string() } else { decoded };
    Some(PathBuf::from(path))
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

    fn check(&mut self, path: &Path, out: &mut impl Write) {
        if path.to_string_lossy().ends_with(".phx.html") {
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
                "severity": 1, "source": "phenix", "message": m,
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
                "source": "phenix",
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
        let Some(text) = self.open.get(path).cloned().or_else(|| std::fs::read_to_string(path).ok()) else {
            return Value::Null;
        };
        let root = find_root(path);
        let a = analyze(&root, path, self.overlay());
        let (Some(hir), Some(file)) = (&a.hir, a.entry_file) else { return Value::Null };
        let offset = offset_of(&text, line, character);
        match phenix_types::query::type_at(hir, file, offset) {
            Some((span, ty)) => {
                let src = a.sources.get(file);
                json!({
                    "contents": { "kind": "markdown", "value": format!("```phenix\n{ty}\n```") },
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
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        let doc_path = || params["textDocument"]["uri"].as_str().and_then(uri_to_path);
        let result = match method.as_str() {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": { "openClose": true, "change": 1, "save": { "includeText": false } },
                    "hoverProvider": true,
                },
                "serverInfo": { "name": "phenix", "version": env!("CARGO_PKG_VERSION") },
            })),
            "shutdown" => {
                shutting_down = true;
                Some(Value::Null)
            }
            "exit" => std::process::exit(if shutting_down { 0 } else { 1 }),
            "textDocument/didOpen" => {
                if let Some(p) = doc_path() {
                    let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
                    server.open.insert(p.clone(), text);
                    server.check(&p, &mut out);
                }
                None
            }
            "textDocument/didChange" => {
                if let Some(p) = doc_path() {
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
                if let Some(p) = doc_path() {
                    server.check(&p, &mut out);
                }
                None
            }
            "textDocument/didClose" => {
                if let Some(p) = doc_path() {
                    server.open.remove(&p);
                }
                None
            }
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
        };
        if let (Some(id), Some(result)) = (id, result) {
            send(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_positions() {
        let text = "ab\nこんにちは😀x\n";
        assert_eq!(offset_of(text, 1, 0), 3);
        assert_eq!(offset_of(text, 1, 5), 3 + 15);
        assert_eq!(offset_of(text, 1, 7), 3 + 15 + 4);
        let p = PathBuf::from(if cfg!(windows) { "C:/a b/x.phx" } else { "/a b/x.phx" });
        assert_eq!(uri_to_path(&path_to_uri(&p)).unwrap(), p);
    }
}
