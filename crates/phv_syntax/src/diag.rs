use crate::span::{SourceMap, Span};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub label: Option<String>,
    pub notes: Vec<String>,
    /// Identifies a kind of diagnostic that may be dropped later (see `CSP_INLINE`).
    pub code: Option<&'static str>,
}

/// Warning code for inline JavaScript (event handler attributes, `javascript:`
/// URLs) that the default Content-Security-Policy blocks. The driver drops it
/// when phv.toml's policy allows inline code.
pub const CSP_INLINE: &str = "csp-inline";

/// Warning code for code that is never used (variables, imports, unreachable
/// statements). The driver drops it while the program has errors, which often
/// leave such code behind.
pub const UNUSED: &str = "unused";

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
            label: None,
            notes: Vec::new(),
            code: None,
        }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { severity: Severity::Warning, ..Diagnostic::error(message, span) }
    }

    pub fn label(mut self, label: impl Into<String>) -> Diagnostic {
        self.label = Some(label.into());
        self
    }

    pub fn code(mut self, code: &'static str) -> Diagnostic {
        self.code = Some(code);
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Diagnostic {
        self.notes.push(note.into());
        self
    }

    /// In-place variant of `note`, for diagnostics already pushed to a list.
    pub fn add_note(&mut self, note: impl Into<String>) -> &mut Diagnostic {
        self.notes.push(note.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Renders diagnostics in a rustc-like layout:
///
/// ```text
/// error: cannot apply `+` to `string` and `int`
///   --> app.phv:3:6
///    |
///  3 | $y = "1" + 1;
///    |      ^^^^^^^ no implicit conversion
///    = note: ...
/// ```
/// Messages shown at most; the rest are counted. Generated or hostile source
/// can produce thousands, and each one quotes a line.
const MAX_SHOWN: usize = 50;
/// Characters of a source line shown around the error.
const LINE_WINDOW: usize = 160;

pub fn render(sources: &SourceMap, diags: &[Diagnostic], color: bool) -> String {
    let mut out = String::new();
    for d in diags.iter().take(MAX_SHOWN) {
        render_one(&mut out, sources, d, color);
        out.push('\n');
    }
    if diags.len() > MAX_SHOWN {
        let _ = writeln!(out, "... and {} more\n", diags.len() - MAX_SHOWN);
    }
    out
}

fn render_one(out: &mut String, sources: &SourceMap, d: &Diagnostic, color: bool) {
    let (red, blue, bold, reset) =
        if color { ("\x1b[31;1m", "\x1b[34;1m", "\x1b[1m", "\x1b[0m") } else { ("", "", "", "") };
    let kind = match d.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let _ = writeln!(out, "{red}{kind}{reset}{bold}: {}{reset}", printable(&d.message));
    let file = sources.get(d.span.file);
    let (line, col) = file.line_col(d.span.start);
    let gutter = line.to_string().len();
    let pad = " ".repeat(gutter);
    let _ = writeln!(out, "{pad}{blue}-->{reset} {}:{}:{}", printable(&file.name), line, col);
    let _ = writeln!(out, "{pad} {blue}|{reset}");
    let text = file.line_text(line);
    let (end_line, end_col) = file.line_col(d.span.end.max(d.span.start));
    let len = text.chars().count();
    let mut width =
        if end_line == line { end_col.saturating_sub(col).max(1) } else { len.saturating_sub(col - 1).max(1) };
    // Long lines are shown as a window around the error.
    let from = if len > LINE_WINDOW { (col - 1).saturating_sub(LINE_WINDOW / 3) } else { 0 };
    let shown: String = printable(&text.chars().skip(from).take(LINE_WINDOW).collect::<String>());
    let (before, after) = (if from > 0 { "..." } else { "" }, if from + LINE_WINDOW < len { "..." } else { "" });
    let _ = writeln!(out, "{blue}{line} |{reset} {before}{shown}{after}");
    let caret = col - 1 - from + before.len();
    width = width.min(LINE_WINDOW.saturating_sub(col - 1 - from)).max(1);
    let _ = write!(out, "{pad} {blue}|{reset} {}{red}{}{reset}", " ".repeat(caret), "^".repeat(width));
    if let Some(label) = &d.label {
        let _ = write!(out, " {red}{label}{reset}");
    }
    out.push('\n');
    for note in &d.notes {
        let _ = writeln!(out, "{pad} {blue}={reset} {bold}note{reset}: {}", printable(note));
    }
}

/// Text from a source file or a name, as it may be shown on a terminal: control
/// characters (an escape sequence could clear the screen or set the clipboard)
/// become U+FFFD, one character each, so columns stay where they were (found
/// by the 1.0 audit).
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\t' { '\u{FFFD}' } else { c }).collect()
}

#[cfg(test)]
mod printable_tests {
    /// Found by the 1.0 audit: a source line with escape sequences reached
    /// the terminal as it was.
    #[test]
    fn control_characters_are_not_printed() {
        let s = super::printable("a\u{1b}]52;c;eA==\u{7}b\u{1b}[2J\tc");
        assert!(!s.chars().any(|c| c.is_control() && c != '\t'), "{s:?}");
        assert_eq!(s.chars().count(), "a\u{1b}]52;c;eA==\u{7}b\u{1b}[2J\tc".chars().count());
    }
}
