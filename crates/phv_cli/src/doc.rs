//! `phv doc`: an API reference from the `/** ... */` comments before exported
//! declarations and public members, as one HTML page or one Markdown file.
//! `--std` documents the standard library's modules.

use phv_syntax::ast::{ClassDecl, ClassKind, Module, StmtKind, Visibility};
use phv_syntax::Span;
use std::path::{Path, PathBuf};

/// The standard library's modules written in PHV.
const STD_MODULES: &[&str] = &[
    "std.http",
    "std.db",
    "std.net",
    "std.async",
    "std.validate",
    "std.mail",
    "std.testing",
    "std.regex",
    "std.time",
    "std.ws",
    "std.jobs",
    "std.jwt",
    "std.oauth",
    "std.orm",
    "std.i18n",
    "std.image",
];

/// Files documented at most (the rest of a huge tree is left out).
const MAX_FILES: usize = 2000;

#[derive(Debug, Default)]
pub struct DocItem {
    pub kind: &'static str,
    pub name: String,
    pub signature: String,
    pub doc: String,
    pub members: Vec<DocItem>,
}

#[derive(Debug, Default)]
pub struct DocModule {
    /// `lib.greet`, `std.time`.
    pub name: String,
    /// The `//` comment at the top of the file.
    pub header: String,
    pub items: Vec<DocItem>,
}

/// Where a declaration really starts: before its modifiers (`public static`).
fn decl_start(text: &str, start: usize) -> usize {
    let mut start = start.min(text.len());
    loop {
        let before = text[..start].trim_end();
        let word_len = before.chars().rev().take_while(|c| c.is_ascii_alphabetic()).count();
        let word = &before[before.len() - word_len..];
        if matches!(word, "public" | "protected" | "private" | "static" | "abstract" | "final" | "readonly") {
            start = before.len() - word_len;
        } else {
            return start;
        }
    }
}

/// The `/** ... */` comment right before `start` (skipping `export` and
/// `#[...]` lines), without its markers.
fn doc_before(text: &str, start: usize) -> String {
    let start = decl_start(text, start);
    let mut before = text[..start.min(text.len())].trim_end();
    loop {
        if let Some(b) = before.strip_suffix("export") {
            before = b.trim_end();
            continue;
        }
        // An attribute line: `#[Deprecated("...")]`.
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        if before[line_start..].trim_start().starts_with("#[") && before.ends_with(']') {
            before = before[..line_start].trim_end();
            continue;
        }
        break;
    }
    let Some(body) = before.strip_suffix("*/") else { return String::new() };
    let Some(open) = body.rfind("/**") else { return String::new() };
    let raw = &body[open + 3..];
    let lines: Vec<&str> = raw
        .lines()
        .map(|l| {
            let t = l.trim_start();
            t.strip_prefix("* ").or_else(|| t.strip_prefix('*')).unwrap_or(t).trim_end()
        })
        .collect();
    lines.join("\n").trim().to_string()
}

/// The declaration's head: its text up to the body (`{` or `;` outside
/// brackets and strings), on one line, without `export` and attributes.
fn head(text: &str, span: Span) -> String {
    let s = &text[decl_start(text, span.start as usize)..(span.end as usize).min(text.len())];
    // Comments inside it (a parameter's doc) are left out.
    let mut clean = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        clean.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    clean.push_str(rest);
    let s = clean.as_str();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
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
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' | ';' if depth <= 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let one_line = s[..end].split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = one_line.replace("( ", "(").replace(", )", ")").replace(",)", ")");
    while let Some(rest) = out.strip_prefix("export ") {
        out = rest.to_string();
    }
    while out.starts_with("#[") {
        match out.find("] ") {
            Some(i) => out = out[i + 2..].to_string(),
            None => break,
        }
    }
    out.trim().to_string()
}

/// The `//` lines at the top of a file (after `<?phv`).
fn header(text: &str) -> String {
    let mut lines = Vec::new();
    for l in text.lines() {
        let t = l.trim();
        if t == "<?phv" || (t.is_empty() && lines.is_empty()) {
            continue;
        }
        match t.strip_prefix("//") {
            Some(c) => lines.push(c.strip_prefix(' ').unwrap_or(c).to_string()),
            None => break,
        }
    }
    lines.join("\n").trim().to_string()
}

