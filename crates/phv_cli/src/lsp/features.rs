//! `phv lsp` beyond completion, hover and definitions: references and rename
//! (across the project), formatting, signature help, document and workspace
//! symbols, quick fixes, semantic tokens and inlay hints.

use super::*;
use phv_syntax::ast::{ClassDecl, ClassKind, Module, StmtKind};
use phv_types::index::EditorIndex;

/// Files of the project analyzed for references and rename (each as an entry,
/// so that the files importing a module are seen too), and searched for
/// workspace symbols.
const MAX_PROJECT_FILES: usize = 300;
/// Longest query answer of workspace symbols.
const MAX_SYMBOLS: usize = 500;

/// A span of a file, by path (spans of different analyses are comparable).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Place {
    path: PathBuf,
    start: u32,
    end: u32,
}

use super::MAX_DOCUMENT;

fn place(a: &Analysis, span: phv_syntax::Span) -> Option<Place> {
    let src = a.sources.get(span.file);
    let path = normalize(src.path.as_ref()?);
    Some(Place { path, start: span.start, end: span.end })
}

/// Where in `text` (a span's text) the name `name` is: a whole word, the one
/// after `$` if there is one (`public int $count = 0` names the property
/// `count`, not the type).
fn name_in(text: &str, name: &str) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    let mut first = None;
    let mut from = 0;
    while let Some(i) = text[from..].find(name).map(|i| i + from) {
        let end = i + name.len();
        let before = text[..i].chars().next_back();
        let after = text[end..].chars().next();
        if !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char) {
            if before == Some('$') {
                return Some((i, end));
            }
            first.get_or_insert((i, end));
        }
        from = i + name.len().max(1);
    }
    first
}

fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_alphabetic() || c == '_') && cs.all(is_ident_char)
}

/// Edit distance, for names that were probably meant.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = if a[i - 1] == b[j - 1] { prev } else { 1 + prev.min(row[j]).min(row[j - 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// Longest name suggestions are looked for (the edit distance takes the product of the lengths).
const MAX_SUGGESTED_NAME: usize = 64;

/// Names close to `wrong`, closest first (at most three).
fn close_names<'a>(wrong: &str, names: impl Iterator<Item = &'a str>) -> Vec<String> {
    if wrong.len() > MAX_SUGGESTED_NAME {
        return Vec::new();
    }
    let limit = if wrong.chars().count() <= 3 { 1 } else { 2 };
    let mut found: Vec<(usize, String)> = names
        .filter(|n| *n != wrong && n.len() <= MAX_SUGGESTED_NAME && n.len().abs_diff(wrong.len()) <= 4 * limit)
        .map(|n| (distance(wrong, n), n.to_string()))
        .filter(|(d, _)| *d <= limit)
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.1 == b.1);
    found.into_iter().take(3).map(|(_, n)| n).collect()
}

/// The text between backquotes after `prefix` in a message (`undefined function \`f\``).
fn quoted_after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = message.strip_prefix(prefix)?.strip_prefix('`')?;
    rest.split('`').next()
}

fn source(path: &Path, text: String) -> SourceFile {
    SourceFile::new(String::new(), Some(path.to_path_buf()), text)
}

/// LSP positions of byte offsets of `text`, for offsets that mostly increase:
/// each is found from the previous one, so a whole file takes one pass.
struct Positions<'a> {
    text: &'a str,
    at: usize,
    line: u64,
    col: u64,
}

