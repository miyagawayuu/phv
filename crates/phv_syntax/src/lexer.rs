//! Hand-written lexer. A file switches between two modes:
//! - HTML mode: everything is literal output until `<?phv`, `<?` or `<?=`.
//! - Code mode: PHV tokens until `?>`.
//!
//! `.phv` files start in code mode, `.phv.html` templates start in HTML mode.

use crate::diag::Diagnostic;
use crate::span::Span;
use crate::token::{StrPart, Tok, Token};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Code,
    Html,
}

pub fn lex(src: &str, file: u32, base: u32, mode: Mode) -> (Vec<Token>, Vec<Diagnostic>) {
    let (toks, _, diags) = lex_with_comments(src, file, base, mode);
    (toks, diags)
}

/// Like `lex`, also returning the spans of comments (for the formatter).
pub fn lex_with_comments(src: &str, file: u32, base: u32, mode: Mode) -> (Vec<Token>, Vec<Span>, Vec<Diagnostic>) {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        file,
        base,
        toks: Vec::new(),
        diags: Vec::new(),
        comments: Vec::new(),
    };
    lx.run(mode);
    (lx.toks, lx.comments, lx.diags)
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    file: u32,
    base: u32,
    toks: Vec<Token>,
    diags: Vec<Diagnostic>,
    comments: Vec<Span>,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