fn class_item(c: &ClassDecl, text: &str) -> DocItem {
    let kind = match c.kind {
        ClassKind::Interface => "interface",
        ClassKind::Enum => "enum",
        ClassKind::Trait => "trait",
        ClassKind::Abstract => "abstract class",
        ClassKind::Class => "class",
    };
    let mut members = Vec::new();
    for case in &c.cases {
        members.push(DocItem {
            kind: "case",
            name: case.name.clone(),
            signature: head(text, case.span),
            doc: doc_before(text, case.span.start as usize),
            ..Default::default()
        });
    }
    for k in &c.consts {
        members.push(DocItem {
            kind: "const",
            name: k.name.clone(),
            signature: head(text, k.span),
            doc: doc_before(text, k.span.start as usize),
            ..Default::default()
        });
    }
    for p in c.props.iter().filter(|p| p.visibility == Visibility::Public) {
        members.push(DocItem {
            kind: "property",
            name: p.name.clone(),
            signature: head(text, p.span),
            doc: doc_before(text, p.span.start as usize),
            ..Default::default()
        });
    }
    for m in &c.methods {
        if m.name == "__construct" {
            // Promoted public parameters are properties.
            for p in m.params.iter().filter(|p| matches!(p.promote, Some((Visibility::Public, _)))) {
                members.push(DocItem {
                    kind: "property",
                    name: p.name.clone(),
                    signature: head(text, p.span),
                    doc: doc_before(text, p.span.start as usize),
                    ..Default::default()
                });
            }
        }
        // A constructor with private state is for the class itself (or the runtime).
        let internal_ctor = m.name == "__construct"
            && (m.params.is_empty()
                || m.params.iter().any(|p| matches!(p.promote, Some((v, _)) if v != Visibility::Public)));
        if m.visibility != Visibility::Public || internal_ctor {
            continue;
        }
        members.push(DocItem {
            kind: "method",
            name: m.name.clone(),
            signature: head(text, m.span),
            doc: doc_before(text, m.span.start as usize),
            ..Default::default()
        });
    }
    DocItem {
        kind,
        name: c.name.clone(),
        signature: head(text, c.span),
        doc: doc_before(text, c.span.start as usize),
        members,
    }
}

/// The documented declarations of a module: what it exports.
pub fn document(name: &str, text: &str) -> DocModule {
    let (module, _) = phv_syntax::parser::parse_module(text, 0, false);
    DocModule { name: name.to_string(), header: header(text), items: items(&module, text) }
}

fn items(module: &Module, text: &str) -> Vec<DocItem> {
    let mut out = Vec::new();
    for s in &module.stmts {
        match &s.kind {
            StmtKind::Function(f) if f.exported => out.push(DocItem {
                kind: "function",
                name: f.name.clone(),
                signature: head(text, f.span),
                doc: doc_before(text, f.span.start as usize),
                members: Vec::new(),
            }),
            StmtKind::Class(c) if c.exported => out.push(class_item(c, text)),
            StmtKind::Const(k) if k.exported => out.push(DocItem {
                kind: "const",
                name: k.name.clone(),
                signature: head(text, k.span),
                doc: doc_before(text, k.span.start as usize),
                members: Vec::new(),
            }),
            StmtKind::TypeAlias(t) if t.exported => out.push(DocItem {
                kind: "type",
                name: t.name.clone(),
                signature: head(text, t.span),
                doc: doc_before(text, t.span.start as usize),
                members: Vec::new(),
            }),
            _ => {}
        }
    }
    out
}

/// The modules under `paths` (by their import name), and with `std` the
/// standard library's.
pub fn collect_modules(paths: &[PathBuf], std: bool) -> Vec<DocModule> {
    let mut out = Vec::new();
    if std {
        for name in STD_MODULES {
            if let Some(text) = phv_std::std_module(name) {
                out.push(document(name, text));
            }
        }
    }
    let want = |p: &Path| {
        let s = p.to_string_lossy();
        s.ends_with(".phv") && !s.ends_with(".phv.html") && !s.ends_with("_test.phv")
    };
    // The project root of each directory, looked up once.
    let mut roots: std::collections::HashMap<PathBuf, PathBuf> = std::collections::HashMap::new();
    for file in crate::tools::collect(paths, &want).into_iter().take(MAX_FILES) {
        // A regular file of at most 8 MB, as the compiler reads them.
        let Ok(text) = phv_driver::read_source(&file) else { continue };
        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let root = roots.entry(dir).or_insert_with(|| phv_driver::find_root(&file)).clone();
        let rel = file.strip_prefix(&root).unwrap_or(&file).with_extension("");
        let name: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let m = document(&name.join("."), &text);
        if !m.items.is_empty() {
            out.push(m);
        }
    }
    out
}