impl<'a> Positions<'a> {
    fn new(text: &'a str) -> Positions<'a> {
        Positions { text, at: 0, line: 0, col: 0 }
    }

    fn get(&mut self, offset: usize) -> (u64, u64) {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        if offset < self.at {
            *self = Positions::new(self.text);
        }
        for ch in self.text[self.at..offset].chars() {
            if ch == '\n' {
                self.line += 1;
                self.col = 0;
            } else {
                self.col += ch.len_utf16() as u64;
            }
        }
        self.at = offset;
        (self.line, self.col)
    }
}

/// The LSP positions of `offsets` (in any order), found in one pass over `text`.
pub(super) fn positions_of(text: &str, offsets: impl Iterator<Item = u32>) -> HashMap<u32, (u64, u64)> {
    let mut sorted: Vec<u32> = offsets.collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut p = Positions::new(text);
    sorted.into_iter().map(|o| (o, p.get(o as usize))).collect()
}

/// Semantic tokens and inlay hints given at most (an editor shows far fewer).
const MAX_TOKENS: usize = 100_000;
/// How long references and rename may spend analyzing the project's files.
const SEARCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

fn range_in(src: &SourceFile, start: u32, end: u32) -> Value {
    json!({ "start": position(src, start), "end": position(src, end) })
}

/// What the cursor is on, for references and rename.
struct Target {
    /// The definitions involved (a member and the ones it overrides or that override it).
    defs: Vec<Place>,
    /// The name as written (without `$`).
    name: String,
    /// Defined in a file of the project (not the standard library).
    in_project: bool,
    is_variable: bool,
}

impl Server {
    /// The documents the editor has open and the `.phv` files of the project.
    fn project_entries(&self, path: &Path) -> Vec<PathBuf> {
        let root = find_root(path);
        let mut entries = vec![path.to_path_buf()];
        for p in self.open.keys() {
            if !entries.contains(p) && find_root(p) == root {
                entries.push(p.clone());
            }
        }
        let want = |p: &Path| {
            let s = p.to_string_lossy();
            s.ends_with(".phv") && !s.ends_with(".phv.html")
        };
        for p in crate::tools::collect_max(std::slice::from_ref(&root), &want, MAX_PROJECT_FILES) {
            if entries.len() >= MAX_PROJECT_FILES {
                break;
            }
            if !entries.iter().any(|e| normalize(e) == normalize(&p)) {
                entries.push(p);
            }
        }
        entries
    }

    /// The text of an open document, or of the file on disk (a regular file of
    /// at most 8 MB, as the compiler reads them).
    fn text_of(&self, path: &Path) -> Option<String> {
        self.open.get(path).cloned().or_else(|| phv_driver::read_source(path).ok())
    }

    /// The definition the cursor is on: a use of it or the definition itself.
    fn target(&self, a: &Analysis, offset: u32) -> Option<Target> {
        let (ix, file) = (a.index.as_ref()?, a.entry_file?);
        let def = ix.definition(file, offset).or_else(|| def_around(ix, file, offset))?;
        let item = ix.item_at_def(def)?;
        if matches!(item.name.as_str(), "__construct" | "this") {
            return None;
        }
        let is_variable = item.kind == ItemKind::Variable;
        let related = ix.related_defs(def);
        let defs: Vec<Place> = related.iter().filter_map(|d| place(a, *d)).collect();
        let in_project = !defs.is_empty() && defs.len() == related.len();
        Some(Target { defs, name: item.name.clone(), in_project, is_variable })
    }

    /// Every place the target is defined or used, in the files of the project,
    /// and whether every file was searched (not when the time ran out).
    fn places(&self, path: &Path, target: &Target) -> (Vec<(Place, String)>, bool) {
        let mut out: Vec<(Place, String)> = Vec::new();
        let mut seen: HashSet<Place> = HashSet::new();
        let entries = if target.is_variable { vec![path.to_path_buf()] } else { self.project_entries(path) };
        let deadline = std::time::Instant::now() + SEARCH_BUDGET;
        let mut complete = true;
        for entry in entries {
            if std::time::Instant::now() > deadline {
                complete = false;
                break;
            }
            let Some(text) = self.text_of(&entry) else { continue };
            let a = self.analyze_text(&entry, text);
            let Some(ix) = &a.index else { continue };
            let mut add = |span: phv_syntax::Span| {
                if let Some(p) = place(&a, span) {
                    if seen.insert(p.clone()) {
                        let text = a.sources.get(span.file).text.get(p.start as usize..p.end as usize).unwrap_or("");
                        out.push((p, text.to_string()));
                    }
                }
            };
            for (u, d) in &ix.refs {
                if place(&a, *d).is_some_and(|p| target.defs.contains(&p)) {
                    add(*u);
                    add(*d);
                }
            }
            // Definitions nobody uses yet.
            for d in ix.defs.iter().chain(ix.classes.iter().flatten().map(|m| &m.item)).filter_map(|i| i.def) {
                if place(&a, d).is_some_and(|p| target.defs.contains(&p)) {
                    add(d);
                }
            }
        }
        // Spans that do not contain the name (a module path, a template) are not uses of it.
        out.retain(|(_, text)| name_in(text, &target.name).is_some());
        out.sort_by(|a, b| (&a.0.path, a.0.start).cmp(&(&b.0.path, b.0.start)));
        (out, complete)
    }

