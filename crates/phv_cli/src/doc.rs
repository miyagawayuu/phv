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

/// A module header's lines, with indented ones (examples) as code blocks.
fn md_text(text: &str, out: &mut String) {
    let mut in_code = false;
    for line in text.lines() {
        let code = line.starts_with("  ");
        if code != in_code {
            if code && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(if code { "```phv\n" } else { "```\n\n" });
            in_code = code;
        }
        if code {
            out.push_str(line.trim_start_matches(' '));
        } else {
            out.push_str(&md_escape(line));
        }
        out.push('\n');
    }
    if in_code {
        out.push_str("```\n");
    }
}

/// Prose for Markdown: `<` and `&` outside code spans are written as entities,
/// so a comment cannot add HTML (or lose `<name>` to it) where it is rendered.
fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, part) in text.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            out.push_str(part);
        } else {
            out.push_str(&part.replace('&', "&amp;").replace('<', "&lt;"));
        }
    }
    out
}

fn anchor(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>().to_lowercase()
}

pub fn markdown(modules: &[DocModule], title: &str) -> String {
    let mut out = format!("# {title}\n\n");
    for m in modules {
        out.push_str(&format!("- [{}](#{})\n", m.name, anchor(&m.name)));
    }
    for m in modules {
        out.push_str(&format!("\n## {}\n\n", m.name));
        if !m.header.is_empty() {
            md_text(&m.header, &mut out);
            out.push('\n');
        }
        for item in &m.items {
            out.push_str(&format!("### {} {}\n\n```phv\n{}\n```\n\n", item.kind, item.name, item.signature));
            if !item.doc.is_empty() {
                out.push_str(&md_escape(&item.doc));
                out.push_str("\n\n");
            }
            for member in &item.members {
                let doc = md_escape(&member.doc.split_whitespace().collect::<Vec<_>>().join(" "));
                let sep = if doc.is_empty() { "" } else { " — " };
                out.push_str(&format!("- `{}`{sep}{doc}\n", member.signature));
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