// ----- Markdown -------------------------------------------------------------

/// A fence for `code`: longer than any run of backticks in it, so that
/// nothing in the code ends the block.
fn fence(code: &str) -> String {
    "`".repeat(longest_backticks(code).max(2) + 1)
}

fn longest_backticks(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// `code` as a code span, delimited by more backticks than it has in a row.
fn code_span(code: &str) -> String {
    let code = code.replace('\n', " ");
    match longest_backticks(&code) {
        0 => format!("`{code}`"),
        n => format!("{f} {code} {f}", f = "`".repeat(n + 1)),
    }
}

/// A module header's lines, with indented ones (examples) as code blocks.
fn md_text(text: &str, out: &mut String) {
    let lines: Vec<&str> = text.lines().collect();
    let is_code = |l: &str| l.starts_with("  ");
    let mut i = 0;
    while i < lines.len() {
        let mut j = i;
        while j < lines.len() && is_code(lines[j]) == is_code(lines[i]) {
            j += 1;
        }
        if is_code(lines[i]) {
            let code: Vec<&str> = lines[i..j].iter().map(|l| l.trim_start_matches(' ')).collect();
            let code = code.join("\n");
            let fence = fence(&code);
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&format!("{fence}phv\n{code}\n{fence}\n"));
            if j < lines.len() {
                out.push('\n');
            }
        } else {
            out.push_str(&md_escape(&lines[i..j].join("\n")));
            out.push('\n');
        }
        i = j;
    }
}

/// How far a code span is looked for past its opening backticks.
const MAX_CODE_SPAN: usize = 4096;

/// Prose for Markdown, so that a comment adds no HTML and no link where it is
/// rendered, whichever renderer reads it, and keeps its `code` spans (and
/// the `<name>` in them).
///
/// Outside code spans nothing is left that could make a tag, a link, an
/// image, a link definition, a table or a code span: `<` and `&` are written
/// as entities, `[`, `]`, `|`, backticks and a backslash before punctuation
/// get a backslash, and what would make a bare URL or address a link
/// (`://`, `www.`, `@`) is written with an entity. So the only backticks left
/// are those of the code spans kept.
///
/// A run of backticks opens a code span if a run of the same length closes
/// it (as in Markdown), on its line or over lines that each start with a
/// letter. A span is kept as it is unless its text could do harm if a
/// renderer did not take it for code (`<`, brackets, a URL): such a span is
/// kept only on one line, with no `|` in it (a table would cut it) and with no
/// span over several lines before it; otherwise it is written as prose,
/// backticks included.
///
/// 21st assessment: every second piece between backticks was taken for code,
/// and then, pairing runs as Markdown does, a renderer still paired them
/// otherwise (a bare URL took the opening backtick with it), so a comment's
/// `[x](javascript:...)` inside what was taken for code became a link.
fn md_escape(text: &str) -> String {
    let b = text.as_bytes();
    let line_start = |i: usize| text[..i].rfind('\n').map_or(0, |n| n + 1);
    // Whether the line starting at `i` can only be more text of a paragraph.
    let plain = |i: usize| text[i..].chars().next().is_some_and(|c| c.is_alphabetic());
    let run = |i: usize| b[i..].iter().take_while(|c| **c == b'`').count();
    // The end of the code span that the `n` backticks at `i` open, if one closes it.
    let close = |i: usize, n: usize| -> Option<usize> {
        let mut j = i + n;
        let mut may_cross = plain(line_start(i));
        while j < b.len() && j - i <= MAX_CODE_SPAN {
            match b[j] {
                b'`' => {
                    let m = run(j);
                    if m == n {
                        return Some(j + n);
                    }
                    j += m;
                }
                b'\n' => {
                    may_cross = may_cross && plain(j + 1);
                    if !may_cross {
                        return None;
                    }
                    j += 1;
                }
                _ => j += 1,
            }
        }
        None
    };
    // Text that would be a tag, a link or a URL if it were not taken for code.
    let risky = |code: &str| {
        let lower = code.to_ascii_lowercase();
        code.contains(['<', '[', ']', '@']) || lower.contains("://") || lower.contains("www.")
    };
    let mut out = String::with_capacity(text.len() + 16);
    let mut crossed = false;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' {
            i = md_prose_char(text, i, &mut out);
            continue;
        }
        let n = run(i);
        let Some(end) = close(i, n) else {
            out.push_str(&"\\`".repeat(n));
            i += n;
            continue;
        };
        let code = &text[i + n..end - n];
        let one_line = !code.contains('\n');
        if !risky(code) || (one_line && !crossed && !code.contains('|')) {
            out.push_str(&text[i..end]);
            crossed |= !one_line;
        } else {
            let mut j = i;
            while j < end {
                j = md_prose_char(text, j, &mut out);
            }
        }
        i = end;
    }
    out
}