    pub(super) fn references(&self, path: &Path, line: u64, character: u64, include_decl: bool) -> Value {
        let Some(text) = self.open.get(path) else { return Value::Null };
        let offset = offset_of(text, line, character);
        let a = self.analysis(path);
        let Some(target) = self.target(&a, offset) else { return json!([]) };
        // Each file is read once, however many references it has.
        let mut files: HashMap<PathBuf, Option<SourceFile>> = HashMap::new();
        let (places, _) = self.places(path, &target);
        let out: Vec<Value> = places
            .into_iter()
            .filter(|(p, _)| include_decl || !target.defs.contains(p))
            .filter_map(|(p, text)| {
                let (s, e) = name_in(&text, &target.name)?;
                let file =
                    files.entry(p.path.clone()).or_insert_with(|| self.text_of(&p.path).map(|t| source(&p.path, t)));
                Some(json!({
                    "uri": path_to_uri(&p.path),
                    "range": range_in(file.as_ref()?, p.start + s as u32, p.start + e as u32),
                }))
            })
            .collect();
        json!(out)
    }

    pub(super) fn prepare_rename(&self, path: &Path, line: u64, character: u64) -> Result<Value, String> {
        self.renamable()?;
        let text = self.open.get(path).ok_or("the document is not open")?;
        let offset = offset_of(text, line, character);
        let a = self.analysis(path);
        let target = self.target(&a, offset).ok_or("nothing to rename here")?;
        if !target.in_project {
            return Err(format!("`{}` is defined in the standard library", target.name));
        }
        // The word at the cursor.
        let o = offset as usize;
        let start = o - text[..o].chars().rev().take_while(|c| is_ident_char(*c)).map(char::len_utf8).sum::<usize>();
        let end = o + text[o..].chars().take_while(|c| is_ident_char(*c)).map(char::len_utf8).sum::<usize>();
        if text[start..end] != target.name {
            return Err("nothing to rename here".into());
        }
        Ok(json!({
            "range": { "start": text_position(text, start), "end": text_position(text, end) },
            "placeholder": target.name,
        }))
    }

    /// The edits that rename what the cursor is on to `new_name`.
    pub(super) fn rename(&self, path: &Path, line: u64, character: u64, new_name: &str) -> Result<Value, String> {
        self.renamable()?;
        let text = self.open.get(path).ok_or("the document is not open")?;
        let offset = offset_of(text, line, character);
        let a = self.analysis(path);
        let target = self.target(&a, offset).ok_or("nothing to rename here")?;
        self.rename_target(path, &target, new_name)
    }

    fn rename_target(&self, path: &Path, target: &Target, new_name: &str) -> Result<Value, String> {
        if !target.in_project {
            return Err(format!("`{}` is defined in the standard library and cannot be renamed", target.name));
        }
        let new_name = if target.is_variable { new_name.trim_start_matches('$') } else { new_name };
        if !is_identifier(new_name) {
            return Err(format!("`{new_name}` is not a valid name"));
        }
        let mut changes: HashMap<String, Vec<Value>> = HashMap::new();
        let mut files: HashMap<PathBuf, SourceFile> = HashMap::new();
        let (places, complete) = self.places(path, target);
        if !complete {
            return Err("the project is too large to find every use in time; nothing was renamed".into());
        }
        for (p, span_text) in places {
            let Some((s, e)) = name_in(&span_text, &target.name) else { continue };
            if !files.contains_key(&p.path) {
                let Some(text) = self.text_of(&p.path) else { continue };
                files.insert(p.path.clone(), source(&p.path, text));
            }
            let file = &files[&p.path];
            changes.entry(path_to_uri(&p.path)).or_default().push(json!({
                "range": range_in(file, p.start + s as u32, p.start + e as u32),
                "newText": new_name,
            }));
        }
        Ok(json!({ "changes": changes }))
    }

    /// A rename would miss the uses in an open document kept without its text
    /// (one over 8 MB; found by the 1.0.0 audit).
    fn renamable(&self) -> Result<(), String> {
        match self.too_large.is_empty() {
            true => Ok(()),
            false => Err(format!("a document larger than {} MB is open; close it to rename", MAX_DOCUMENT >> 20)),
        }
    }

    pub(super) fn formatting(&self, path: &Path) -> Value {
        let Some(text) = self.open.get(path).filter(|_| !self.too_large.contains(path)) else { return Value::Null };
        if path.to_string_lossy().ends_with(".phv.html") {
            return Value::Null;
        }
        // `[fmt] width` of the project, as `phv fmt` uses.
        let width =
            phv_driver::load_config(&find_root(path)).map(|c| c.fmt.width).unwrap_or(phv_syntax::format::DEFAULT_WIDTH);
        match phv_syntax::format::format_source_width(text, width) {
            Ok(formatted) if formatted != *text => json!([{
                "range": { "start": text_position(text, 0), "end": text_position(text, text.len()) },
                "newText": formatted,
            }]),
            // Unchanged, or code that does not parse: nothing to do.
            _ => json!([]),
        }
    }

