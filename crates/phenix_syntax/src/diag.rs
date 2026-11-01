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
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { severity: Severity::Error, message: message.into(), span, label: None, notes: Vec::new() }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { severity: Severity::Warning, ..Diagnostic::error(message, span) }
    }

    pub fn label(mut self, label: impl Into<String>) -> Diagnostic {
        self.label = Some(label.into());
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
///   --> app.phx:3:6
///    |
///  3 | $y = "1" + 1;
///    |      ^^^^^^^ no implicit conversion
///    = note: ...
/// ```
pub fn render(sources: &SourceMap, diags: &[Diagnostic], color: bool) -> String {
    let mut out = String::new();
    for d in diags {
        render_one(&mut out, sources, d, color);
        out.push('\n');
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
    let _ = writeln!(out, "{red}{kind}{reset}{bold}: {}{reset}", d.message);
    let file = sources.get(d.span.file);
    let (line, col) = file.line_col(d.span.start);
    let gutter = line.to_string().len();
    let pad = " ".repeat(gutter);
    let _ = writeln!(out, "{pad}{blue}-->{reset} {}:{}:{}", file.name, line, col);
    let _ = writeln!(out, "{pad} {blue}|{reset}");
    let text = file.line_text(line);
    let _ = writeln!(out, "{blue}{line} |{reset} {text}");
    let (end_line, end_col) = file.line_col(d.span.end.max(d.span.start));
    let width = if end_line == line {
        end_col.saturating_sub(col).max(1)
    } else {
        text.chars().count().saturating_sub(col - 1).max(1)
    };
    let _ = write!(out, "{pad} {blue}|{reset} {}{red}{}{reset}", " ".repeat(col - 1), "^".repeat(width));
    if let Some(label) = &d.label {
        let _ = write!(out, " {red}{label}{reset}");
    }
    out.push('\n');
    for note in &d.notes {
        let _ = writeln!(out, "{pad} {blue}={reset} {bold}note{reset}: {note}");
    }
}