/// Writes the character of `text` at `i` as Markdown prose (see `md_escape`); returns where the next one is.
fn md_prose_char(text: &str, i: usize, out: &mut String) -> usize {
    let b = text.as_bytes();
    match b[i] {
        b'`' => out.push_str("\\`"),
        b'<' => out.push_str("&lt;"),
        b'&' => out.push_str("&amp;"),
        b'[' => out.push_str("\\["),
        b']' => out.push_str("\\]"),
        b'|' => out.push_str("\\|"),
        b'@' => out.push_str("&#64;"),
        // Before a letter, a digit or a space a backslash is one already.
        b'\\' if b.get(i + 1).is_none_or(|c| c.is_ascii_punctuation() || *c == b'\n') => out.push_str("\\\\"),
        b':' if text[i..].starts_with("://") => out.push_str("&#58;"),
        // `~~~` at the start of a line (also in a list or a quote) opens a code
        // block, to the end of the page if nothing closes it.
        b'~' if text[i..].starts_with("~~~") && !text[..i].ends_with('~') => out.push_str("\\~"),
        b'w' | b'W' if text[i..].get(..4).is_some_and(|w| w.eq_ignore_ascii_case("www.")) => {
            out.push_str(&text[i..i + 3]);
            out.push_str("&#46;");
            return i + 4;
        }
        _ => {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            return i + c.len_utf8();
        }
    }
    i + 1
}

fn anchor(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>().to_lowercase()
}

pub fn markdown(modules: &[DocModule], title: &str) -> String {
    let mut out = format!("# {title}\n\n");
    for m in modules {
        out.push_str(&format!("- [{}](#{})\n", md_escape(&m.name), anchor(&m.name)));
    }
    for m in modules {
        out.push_str(&format!("\n## {}\n\n", md_escape(&m.name)));
        if !m.header.is_empty() {
            md_text(&m.header, &mut out);
            out.push('\n');
        }
        for item in &m.items {
            let fence = fence(&item.signature);
            out.push_str(&format!("### {} {}\n\n{fence}phv\n{}\n{fence}\n\n", item.kind, item.name, item.signature));
            if !item.doc.is_empty() {
                out.push_str(&md_escape(&item.doc));
                out.push_str("\n\n");
            }
            for member in &item.members {
                let doc = md_escape(&member.doc.split_whitespace().collect::<Vec<_>>().join(" "));
                let sep = if doc.is_empty() { "" } else { " — " };
                out.push_str(&format!("- {}{sep}{doc}\n", code_span(&member.signature)));
            }
            if !item.members.is_empty() {
                out.push('\n');
            }
        }
    }
    out
}

// ----- HTML -----------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Text with `code` spans, as escaped HTML paragraphs.
fn html_text(text: &str) -> String {
    let mut out = String::new();
    for para in text.split("\n\n") {
        let mut p = String::new();
        for (i, part) in esc(para.trim()).split('`').enumerate() {
            if i % 2 == 1 {
                p.push_str(&format!("<code>{part}</code>"));
            } else {
                p.push_str(part);
            }
        }
        if !p.is_empty() {
            out.push_str(&format!("<p>{}</p>\n", p.replace('\n', " ")));
        }
    }
    out
}