    pub(super) fn signature_help(&self, path: &Path, line: u64, character: u64) -> Value {
        let Some(text) = self.open.get(path) else { return Value::Null };
        let offset = offset_of(text, line, character) as usize;
        let Some((open, active)) = open_call(&text[..offset]) else { return Value::Null };
        // The name before `(`.
        let before = text[..open].trim_end();
        let name_len: usize = before.chars().rev().take_while(|c| is_ident_char(*c)).map(char::len_utf8).sum();
        if name_len == 0 {
            return Value::Null;
        }
        let name_start = before.len() - name_len;
        let name = &before[name_start..];
        let a = self.analysis(path);
        let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { return Value::Null };
        let detail =
            ix.definition(file, name_start as u32).and_then(|d| ix.item_at_def(d)).map(|i| i.detail.clone()).or_else(
                || {
                    // A method of a string, list or map, or a builtin function.
                    let member = before[..name_start].ends_with("->") || before[..name_start].ends_with("::");
                    let items = if member { ix.members_at(file, name_start as u32)? } else { ix.globals.clone() };
                    items.into_iter().find(|i| i.name == name).map(|i| i.detail)
                },
            );
        let Some(detail) = detail.filter(|d| d.starts_with('(')) else { return Value::Null };
        let params = split_params(&detail);
        json!({
            "signatures": [{
                "label": format!("{name}{detail}"),
                "parameters": params.iter().map(|p| json!({ "label": p })).collect::<Vec<_>>(),
            }],
            "activeSignature": 0,
            "activeParameter": active.min(params.len().saturating_sub(1)),
        })
    }

    pub(super) fn document_symbols(&self, path: &Path) -> Value {
        let Some(text) = self.open.get(path).filter(|_| !self.too_large.contains(path)) else { return Value::Null };
        let template = path.to_string_lossy().ends_with(".phv.html");
        let (module, _) = phv_syntax::parser::parse_module(text, 0, template);
        let file = source(path, text.clone());
        json!(module_symbols(&module, &file))
    }