impl<'a> Lexer<'a> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, self.base + start as u32, self.base + end as u32)
    }

    fn push(&mut self, tok: Tok, start: usize) {
        let span = self.span(start, self.pos);
        self.toks.push(Token { tok, span });
    }

    fn error(&mut self, msg: impl Into<String>, start: usize, end: usize) {
        let span = self.span(start, end);
        self.diags.push(Diagnostic::error(msg, span));
    }

    fn peek(&self, off: usize) -> u8 {
        *self.bytes.get(self.pos + off).unwrap_or(&0)
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes[self.pos.min(self.bytes.len())..].starts_with(s.as_bytes())
    }

    fn run(&mut self, mut mode: Mode) {
        if self.starts_with("\u{feff}") {
            self.pos += 3;
        }
        // `.phv` files are code from the start, but an opening `<?phv` (as in
        // PHP) is accepted.
        if mode == Mode::Code && self.base == 0 && self.starts_with("<?phv") {
            self.pos += 5;
        }
        while self.pos < self.bytes.len() {
            mode = match mode {
                Mode::Html => self.html(),
                Mode::Code => self.code(),
            };
        }
        let end = self.pos;
        self.push(Tok::Eof, end);
    }

    /// Scans inline HTML up to the next open tag. Returns the next mode.
    fn html(&mut self) -> Mode {
        let start = self.pos;
        let mut i = self.pos;
        while let Some(rel) = self.src[i..].find("<?") {
            let at = i + rel;
            let rest = &self.src[at + 2..];
            let (len, echo) = if rest.starts_with('=') {
                (3, true)
            } else if rest.starts_with("phv") && !rest.as_bytes().get(3).copied().is_some_and(is_ident_char) {
                (5, false)
            } else if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
                (2, false)
            } else {
                // Something like `<?xml`: plain text.
                i = at + 2;
                continue;
            };
            if at > start {
                self.pos = at;
                let text = self.src[start..at].to_string();
                self.push(Tok::InlineHtml(text), start);
            }
            self.pos = at + len;
            if echo {
                self.push(Tok::EchoOpen, at);
            }
            return Mode::Code;
        }
        self.pos = self.bytes.len();
        if self.pos > start {
            let text = self.src[start..].to_string();
            self.push(Tok::InlineHtml(text), start);
        }
        Mode::Html
    }

    /// Scans code tokens until `?>` or end of input. Returns the next mode.
    fn code(&mut self) -> Mode {
        loop {
            self.skip_trivia();
            if self.pos >= self.bytes.len() {
                return Mode::Code;
            }
            if self.starts_with("?>") {
                let start = self.pos;
                self.pos += 2;
                self.push(Tok::CloseTag, start);
                // Like PHP, a single newline right after `?>` is swallowed.
                if self.starts_with("\r\n") {
                    self.pos += 2;
                } else if self.peek(0) == b'\n' {
                    self.pos += 1;
                }
                return Mode::Html;
            }
            self.token();
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            let b = self.peek(0);
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else if self.starts_with("//") || (b == b'#' && self.peek(1) != b'[') {
                // Line comment; like PHP it ends at `?>` too. `#[` starts an annotation.
                let start = self.pos;
                while self.pos < self.bytes.len() && self.peek(0) != b'\n' && !self.starts_with("?>") {
                    self.pos += 1;
                }
                let span = self.span(start, self.pos);
                self.comments.push(span);
            } else if self.starts_with("/*") {
                let start = self.pos;
                match self.src[self.pos + 2..].find("*/") {
                    Some(rel) => self.pos += 2 + rel + 2,
                    None => {
                        self.pos = self.bytes.len();
                        self.error("unterminated block comment", start, start + 2);
                    }
                }
                let span = self.span(start, self.pos);
                self.comments.push(span);
            } else {
                return;
            }
        }
    }

    fn ident(&mut self) -> &'a str {
        let start = self.pos;
        while self.pos < self.bytes.len() && is_ident_char(self.peek(0)) {
            self.pos += 1;
        }
        &self.src[start..self.pos]
    }

    fn token(&mut self) {
        let start = self.pos;
        let b = self.peek(0);

        if b == b'$' {
            self.pos += 1;
            if is_ident_start(self.peek(0)) {
                let name = self.ident().to_string();
                self.push(Tok::Var(name), start);
            } else if self.peek(0) == b'$' {
                self.pos += 1;
                self.error("variable variables (`$$name`) are not supported", start, self.pos);
            } else {
                self.error("expected a variable name after `$`", start, self.pos);
            }
            return;
        }
        if is_ident_start(b) {
            let word = self.ident();
            if word == "sql" && self.peek(0) == b'"' {
                self.double_quoted();
                let last = self.toks.pop().unwrap();
                let parts = match last.tok {
                    Tok::Str(s) => vec![StrPart::Lit(s)],
                    Tok::Interp(p) => p,
                    _ => Vec::new(),
                };
                self.push(Tok::Sql(parts), start);
                return;
            }
            let tok = Tok::keyword(word).unwrap_or_else(|| Tok::Ident(word.to_string()));
            self.push(tok, start);
            return;
        }
        if b.is_ascii_digit() {
            self.number();
            return;
        }
        if b == b'"' {
            self.double_quoted();
            return;
        }
        if b == b'\'' {
            self.single_quoted();
            return;
        }

        const PUNCT: &[(&str, Tok)] = &[
            ("<=>", Tok::Spaceship),
            ("===", Tok::EqEqEq),
            ("!==", Tok::NotEqEq),
            ("??=", Tok::QQEq),
            ("?->", Tok::NullsafeArrow),
            ("**=", Tok::StarStarEq),
            ("..=", Tok::DotDotEq),
            ("::", Tok::DoubleColon),
            ("->", Tok::Arrow),
            ("=>", Tok::FatArrow),
            ("??", Tok::QQ),
            (".=", Tok::DotEq),
            ("..", Tok::DotDot),
            ("**", Tok::StarStar),
            ("+=", Tok::PlusEq),
            ("-=", Tok::MinusEq),
            ("*=", Tok::StarEq),
            ("/=", Tok::SlashEq),
            ("%=", Tok::PercentEq),
            ("++", Tok::PlusPlus),
            ("--", Tok::MinusMinus),
            ("==", Tok::EqEq),
            ("!=", Tok::NotEq),
            ("<>", Tok::NotEq),
            ("<=", Tok::Le),
            (">=", Tok::Ge),
            ("&&", Tok::AndAnd),
            ("||", Tok::OrOr),
            ("<<", Tok::Shl),
            (">>", Tok::Shr),
            ("(", Tok::LParen),
            (")", Tok::RParen),
            ("#", Tok::Hash),
            ("[", Tok::LBracket),
            ("]", Tok::RBracket),
            ("{", Tok::LBrace),
            ("}", Tok::RBrace),
            (",", Tok::Comma),
            (";", Tok::Semi),
            (":", Tok::Colon),
            ("?", Tok::Question),
            (".", Tok::Dot),
            ("+", Tok::Plus),
            ("-", Tok::Minus),
            ("*", Tok::Star),
            ("/", Tok::Slash),
            ("%", Tok::Percent),
            ("=", Tok::Eq),
            ("<", Tok::Lt),
            (">", Tok::Gt),
            ("!", Tok::Bang),
            ("&", Tok::Amp),
            ("|", Tok::Pipe),
            ("^", Tok::Caret),
            ("~", Tok::Tilde),
        ];
        for (text, tok) in PUNCT {
            if self.starts_with(text) {
                self.pos += text.len();
                self.push(tok.clone(), start);
                return;
            }
        }

        let ch = self.src[self.pos..].chars().next().unwrap();
        self.pos += ch.len_utf8();
        self.error(format!("unexpected character `{ch}`"), start, self.pos);
    }

    fn number(&mut self) {
        let start = self.pos;
        let radix = match (self.peek(0), self.peek(1)) {
            (b'0', b'x' | b'X') => 16,
            (b'0', b'b' | b'B') => 2,
            (b'0', b'o' | b'O') => 8,
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek(0).is_ascii_alphanumeric() || self.peek(0) == b'_' {
                self.pos += 1;
            }
            let digits: String = self.src[digits_start..self.pos].chars().filter(|c| *c != '_').collect();
            match i64::from_str_radix(&digits, radix) {
                Ok(v) => self.push(Tok::Int(v), start),
                Err(_) => {
                    self.error("invalid integer literal", start, self.pos);
                    self.push(Tok::Int(0), start);
                }
            }
            return;
        }
        let mut is_float = false;
        self.digits();
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(0), b'e' | b'E')
            && (self.peek(1).is_ascii_digit() || (matches!(self.peek(1), b'+' | b'-') && self.peek(2).is_ascii_digit()))
        {
            is_float = true;
            self.pos += 2;
            self.digits();
        }
        let text: String = self.src[start..self.pos].chars().filter(|c| *c != '_').collect();
        if is_float {
            let v = text.parse::<f64>().unwrap_or(0.0);
            self.push(Tok::Float(v), start);
        } else {
            match text.parse::<i64>() {
                Ok(v) => self.push(Tok::Int(v), start),
                Err(_) => {
                    self.error("integer literal is too large for `int` (64-bit)", start, self.pos);
                    self.push(Tok::Int(0), start);
                }
            }
        }
    }

    fn digits(&mut self) {
        while self.peek(0).is_ascii_digit() || (self.peek(0) == b'_' && self.peek(1).is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn single_quoted(&mut self) {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error("unterminated string", start, start + 1);
                break;
            }
            let b = self.peek(0);
            if b == b'\'' {
                self.pos += 1;
                break;
            }
            if b == b'\\' && matches!(self.peek(1), b'\'' | b'\\') {
                out.push(self.peek(1) as char);
                self.pos += 2;
                continue;
            }
            let ch = self.src[self.pos..].chars().next().unwrap();
            out.push(ch);
            self.pos += ch.len_utf8();
        }
        self.push(Tok::Str(out), start);
    }

    fn double_quoted(&mut self) {
        let start = self.pos;
        self.pos += 1;
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error("unterminated string", start, start + 1);
                break;
            }
            let b = self.peek(0);
            match b {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => self.escape(&mut lit),
                b'$' if is_ident_start(self.peek(1)) => {
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    let var_start = self.pos;
                    self.pos += 1;
                    let name = self.ident().to_string();
                    parts.push(StrPart::Var(name, self.span(var_start, self.pos)));
                }
                b'{' if self.peek(1) == b'$' => {
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    let open = self.pos;
                    self.pos += 1;
                    let expr_start = self.pos;
                    let mut depth = 1;
                    while self.pos < self.bytes.len() {
                        match self.peek(0) {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            b'"' => break,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    if self.peek(0) != b'}' {
                        self.error("unterminated `{` in string interpolation", open, open + 1);
                        continue;
                    }
                    let code = self.src[expr_start..self.pos].to_string();
                    self.pos += 1;
                    parts.push(StrPart::Expr(code, self.base + expr_start as u32));
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    lit.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if parts.is_empty() {
            self.push(Tok::Str(lit), start);
        } else {
            if !lit.is_empty() {
                parts.push(StrPart::Lit(lit));
            }
            self.push(Tok::Interp(parts), start);
        }
    }

    fn escape(&mut self, out: &mut String) {
        let start = self.pos;
        let c = self.peek(1);
        self.pos += 2;
        match c {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'0' => out.push('\0'),
            b'e' => out.push('\x1b'),
            b'\\' => out.push('\\'),
            b'"' => out.push('"'),
            b'$' => out.push('$'),
            b'{' => out.push('{'),
            b'u' if self.peek(0) == b'{' => {
                let hex_start = self.pos + 1;
                match self.src[hex_start..].find('}') {
                    Some(rel) => {
                        let hex = &self.src[hex_start..hex_start + rel];
                        self.pos = hex_start + rel + 1;
                        match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                            Some(ch) => out.push(ch),
                            None => self.error("invalid unicode escape", start, self.pos),
                        }
                    }
                    None => self.error("unterminated unicode escape", start, self.pos),
                }
            }
            0 => self.pos -= 1,
            _ => {
                // Unknown escapes are kept as written, like PHP.
                self.pos -= 1;
                out.push('\\');
            }
        }
    }
}