/// A module header: text paragraphs, and indented lines as code.
fn html_header(text: &str) -> String {
    let mut out = String::new();
    let mut code: Vec<&str> = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let flush_code = |code: &mut Vec<&str>, out: &mut String| {
        if !code.is_empty() {
            let lines: Vec<&str> = code.iter().map(|l| l.trim_start_matches(' ')).collect();
            out.push_str(&format!("<pre><code>{}</code></pre>\n", esc(&lines.join("\n"))));
            code.clear();
        }
    };
    for line in text.lines() {
        if line.starts_with("  ") {
            if !prose.is_empty() {
                out.push_str(&html_text(&prose.join("\n")));
                prose.clear();
            }
            code.push(line);
        } else {
            flush_code(&mut code, &mut out);
            prose.push(line);
        }
    }
    flush_code(&mut code, &mut out);
    if !prose.is_empty() {
        out.push_str(&html_text(&prose.join("\n")));
    }
    out
}

pub fn html(modules: &[DocModule], title: &str) -> String {
    let mut nav = String::new();
    let mut body = String::new();
    for m in modules {
        let id = anchor(&m.name);
        nav.push_str(&format!("<li><a href=\"#{id}\">{}</a></li>\n", esc(&m.name)));
        body.push_str(&format!("<section id=\"{id}\">\n<h2>{}</h2>\n", esc(&m.name)));
        body.push_str(&html_header(&m.header));
        for item in &m.items {
            let iid = format!("{id}-{}", anchor(&item.name));
            body.push_str(&format!(
                "<h3 id=\"{iid}\"><span class=\"kind\">{}</span> {}</h3>\n<pre><code>{}</code></pre>\n{}",
                item.kind,
                esc(&item.name),
                esc(&item.signature),
                html_text(&item.doc)
            ));
            if !item.members.is_empty() {
                body.push_str("<dl>\n");
                for member in &item.members {
                    body.push_str(&format!(
                        "<dt><code>{}</code></dt><dd>{}</dd>\n",
                        esc(&member.signature),
                        html_text(&member.doc)
                    ));
                }
                body.push_str("</dl>\n");
            }
        }
        body.push_str("</section>\n");
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{t}</title>\n<style>\n{STYLE}</style>\n</head>\n<body>\n<nav><h1>{t}</h1>\n<ul>\n{nav}</ul></nav>\n<main>\n{body}</main>\n</body>\n</html>\n",
        t = esc(title)
    )
}