    pub(super) fn workspace_symbols(&self, query: &str) -> Value {
        let query = query.to_lowercase();
        let mut roots: Vec<PathBuf> = Vec::new();
        for p in self.open.keys() {
            let r = find_root(p);
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        let want = |p: &Path| p.to_string_lossy().ends_with(".phv");
        let mut out = Vec::new();
        for file in crate::tools::collect_max(&roots, &want, MAX_PROJECT_FILES) {
            let Some(text) = self.text_of(&file) else { continue };
            let template = file.to_string_lossy().ends_with(".phv.html");
            let (module, _) = phv_syntax::parser::parse_module(&text, 0, template);
            let src = source(&file, text);
            let mut flat = Vec::new();
            flatten_symbols(&module_symbols(&module, &src), None, &mut flat);
            for (name, kind, range, container) in flat {
                if out.len() >= MAX_SYMBOLS {
                    break;
                }
                if query.is_empty() || name.to_lowercase().contains(&query) {
                    out.push(json!({
                        "name": name, "kind": kind, "containerName": container,
                        "location": { "uri": path_to_uri(&file), "range": range },
                    }));
                }
            }
        }
        json!(out)
    }

    /// Quick fixes for the diagnostics in `range`.
    pub(super) fn code_actions(&self, path: &Path, range: &Value) -> Value {
        let Some(text) = self.open.get(path).filter(|_| !self.too_large.contains(path)) else { return json!([]) };
        let from = offset_of(
            text,
            range["start"]["line"].as_u64().unwrap_or(0),
            range["start"]["character"].as_u64().unwrap_or(0),
        );
        let to = offset_of(
            text,
            range["end"]["line"].as_u64().unwrap_or(0),
            range["end"]["character"].as_u64().unwrap_or(0),
        );
        let a = self.analysis(path);
        let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { return json!([]) };
        let src = a.sources.get(file);
        let uri = path_to_uri(path);
        let mut actions = Vec::new();
        let edit = |title: String, start: u32, end: u32, new_text: &str| {
            json!({
                "title": title, "kind": "quickfix",
                "edit": { "changes": { uri.clone(): [{ "range": range_in(src, start, end), "newText": new_text }] } },
            })
        };
        for d in a.diags.iter().filter(|d| d.span.file == file && d.span.start <= to && d.span.end >= from) {
            let (s, e) = (d.span.start, d.span.end);
            let m = d.message.as_str();
            if let Some(name) = quoted_after(m, "variable ").filter(|_| m.ends_with("is never used")) {
                let name = name.trim_start_matches('$');
                if let Some(target) = self.target(&a, s) {
                    if let Ok(edit) = self.rename_target(path, &target, &format!("_{name}")) {
                        actions.push(
                            json!({ "title": format!("Rename to `$_{name}`"), "kind": "quickfix", "edit": edit }),
                        );
                    }
                }
            } else if let Some(name) = m.strip_prefix('`').and_then(|r| r.strip_suffix("` is imported but never used"))
            {
                if let Some((start, end)) = import_removal(&src.text, s as usize, e as usize) {
                    actions.push(edit(format!("Remove `{name}` from the import"), start as u32, end as u32, ""));
                }
            } else if let Some(name) = quoted_after(m, "undefined variable ") {
                let name = name.trim_start_matches('$');
                let vars = ix.variables_at(file, s);
                for n in close_names(name, vars.iter().map(|v| v.name.as_str())) {
                    actions.push(edit(format!("Change to `${n}`"), s + 1, e, &n));
                }
            } else if let Some(name) = quoted_after(m, "undefined function ") {
                let names = ix.globals.iter().filter(|i| i.kind == ItemKind::Function).map(|i| i.name.as_str());
                for n in close_names(name, names) {
                    actions.push(edit(format!("Change to `{n}`"), s, e, &n));
                }
                if let Some(n) = php_replacement(name) {
                    actions.push(edit(format!("Use `{n}`"), s, e, n));
                }
            } else if let Some(name) = quoted_after(m, "unknown class ") {
                let kinds = [ItemKind::Class, ItemKind::Interface, ItemKind::Enum];
                let names = ix.globals.iter().filter(|i| kinds.contains(&i.kind)).map(|i| i.name.as_str());
                for n in close_names(name, names) {
                    actions.push(edit(format!("Change to `{n}`"), s, e, &n));
                }
            } else if let Some(name) =
                m.rsplit_once(" has no method ").and_then(|(_, r)| r.strip_prefix('`')?.strip_suffix('`'))
            {
                let members = ix.members_at(file, s).unwrap_or_default();
                let names = members.iter().filter(|i| i.kind == ItemKind::Method).map(|i| i.name.as_str());
                for n in close_names(name, names) {
                    actions.push(edit(format!("Change to `{n}`"), s, e, &n));
                }
            } else if let Some(name) =
                m.rsplit_once(" has no property ").and_then(|(_, r)| r.strip_prefix("`$")?.strip_suffix('`'))
            {
                let members = ix.members_at(file, s).unwrap_or_default();
                let names = members.iter().filter(|i| i.kind == ItemKind::Property).map(|i| i.name.as_str());
                for n in close_names(name, names) {
                    actions.push(edit(format!("Change to `{n}`"), s, e, &n));
                }
            }
        }
        json!(actions)
    }

    pub(super) fn semantic_tokens(&self, path: &Path) -> Value {
        if !self.open.contains_key(path) {
            return Value::Null;
        }
        let a = self.analysis(path);
        let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { return json!({ "data": [] }) };
        let src = a.sources.get(file);
        // Every definition by its span, first one wins (as `item_at_def` finds it).
        let mut by_def: HashMap<phv_syntax::Span, &Item> = HashMap::new();
        for item in ix
            .defs
            .iter()
            .chain(ix.classes.iter().flatten().map(|m| &m.item))
            .chain(ix.scopes.iter().flat_map(|s| &s.vars))
        {
            if let Some(d) = item.def {
                by_def.entry(d).or_insert(item);
            }
        }
        let mut tokens: Vec<(u32, u32, u32, u32)> = Vec::new(); // start, end, type, modifiers
        let mut add = |span: phv_syntax::Span, item: &Item, declaration: bool| {
            let Some(kind) = token_type(item.kind) else { return };
            let Some(t) = src.text.get(span.start as usize..span.end as usize) else { return };
            let Some((s, e)) = name_in(t, &item.name) else { return };
            tokens.push((span.start + s as u32, span.start + e as u32, kind, declaration as u32));
        };
        for (u, d) in &ix.refs {
            if u.file == file {
                if let Some(item) = by_def.get(d) {
                    add(*u, item, false);
                }
            }
        }
        for (d, item) in &by_def {
            if d.file == file && d.end > d.start {
                add(*d, item, true);
            }
        }
        tokens.sort();
        tokens.dedup_by(|b, a| b.0 < a.1); // overlapping: keep the first
        tokens.truncate(MAX_TOKENS);
        let mut data = Vec::with_capacity(tokens.len() * 5);
        let (mut line, mut col) = (0u64, 0u64);
        let mut positions = Positions::new(&src.text);
        for (start, end, kind, mods) in tokens {
            let (l, c) = positions.get(start as usize);
            let len: usize = src.text[start as usize..end as usize].chars().map(char::len_utf16).sum();
            let dc = if l == line { c - col } else { c };
            data.extend([l - line, dc, len as u64, kind as u64, mods as u64]);
            (line, col) = (l, c);
        }
        json!({ "data": data })
    }

    pub(super) fn inlay_hints(&self, path: &Path, range: &Value) -> Value {
        let Some(text) = self.open.get(path) else { return Value::Null };
        let from = offset_of(
            text,
            range["start"]["line"].as_u64().unwrap_or(0),
            range["start"]["character"].as_u64().unwrap_or(0),
        );
        let to = offset_of(
            text,
            range["end"]["line"].as_u64().unwrap_or(u64::MAX),
            range["end"]["character"].as_u64().unwrap_or(0),
        );
        let a = self.analysis(path);
        let (Some(ix), Some(file)) = (&a.index, a.entry_file) else { return json!([]) };
        let mut hints: Vec<(u32, &str)> = ix
            .scopes
            .iter()
            .flat_map(|s| &s.vars)
            .filter_map(|v| v.def.filter(|d| d.file == file && d.start >= from && d.start <= to).map(|d| (d, v)))
            .filter(|(d, v)| !v.detail.is_empty() && untyped_assignment(text, d.start as usize, d.end as usize))
            .map(|(d, v)| (d.start, v.detail.as_str()))
            .collect();
        hints.sort();
        hints.dedup();
        hints.truncate(MAX_TOKENS);
        let mut positions = Positions::new(text);
        let out: Vec<Value> = hints
            .into_iter()
            .map(|(start, detail)| {
                let (line, character) = positions.get(start as usize);
                json!({
                    "position": { "line": line, "character": character },
                    "label": format!("{detail} "),
                    "kind": 1,
                })
            })
            .collect();
        json!(out)
    }
}

/// A definition whose span contains `offset` (the cursor on a declaration).
fn def_around(ix: &EditorIndex, file: u32, offset: u32) -> Option<phv_syntax::Span> {
    let inside = |d: &phv_syntax::Span| d.file == file && d.start <= offset && offset <= d.end && d.end > d.start;
    ix.defs
        .iter()
        .chain(ix.classes.iter().flatten().map(|m| &m.item))
        .chain(ix.scopes.iter().flat_map(|s| &s.vars))
        .filter_map(|i| i.def)
        .filter(inside)
        .min_by_key(|d| d.end - d.start)
}

fn token_type(k: ItemKind) -> Option<u32> {
    Some(match k {
        ItemKind::Class => 0,
        ItemKind::Interface => 1,
        ItemKind::Trait => 0,
        ItemKind::Enum => 2,
        ItemKind::EnumCase => 3,
        ItemKind::Function => 4,
        ItemKind::Method => 5,
        ItemKind::Property => 6,
        ItemKind::Variable => 7,
        ItemKind::Constant => 8,
        ItemKind::Keyword => return None,
    })
}

/// The legend of `semantic_tokens`.
pub(super) fn token_legend() -> Value {
    json!({
        "tokenTypes": ["class", "interface", "enum", "enumMember", "function", "method", "property", "variable", "variable"],
        "tokenModifiers": ["declaration"],
    })
}

/// `$x = ...` with no type before `$x`: a variable whose type is inferred.
fn untyped_assignment(text: &str, start: usize, end: usize) -> bool {
    let after = text[end.min(text.len())..].trim_start();
    if !after.starts_with('=') || after.starts_with("==") || after.starts_with("=>") {
        return false;
    }
    let before = text[..start.min(text.len())].trim_end();
    !before.ends_with(|c: char| is_ident_char(c) || matches!(c, '>' | ']' | '?' | ')' | '.' | '$'))
}

/// In the text before the cursor, the `(` of the call it is in, and how many
/// arguments come before it (commas outside brackets and strings).
fn open_call(before: &str) -> Option<(usize, usize)> {
    let mut stack: Vec<(usize, char, usize)> = Vec::new(); // (offset, bracket, commas)
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in before.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' | '{' => stack.push((i, c, 0)),
            ')' | ']' | '}' => {
                stack.pop();
            }
            ',' => {
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                }
            }
            ';' => stack.clear(),
            _ => {}
        }
    }
    stack.iter().rev().find(|(_, c, _)| *c == '(').map(|(i, _, n)| (*i, *n))
}

/// The parameters of a signature `(int $a, list<int> $b = []): T`.
fn split_params(sig: &str) -> Vec<String> {
    let mut depth = 0i32;
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in sig.chars() {
        match c {
            '(' | '<' | '[' if depth >= 0 => {
                if !(c == '(' && depth == 0) {
                    cur.push(c);
                }
                depth += 1;
            }
            ')' | '>' | ']' => {
                depth -= 1;
                if depth == 0 && c == ')' {
                    break;
                }
                cur.push(c);
            }
            ',' if depth == 1 => out.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    let last = cur.trim().to_string();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// The text to delete to remove the imported name at `start..end`: the whole
/// `import` when it is the only name, else the name and one comma beside it.
fn import_removal(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if start > end || end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return None;
    }
    let stmt_start = text[..start].rfind("import")?;
    let stmt_end = start + text[start..].find(';')? + 1;
    let stmt = &text[stmt_start..stmt_end];
    // Anything but one `{ ... }` around the names (comments with braces, say) gets no fix.
    let names = match (stmt.find('{'), stmt.find('}'), stmt.matches('{').count(), stmt.matches('}').count()) {
        (Some(a), Some(b), 1, 1) if a < b => stmt[a + 1..b].split(',').filter(|n| !n.trim().is_empty()).count(),
        (None, None, _, _) => 1,
        _ => return None,
    };
    if names <= 1 {
        let line_end = if text[stmt_end..].starts_with("\r\n") {
            stmt_end + 2
        } else if text[stmt_end..].starts_with('\n') {
            stmt_end + 1
        } else {
            stmt_end
        };
        return Some((stmt_start, line_end));
    }
    // The entry runs to the next `,` or `}` (`X as Y` included); its own start
    // is after the `,` or `{` before it.
    let entry_end = end + text[end..stmt_end].find([',', '}'])?;
    let entry_start = text[..start].rfind([',', '{'])? + 1;
    if entry_start <= stmt_start || entry_start > start {
        return None;
    }
    if text[entry_end..].starts_with(',') {
        // Up to the next name.
        let next = entry_end + 1;
        let next = next + (text[next..].len() - text[next..].trim_start().len());
        Some((entry_start + (text[entry_start..start].len() - text[entry_start..start].trim_start().len()), next))
    } else {
        // The last name: from the comma before it.
        Some((entry_start - 1, entry_end))
    }
}

/// PHP functions with a PHV function that takes the same arguments.
fn php_replacement(name: &str) -> Option<&'static str> {
    Some(match name {
        "mt_rand" => "random_int",
        "uniqid" => "random_token",
        _ => return None,
    })
}

/// LSP symbol kinds.
const FUNCTION: u32 = 12;
const CLASS: u32 = 5;
const INTERFACE: u32 = 11;
const ENUM: u32 = 10;
const ENUM_MEMBER: u32 = 22;
const METHOD: u32 = 6;
const PROPERTY: u32 = 7;
const CONSTANT: u32 = 14;
const TYPE_PARAMETER: u32 = 26;

fn symbol(
    name: &str,
    kind: u32,
    file: &SourceFile,
    span: phv_syntax::Span,
    sel: phv_syntax::Span,
    children: Vec<Value>,
) -> Value {
    let sel = if sel.start >= span.start && sel.end <= span.end { sel } else { span };
    json!({
        "name": name, "kind": kind,
        "range": range_in(file, span.start, span.end.max(span.start)),
        "selectionRange": range_in(file, sel.start, sel.end.max(sel.start)),
        "children": children,
    })
}

fn class_symbol(c: &ClassDecl, file: &SourceFile) -> Value {
    let kind = match c.kind {
        ClassKind::Interface => INTERFACE,
        ClassKind::Enum => ENUM,
        _ => CLASS,
    };
    let mut children = Vec::new();
    for case in &c.cases {
        children.push(symbol(&case.name, ENUM_MEMBER, file, case.span, case.span, Vec::new()));
    }
    for k in &c.consts {
        children.push(symbol(&k.name, CONSTANT, file, k.span, k.name_span, Vec::new()));
    }
    for p in &c.props {
        children.push(symbol(&format!("${}", p.name), PROPERTY, file, p.span, p.span, Vec::new()));
    }
    for m in &c.methods {
        if m.name == "__construct" {
            for p in m.params.iter().filter(|p| p.promote.is_some()) {
                children.push(symbol(&format!("${}", p.name), PROPERTY, file, p.span, p.span, Vec::new()));
            }
        }
        children.push(symbol(&m.name, METHOD, file, m.span, m.name_span, Vec::new()));
    }
    symbol(&c.name, kind, file, c.span, c.name_span, children)
}

fn module_symbols(module: &Module, file: &SourceFile) -> Vec<Value> {
    let mut out = Vec::new();
    for s in &module.stmts {
        match &s.kind {
            StmtKind::Function(f) => out.push(symbol(&f.name, FUNCTION, file, f.span, f.name_span, Vec::new())),
            StmtKind::Class(c) => out.push(class_symbol(c, file)),
            StmtKind::Const(k) => out.push(symbol(&k.name, CONSTANT, file, k.span, k.name_span, Vec::new())),
            StmtKind::TypeAlias(t) => out.push(symbol(&t.name, TYPE_PARAMETER, file, t.span, t.name_span, Vec::new())),
            _ => {}
        }
    }
    out
}

/// Symbols with their parents' names, for workspace symbols.
fn flatten_symbols(symbols: &[Value], container: Option<&str>, out: &mut Vec<(String, Value, Value, Option<String>)>) {
    for s in symbols {
        let name = s["name"].as_str().unwrap_or("").to_string();
        out.push((name.clone(), s["kind"].clone(), s["selectionRange"].clone(), container.map(str::to_string)));
        if let Some(children) = s["children"].as_array() {
            flatten_symbols(children, Some(&name), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regressions of the v0.9.0 audit (security-audit.md §18).
    #[test]
    fn positions_in_one_pass_and_bounded_suggestions() {
        let text = "ab\nこんにちは😀x\n$y";
        let mut p = Positions::new(text);
        assert_eq!(p.get(0), (0, 0));
        assert_eq!(p.get(3 + 15), (1, 5));
        assert_eq!(p.get(3 + 15 + 4), (1, 7));
        assert_eq!(p.get(text.len()), (2, 2));
        assert_eq!(p.get(1), (0, 1), "an earlier offset starts again");
        assert_eq!(p.get(4), (1, 0), "inside a character: its start");
        // A huge unknown name is not compared with anything (the distance is quadratic).
        let long = "a".repeat(60_000);
        assert!(close_names(&format!("{long}b"), [long.as_str()].into_iter()).is_empty());
        assert_eq!(close_names("lenght", ["length", &"x".repeat(100_000)].into_iter()), ["length"]);
        // Braces inside a comment do not make the edit panic: no fix is offered.
        let text = "import lib.m.{one /* importZ } { */, two};\n";
        let two = text.find("two").unwrap();
        assert_eq!(import_removal(text, two, two + 3), None);
        assert_eq!(import_removal(text, 30, 10), None);
    }

    #[test]
    fn names_inside_spans() {
        assert_eq!(name_in("public int $count = 0", "count"), Some((12, 17)));
        assert_eq!(name_in("int $int", "int"), Some((5, 8)));
        assert_eq!(name_in("Counter", "Count"), None);
        assert_eq!(name_in("$x", "x"), Some((1, 2)));
    }

    #[test]
    fn calls_and_parameters() {
        assert_eq!(open_call("f(1, g(2, 3), \"a,b\", "), Some((1, 3)));
        assert_eq!(open_call("f(1); g("), Some((7, 0)));
        assert_eq!(open_call("[1, 2"), None);
        assert_eq!(
            split_params("(int $a, map<string, int> $b = [], fn(int): int $f): void"),
            ["int $a", "map<string, int> $b = []", "fn(int): int $f"]
        );
        assert!(split_params("(): int").is_empty());
    }

    #[test]
    fn close_names_and_imports() {
        assert_eq!(close_names("lenght", ["length", "left", "len"].into_iter()), ["length"]);
        assert!(close_names("x", ["y", "xyz"].into_iter()).len() <= 1);
        let text = "import a.b.{One, Two, Three};\n";
        let two = text.find("Two").unwrap();
        let (s, e) = import_removal(text, two, two + 3).unwrap();
        assert_eq!(format!("{}{}", &text[..s], &text[e..]), "import a.b.{One, Three};\n");
        let three = text.find("Three").unwrap();
        let (s, e) = import_removal(text, three, three + 5).unwrap();
        assert_eq!(format!("{}{}", &text[..s], &text[e..]), "import a.b.{One, Two};\n");
        let text = "<?phv\nimport a.b.Only;\necho 1;\n";
        let o = text.find("Only").unwrap();
        let (s, e) = import_removal(text, o, o + 4).unwrap();
        assert_eq!(format!("{}{}", &text[..s], &text[e..]), "<?phv\necho 1;\n");
        assert!(untyped_assignment("$x = 1;", 0, 2));
        assert!(!untyped_assignment("int $x = 1;", 4, 6));
        assert!(!untyped_assignment("$x == 1;", 0, 2));
    }
}