const STYLE: &str = "body{margin:0;font:15px/1.55 system-ui,sans-serif;color:#1d1f21;background:#fff;display:flex}
nav{width:15rem;flex:none;padding:1rem;border-right:1px solid #ddd;height:100vh;overflow:auto;position:sticky;top:0;box-sizing:border-box}
nav h1{font-size:1.1rem;margin:0 0 .8rem}nav ul{list-style:none;padding:0;margin:0}nav a{color:#0b57d0;text-decoration:none}
main{max-width:56rem;padding:1rem 2rem;min-width:0}h2{border-bottom:1px solid #ddd;padding-bottom:.3rem;margin-top:2.5rem}
h3{margin:1.8rem 0 .4rem}.kind{color:#777;font-weight:normal;font-size:.9em}
pre{background:#f5f5f7;padding:.6rem .8rem;border-radius:6px;overflow:auto}code{font:13px ui-monospace,monospace}
p code,dd code{background:#f0f0f2;padding:0 .25rem;border-radius:3px}dt{margin-top:.6rem}dd{margin:.2rem 0 0 1.2rem}dd p{margin:.1rem 0}
@media (prefers-color-scheme:dark){body{background:#17181a;color:#e6e6e6}nav{border-color:#333}pre,p code,dd code{background:#24262a}nav a{color:#8ab4f8}h2{border-color:#333}}
@media (max-width:700px){body{display:block}nav{width:auto;height:auto;position:static;border-right:0;border-bottom:1px solid #ddd}main{padding:1rem}}
";

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "<?phv\n// Greetings.\n//\n//   import lib.greet.greet;\n\n/**\n * Says hello.\n * Twice if `loud`.\n */\nexport function greet(\n    string $name,\n    bool $loud = false,\n): string {\n    return \"hi {$name}\";\n}\n\nfunction hidden(): void {}\n\n/** A point. */\n#[Deprecated(\"use Vec\")]\nexport class Point {\n    /** Distance from the origin. */\n    public function length(): float { return 0.0; }\n    private function secret(): void {}\n    public function __construct(\n        /** Across. */\n        public int $x,\n        private int $y,\n    ) {}\n}\n";

    #[test]
    fn documents_exported_declarations() {
        let m = document("lib.greet", SRC);
        assert_eq!(m.header, "Greetings.\n\n  import lib.greet.greet;");
        let names: Vec<&str> = m.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["greet", "Point"]);
        assert_eq!(m.items[0].signature, "function greet(string $name, bool $loud = false): string");
        assert_eq!(m.items[0].doc, "Says hello.\nTwice if `loud`.");
        assert_eq!(m.items[1].doc, "A point.");
        let members: Vec<(&str, &str)> =
            m.items[1].members.iter().map(|i| (i.signature.as_str(), i.doc.as_str())).collect();
        assert_eq!(
            members,
            [("public function length(): float", "Distance from the origin."), ("public int $x", "Across."),]
        );
        let md = markdown(&[m], "API");
        assert!(md.contains("```phv\nimport lib.greet.greet;\n```"), "{md}");
        assert!(md.contains("- `public int $x` — Across."));
        let page = html(&[document("lib.greet", SRC)], "API <x>");
        assert!(page.contains("<title>API &lt;x&gt;</title>"));
        assert!(page.contains("Twice if <code>loud</code>."));
    }

    /// 21st assessment: every second piece between backticks was taken for
    /// code where Markdown pairs runs of the same length (after ``` `` ` ``` a
    /// comment's HTML was written as it was), links were never escaped, and a
    /// renderer may pair backticks in its own way (a bare URL took the opening
    /// one with it). Prose is written so that nothing in it is markup, and a
    /// code span whose text could do harm is kept only where nothing can cut it.
    #[test]
    fn markdown_prose_makes_no_markup() {
        // Unpaired backticks are escaped; what follows is prose.
        assert_eq!(
            md_escape("Adds. `` ` <img src=x onerror=mk1> and [a link](javascript:mk1) <b>"),
            "Adds. \\`\\` \\` &lt;img src=x onerror=mk1> and \\[a link\\](javascript:mk1) &lt;b>"
        );
        // The re-attack's comment: the URL is no link, so it cannot take the backtick with it.
        assert_eq!(
            md_escape("`` <http://mk1>:`and</p>#{[a](javascript:mk1)<](mk1)\\`"),
            "\\`\\` &lt;http&#58;//mk1>:`and</p>#{[a](javascript:mk1)<](mk1)\\`"
        );
        // Code spans stay as they are, with their `<name>` and brackets, also with longer runs.
        for kept in [
            "a `<T>` b",
            "a ``x ` <y>`` b",
            "`list<int>` or `$m[\"k\"]`",
            "\\d \\w and `a\\`",
            "over `two\nlines` here",
        ] {
            assert_eq!(md_escape(kept), kept);
        }
        // One whose text could be markup is not kept over lines, after a span over lines, or with a `|`.
        assert_eq!(md_escape("the `[i18n]\nlocales` setting"), "the \\`\\[i18n\\]\nlocales\\` setting");
        assert_eq!(md_escape("see `a\nb` and `<T>`"), "see `a\nb` and \\`&lt;T>\\`");
        assert_eq!(md_escape("x `a | <img>` y"), "x \\`a \\| &lt;img>\\` y");
        // A span does not go on over a line that could start a block.
        assert_eq!(md_escape("a ` b\n# <img> ` c"), "a \\` b\n# &lt;img> \\` c");
        assert_eq!(md_escape("a ` b\n\n<img> ` c"), "a \\` b\n\n&lt;img> \\` c");
        // Links, images, definitions, autolinks, bare URLs, addresses, tables, entities and escapes.
        assert_eq!(
            md_escape("![x](y) [a][b] [c]: d <http://e> \\<f> &amp; \\"),
            "!\\[x\\](y) \\[a\\]\\[b\\] \\[c\\]: d &lt;http&#58;//e> \\\\&lt;f> &amp;amp; \\\\"
        );
        assert_eq!(md_escape("https://a.b/`c WWW.d.e x@y.z a|b"), "https&#58;//a.b/\\`c WWW&#46;d.e x&#64;y.z a\\|b");
        // A fence of tildes (struck text stays).
        assert_eq!(md_escape("a\n ~~~ <img>\n> ~~~~x ~~b~~"), "a\n \\~~~ &lt;img>\n> \\~~~~x ~~b~~");
        // Code blocks and code spans of signatures get fences that their text cannot end.
        let src = "<?phv\n// Module.\n//\n//   ```\n//   <img src=x onerror=mk1>\n\n/** Doc. */\nexport function f(string $s = \"\n```\n<img src=y onerror=mk1>\"): void {}\n\nexport class C {\n    public function g(string $t = \"`<b>\"): void {}\n}\n";
        let md = markdown(&[document("lib.<i>m", src)], "API");
        assert!(md.contains("````phv\n```\n<img src=x onerror=mk1>\n````\n"), "{md}");
        assert!(md.contains("\n````phv\nfunction f(string $s = \""), "{md}");
        assert!(md.contains("- `` public function g(string $t = \"`<b>\"): void ``\n"), "{md}");
        assert!(md.contains("- [lib.&lt;i>m](#libim)\n") && md.contains("\n## lib.&lt;i>m\n"), "{md}");
    }

    /// Whatever a comment holds, the only backticks left unescaped in its
    /// prose are those of code spans on one line (or over plain lines), and no
    /// `<`, `[`, `]`, `|`, `@`, `://` or `www.` is outside them.
    #[test]
    fn markdown_prose_of_generated_comments_makes_no_markup() {
        let pieces = [
            "`",
            "``",
            "```",
            "<",
            ">",
            "[",
            "]",
            "(",
            ")",
            "|",
            "\\",
            "&",
            "@",
            ":",
            "//",
            "http://mk1",
            "www.mk1.x",
            "javascript:mk1",
            "<img src=x onerror=mk1>",
            "</p>",
            "<!--",
            "mk1",
            " ",
            "\n",
            "\n\n",
            "# ",
            "- ",
            "a",
            "!",
            "&lt;",
            "&#96;",
            "*",
            "_",
            "~~",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let n = 1 + next() % 14;
            let text: String = (0..n).map(|_| pieces[(next() % pieces.len() as u64) as usize]).collect();
            let out = md_escape(&text);
            // Take the kept code spans out (each ends at the next run of its length), then look at the rest.
            let b = out.as_bytes();
            let (mut prose, mut i) = (String::new(), 0);
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    // An escaped character is text.
                    prose.push('x');
                    i += 1 + out[i + 1..].chars().next().unwrap().len_utf8();
                } else if b[i] == b'`' {
                    let n = b[i..].iter().take_while(|c| **c == b'`').count();
                    let mut j = i + n;
                    let end = loop {
                        assert!(j < b.len(), "an unpaired backtick in {out:?} (from {text:?})");
                        let m = b[j..].iter().take_while(|c| **c == b'`').count();
                        if m == n {
                            break j + n;
                        }
                        j += m.max(1);
                    };
                    let code = &out[i + n..end - n];
                    let lower = code.to_ascii_lowercase();
                    let risky = code.contains(['<', '[', ']', '@']) || lower.contains("://") || lower.contains("www.");
                    assert!(!risky || !code.contains(['\n', '|']), "{out:?} (from {text:?})");
                    prose.push('x');
                    i = end;
                } else {
                    let c = out[i..].chars().next().unwrap();
                    prose.push(c);
                    i += c.len_utf8();
                }
            }
            let lower = prose.to_ascii_lowercase();
            let markup = prose.contains(['<', '@', '[', ']', '|']) || lower.contains("://") || lower.contains("www.");
            assert!(!markup, "{out:?} (from {text:?})");
        }
    }

    /// docs/stdlib.md is `phv doc --std --format md --title "PHV standard library"`;
    /// regenerate it when the standard library's comments change.
    #[test]
    fn the_standard_library_reference_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/stdlib.md");
        let committed = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
        let fresh = markdown(&collect_modules(&[], true), "PHV standard library");
        assert!(committed == fresh, "docs/stdlib.md is out of date: run `phv doc --std --format md -o docs/stdlib.md --title \"PHV standard library\"`");
    }

    #[test]
    fn documents_the_standard_library() {
        let modules = collect_modules(&[], true);
        assert_eq!(modules.len(), STD_MODULES.len());
        let time = modules.iter().find(|m| m.name == "std.time").unwrap();
        let dt = time.items.iter().find(|i| i.name == "DateTime").unwrap();
        assert!(dt.members.iter().any(|m| m.name == "addMonths" && m.doc.contains("last day")), "{dt:?}");
    }
}
