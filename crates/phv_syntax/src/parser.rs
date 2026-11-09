use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{lex, lex_counted, too_many_tokens, Mode, MAX_TOKENS};
use crate::span::Span;
use crate::token::{StrPart, Tok, Token};

pub fn parse_module(src: &str, file: u32, is_template: bool) -> (Module, Vec<Diagnostic>) {
    let (module, diags, _) = parse_module_within(src, file, is_template, MAX_TOKENS);
    (module, diags)
}

/// Like `parse_module`, reading at most `limit` tokens (at most `MAX_TOKENS`;
/// those of the code in strings included), and also returning how many the
/// file has: `None` when it has more, and then nothing of it is parsed. Past
/// a `limit` below `MAX_TOKENS` no error is reported: the caller, which keeps
/// a budget for a whole program, reports it.
pub fn parse_module_within(
    src: &str,
    file: u32,
    is_template: bool,
    limit: usize,
) -> (Module, Vec<Diagnostic>, Option<usize>) {
    let limit = limit.min(MAX_TOKENS);
    let mode = if is_template { Mode::Html } else { Mode::Code };
    let (toks, mut diags, too_large) = lex_counted(src, file, 0, mode, limit);
    let mut p = Parser::new(toks, file, is_template);
    p.limit = limit;
    // Past the limit of tokens, nothing of the file is parsed: it is refused.
    p.tokens = p.toks.len() - 1;
    p.halted = too_large;
    let stmts = p.block_until(&[]);
    // A page with a layout must end at the top too: its output is joined with the layout's.
    if (is_template || p.uses_layout) && !p.halted && !p.html.at_top_level() {
        let span = p.span();
        let what = if is_template { "template" } else { "page (it uses `layout()`)" };
        let mut d = if p.html.only_select_open() {
            // Found by the 1.2.1 audit: a partial that opened a `<select>` passed.
            let mut d = Diagnostic::error(format!("the {what} ends inside an open `<select>`"), span);
            d.add_note("close the `<select>`: what follows this output is checked outside one, where older browsers would still read a `<title>` or `<style>` as markup inside it");
            d
        } else {
            Diagnostic::error(format!("the {what} ends inside a tag, comment, `<script>`/`<style>` or `<svg>`"), span)
        };
        d.add_note(
            "close everything the template opens: its output is `Html`, which can be joined with other `Html` values",
        );
        p.diags.push(d);
    }
    diags.append(&mut p.diags);
    let over = too_large || p.tokens > limit;
    let tokens = (!over).then_some(p.tokens);
    let module = Module { file, is_template, stmts, template_refs: p.template_refs };
    (module, diags, tokens)
}

/// Parses a standalone type such as `fn(list<T>, fn(T): U): list<U>`.
/// Used for the signatures of built-in functions.
pub fn parse_type_str(src: &str) -> Option<TypeExpr> {
    let (toks, diags) = lex(src, u32::MAX, 0, Mode::Code);
    if !diags.is_empty() {
        return None;
    }
    let mut p = Parser::new(toks, u32::MAX, false);
    let ty = p.ty().ok()?;
    if p.diags.is_empty() && matches!(p.peek(), Tok::Eof) {
        Some(ty)
    } else {
        None
    }
}

/// Normalizes a template path written in `render("...")`, relative to the project root.
pub fn normalize_template_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

struct PErr;
type PResult<T> = Result<T, PErr>;

struct Parser {
    html: crate::htmlctx::HtmlTracker,
    toks: Vec<Token>,
    pos: usize,
    diags: Vec<Diagnostic>,
    file: u32,
    is_template: bool,
    template_refs: Vec<(String, Span)>,
    /// The page calls `layout()`: its output becomes the layout's body.
    uses_layout: bool,
    /// Current nesting of expressions and blocks (see `enter`).
    depth: u32,
    /// Set when the nesting limit was hit: the rest of the file is skipped and
    /// no further errors are reported.
    halted: bool,
    /// Tokens of the file so far, those of interpolated expressions included
    /// (see `lexer::MAX_TOKENS`).
    tokens: usize,
    /// The most tokens the file may have (see `parse_module_within`).
    limit: usize,
    /// Tokens that `expect_gt` split (`>>` into `>` `>`), with their original
    /// form, so a parse that backtracks can put them back.
    splits: Vec<(usize, Token)>,
    /// The loops and `try` blocks around the statement being parsed (in the
    /// current function), with the HTML state where each began: `break`,
    /// `continue`, `return` and `throw` must leave the HTML where the code
    /// they jump to expects it.
    /// With, for a loop, `after_value()` of the HTML at a `break` and at a
    /// `continue` that leave it (bits of each).
    exits: Vec<(Exit, crate::htmlctx::HtmlTracker, [u8; 2])>,
}

#[derive(Clone, Copy, PartialEq)]
enum Exit {
    Loop,
    Try,
}

const BP_IS: u8 = 31;
const BP_UNARY: u8 = 30;

fn binop(tok: &Tok) -> Option<(BinOp, u8, u8)> {
    Some(match tok {
        Tok::QQ => (BinOp::Coalesce, 3, 3),
        Tok::OrOr => (BinOp::Or, 4, 5),
        Tok::AndAnd => (BinOp::And, 6, 7),
        Tok::Pipe => (BinOp::BitOr, 8, 9),
        Tok::Caret => (BinOp::BitXor, 10, 11),
        Tok::Amp => (BinOp::BitAnd, 12, 13),
        Tok::EqEq | Tok::EqEqEq => (BinOp::Eq, 14, 15),
        Tok::NotEq | Tok::NotEqEq => (BinOp::Ne, 14, 15),
        Tok::Spaceship => (BinOp::Cmp, 14, 15),
        Tok::Lt => (BinOp::Lt, 16, 17),
        Tok::Le => (BinOp::Le, 16, 17),
        Tok::Gt => (BinOp::Gt, 16, 17),
        Tok::Ge => (BinOp::Ge, 16, 17),
        Tok::Shl => (BinOp::Shl, 20, 21),
        Tok::Shr => (BinOp::Shr, 20, 21),
        Tok::Dot => (BinOp::Concat, 22, 23),
        Tok::Plus => (BinOp::Add, 24, 25),
        Tok::Minus => (BinOp::Sub, 24, 25),
        Tok::Star => (BinOp::Mul, 26, 27),
        Tok::Slash => (BinOp::Div, 26, 27),
        Tok::Percent => (BinOp::Mod, 26, 27),
        Tok::StarStar => (BinOp::Pow, 33, 33),
        _ => return None,
    })
}

fn assign_op(tok: &Tok) -> Option<AssignOp> {
    Some(match tok {
        Tok::Eq => AssignOp::Set,
        Tok::PlusEq => AssignOp::Add,
        Tok::MinusEq => AssignOp::Sub,
        Tok::StarEq => AssignOp::Mul,
        Tok::SlashEq => AssignOp::Div,
        Tok::PercentEq => AssignOp::Mod,
        Tok::StarStarEq => AssignOp::Pow,
        Tok::DotEq => AssignOp::Concat,
        Tok::QQEq => AssignOp::Coalesce,
        _ => return None,
    })
}

/// Deepest nesting of expressions and blocks the parser accepts.
const MAX_NESTING: u32 = 500;

impl Parser {
    fn new(toks: Vec<Token>, file: u32, is_template: bool) -> Parser {
        Parser {
            html: Default::default(),
            toks,
            pos: 0,
            diags: Vec::new(),
            file,
            is_template,
            template_refs: Vec::new(),
            uses_layout: false,
            depth: 0,
            halted: false,
            tokens: 0,
            limit: MAX_TOKENS,
            splits: Vec::new(),
            exits: Vec::new(),
        }
    }

    /// Enters one level of nesting. The type checker and compiler recurse over
    /// the tree, so very deep source (generated, or hostile) must be refused
    /// here instead of overflowing their stacks.
    fn enter(&mut self, extra: u32) -> PResult<()> {
        if self.halted {
            return Err(PErr);
        }
        if self.depth + extra >= MAX_NESTING {
            let span = self.span();
            self.diags
                .push(Diagnostic::error(format!("code is nested too deeply (more than {MAX_NESTING} levels)"), span));
            self.halted = true;
            self.pos = self.toks.len() - 1;
            return Err(PErr);
        }
        Ok(())
    }

    // ----- token helpers -------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.span()
        } else {
            self.toks[self.pos - 1].span
        }
    }

    /// Moves past the current token, giving its span (the token is not
    /// copied: callers that need its text read it with `peek` first).
    fn bump(&mut self) -> Span {
        let span = self.toks[self.pos.min(self.toks.len() - 1)].span;
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        span
    }

    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error_here(&mut self, msg: impl Into<String>) -> PErr {
        if !self.halted {
            let span = self.span();
            self.diags.push(Diagnostic::error(msg, span));
        }
        PErr
    }

    /// Adds `note` to the diagnostic just reported (none when parsing halted).
    fn note_last(&mut self, note: &str) {
        if !self.halted {
            if let Some(d) = self.diags.last_mut() {
                d.add_note(note);
            }
        }
    }

    fn expect(&mut self, tok: Tok) -> PResult<Span> {
        if self.at(&tok) {
            Ok(self.bump())
        } else {
            let found = self.peek().describe();
            Err(self.error_here(format!("expected {}, found {found}", tok.describe())))
        }
    }

    fn expect_ident(&mut self, what: &str) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let span = self.bump();
                Ok((name, span))
            }
            other => Err(self.error_here(format!("expected {what}, found {}", other.describe()))),
        }
    }

    fn expect_var(&mut self) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Var(name) => {
                let span = self.bump();
                Ok((name, span))
            }
            other => Err(self.error_here(format!("expected a variable, found {}", other.describe()))),
        }
    }

    /// Statement terminator: `;`, or a `?>` close tag (left for the block loop).
    fn semi(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Semi => {
                self.bump();
                Ok(())
            }
            Tok::CloseTag | Tok::Eof => Ok(()),
            other => {
                let found = other.describe();
                let span = self.prev_span();
                let span = Span::new(span.file, span.end, span.end);
                self.diags.push(Diagnostic::error(format!("expected `;`, found {found}"), span));
                Err(PErr)
            }
        }
    }

    /// Skips ahead to a likely statement boundary after a syntax error.
    fn recover(&mut self, start: usize) {
        if self.pos == start {
            self.bump();
        }
        loop {
            match self.peek() {
                Tok::Eof | Tok::RBrace | Tok::CloseTag | Tok::InlineHtml(_) => return,
                Tok::Semi => {
                    self.bump();
                    return;
                }
                Tok::Function
                | Tok::Class
                | Tok::Interface
                | Tok::Enum
                | Tok::Const
                | Tok::If
                | Tok::Foreach
                | Tok::While
                | Tok::For
                | Tok::Return
                | Tok::Echo
                | Tok::Import
                | Tok::Export
                | Tok::Hash => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    // ----- statements ----------------------------------------------------

    fn block_until(&mut self, ends: &[Tok]) -> Vec<Stmt> {
        // A block may run any number of times (or not at all), so the HTML it
        // writes must end where it started; otherwise the context of what
        // follows depends on control flow the tracker cannot see.
        let entry = (!ends.is_empty()).then(|| self.html.clone());
        let mut stmts = self.block_stmts(ends);
        if let Some(entry) = entry {
            self.check_block_html(&entry);
        }
        // Kept as long as the tree: without the room the vector grew into
        // (a `Stmt` is 168 bytes, and a body of one statement had room for four).
        stmts.shrink_to_fit();
        stmts
    }

    fn inline_code_warning(&mut self, span: Span, what: crate::htmlctx::InlineCode) {
        use crate::htmlctx::InlineCode;
        let (msg, note) = match what {
            InlineCode::Handler(name) => (
                format!("the event handler attribute `{name}` does not run under the default Content-Security-Policy"),
                "attach the handler from a `<script>` in the template (it gets the CSP nonce), e.g. `el.addEventListener(\"click\", ...)`",
            ),
            InlineCode::ScriptUrl => (
                "`javascript:` URLs do not run under the default Content-Security-Policy".to_string(),
                "use a button with an event listener added from a `<script>` in the template",
            ),
            InlineCode::ForeignCdata => {
                let mut d = Diagnostic::error(
                    "markup (a tag, comment or CDATA section) in a `<script>` or `<style>` inside `<svg>` or `<math>`",
                    span,
                );
                d.add_note("browsers read the text of these elements as markup there, so it could hide the end tag; move the script out of the SVG, or write `<` with spaces around it (`i < n`)");
                self.diags.push(d);
                return;
            }
            InlineCode::HiddenEndTag => {
                let mut d = Diagnostic::error(
                    "`</noscript` (or, inside `<noscript>`, `<svg>` or `<math>`, the end tag of another raw-text element such as `</textarea>`) in a comment, an attribute value or raw text",
                    span,
                );
                d.add_note("where a browser reads that element's text as raw text (any template or function may be written inside a `<noscript>`), the end tag ends it there and what follows becomes markup; remove it, or write `&lt;/` in an attribute");
                self.diags.push(d);
                return;
            }
            InlineCode::CommentEnd => {
                let mut d = Diagnostic::error(
                    "text right after a value in a comment starts with `>`, `!>`, `->` or `-!>`",
                    span,
                );
                d.add_note("a value ending with `--` (or `-`, `--!`) would end the comment there and turn what follows into markup; put a space after the value");
                self.diags.push(d);
                return;
            }
            InlineCode::ForeignStructure(msg) => {
                let mut d = Diagnostic::error(msg, span);
                d.add_note("what follows would be read as SVG or MathML where this template is checked as HTML, or the other way round; close every element opened inside `<foreignObject>`, `<desc>`, `<title>` or a MathML text element (`<mi>`, `<mtext>`...) before its end tag, and write HTML elements only inside these");
                self.diags.push(d);
                return;
            }
            InlineCode::CdataEnd => {
                let mut d = Diagnostic::error("`>` inside a CDATA section that is not the end of `]]>`", span);
                d.add_note("where a browser reads the section as a bogus comment (inside HTML in SVG), it ends at the first `>` and what follows becomes markup; write `&gt;` or move the text out of the section");
                self.diags.push(d);
                return;
            }
            InlineCode::RawTextInSelect(tag) => {
                let mut d = Diagnostic::error(format!("`<{tag}>` inside `<select>`"), span);
                d.add_note("older browsers ignore it there and read its text as markup, where this template is checked as its text; close the `<select>` first");
                self.diags.push(d);
                return;
            }
            InlineCode::TemplateInSelect => {
                let mut d = Diagnostic::error("`<template>` inside `<select>`", span);
                d.add_note("older browsers ignore a `</select>` inside it, so the `<select>` stays open after it where this template is checked outside one; close the `<select>` first");
                self.diags.push(d);
                return;
            }
            InlineCode::MetaHttpEquiv => {
                let mut d =
                    Diagnostic::error("`http-equiv` after a value inserted into the `content` of this `<meta>`", span);
                d.add_note("`http-equiv=\"refresh\"` would redirect to the value; write `http-equiv` before `content`, where only numbers are accepted");
                self.diags.push(d);
                return;
            }
        };
        let mut d = Diagnostic::warning(msg, span);
        d.add_note(note);
        d.add_note("or allow inline code with `[security] csp` in phv.toml (not recommended)");
        d.code = Some(crate::diag::CSP_INLINE);
        self.diags.push(d);
    }

    fn check_block_html(&mut self, entry: &crate::htmlctx::HtmlTracker) {
        let select = self.html.select_open() != entry.select_open();
        if !self.html.end_block(entry) && !self.halted {
            let span = self.span();
            let mut d = Diagnostic::error("the HTML in this block does not end where it started", span);
            d.add_note(
                "close every tag, attribute and `<script>`/`<style>` opened inside a block, loop or function before it ends",
            );
            if select {
                d.add_note("a `<select>` opened or closed in a block (or a loop's body, or a function) must be closed or opened again in it: what follows would be checked inside or outside one, where older browsers read a `<title>` or `<style>` in it as markup");
            }
            self.diags.push(d);
            self.html = entry.clone();
        }
    }

    fn block_stmts(&mut self, ends: &[Tok]) -> Vec<Stmt> {
        let stmts = Vec::new();
        if self.enter(0).is_err() {
            return stmts;
        }
        self.depth += 1;
        let stmts = self.block_stmts_inner(ends, stmts);
        self.depth -= 1;
        stmts
    }

    fn block_stmts_inner(&mut self, ends: &[Tok], mut stmts: Vec<Stmt>) -> Vec<Stmt> {
        loop {
            let tok = self.peek();
            if *tok == Tok::Eof || ends.contains(tok) {
                break;
            }
            if *tok == Tok::CloseTag {
                self.bump();
                continue;
            }
            if ends.is_empty() && *tok == Tok::RBrace {
                let _ = self.error_here("unexpected `}`");
                self.bump();
                continue;
            }
            // An empty statement is passed over: it makes no statement (found by
            // the 1.0.0 audit: a file of `;` took 400 bytes per byte).
            if *tok == Tok::Semi {
                self.bump();
                continue;
            }
            let start = self.pos;
            match self.stmt() {
                Ok(s) => stmts.push(s),
                Err(PErr) => self.recover(start),
            }
        }
        stmts
    }

    fn block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(Tok::LBrace)?;
        let body = self.block_until(&[Tok::RBrace]);
        self.expect(Tok::RBrace)?;
        Ok(body)
    }

    fn stmt_or_block(&mut self) -> PResult<Vec<Stmt>> {
        if self.at(&Tok::LBrace) {
            self.block()
        } else {
            // `if ($a) while ($b) ...` nests as deeply as braces would.
            self.enter(0)?;
            self.depth += 1;
            let entry = self.html.clone();
            let s = self.stmt();
            self.depth -= 1;
            let s = s?;
            self.check_block_html(&entry);
            Ok(vec![s])
        }
    }

    /// A function or closure body: it can be called from anywhere, so its HTML
    /// is tracked from a fresh document and must end in plain text.
    fn body_block(&mut self) -> PResult<Vec<Stmt>> {
        let outer = std::mem::take(&mut self.html);
        let exits = std::mem::take(&mut self.exits);
        let body = self.block();
        self.html = outer;
        self.exits = exits;
        body
    }

    /// Parses a loop's or `try` block's body with `kind` as a target of exits.
    fn exit_target<T>(&mut self, kind: Exit, body: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.exits.push((kind, self.html.clone(), [0, 0]));
        let r = body(self);
        self.exits.pop();
        r
    }

    /// Parses a loop's body (a target of `break` and `continue`). Its start
    /// may follow its end or a `continue`, and what follows the loop may
    /// follow a `break`: in a comment, they may not end it after a value
    /// there (found by the 1.2 audit).
    fn loop_body<T>(&mut self, body: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let start = self.span();
        let head = self.html.begin_loop();
        self.exits.push((Exit::Loop, self.html.clone(), [0, 0]));
        let r = body(self);
        let [broke, continued] = self.exits.pop().map_or([0, 0], |e| e.2);
        if self.html.end_loop(head, continued, broke) && !self.halted {
            let mut d = Diagnostic::error(
                "this loop's body starts with `>`, `!>`, `->` or `-!>` in a comment, and may follow a value at its end",
                start,
            );
            d.add_note("on the next run, a value ending with `--` (or `-`, `--!`) would end the comment there and turn what follows into markup; start the body with a space");
            self.diags.push(d);
        }
        r
    }

    /// Checks a `break`/`continue` (`loop_level` > 0) or a `return`/`throw`:
    /// the HTML must be where the code it jumps to began (the loop, the `try`
    /// block, or the start of the function), and where every `try` block it
    /// leaves began (their `finally` runs on the way out).
    fn check_exit(&mut self, loop_level: u32, throw: bool, is_break: bool, span: Span) {
        let mut checked = Vec::new();
        let mut loops = 0;
        let mut target = None;
        for (k, (kind, at, _)) in self.exits.iter().enumerate().rev() {
            match kind {
                Exit::Try if throw => {
                    target = Some((k, at.clone()));
                    break;
                }
                Exit::Try => checked.push(at.clone()),
                Exit::Loop if loop_level > 0 => {
                    loops += 1;
                    if loops == loop_level {
                        target = Some((k, at.clone()));
                        break;
                    }
                }
                Exit::Loop => {}
            }
        }
        if loop_level > 0 && target.is_none() {
            return; // Not inside that many loops: the type checker reports it.
        }
        // What follows the loop (`break`) or its body's start (`continue`) may
        // follow a value written right before the jump.
        if let (true, Some((k, _))) = (loop_level > 0, &target) {
            self.exits[*k].2[if is_break { 0 } else { 1 }] |= self.html.after_value();
        }
        let target = target.map(|(_, at)| at);
        checked.push(target.unwrap_or_default());
        if self.halted || checked.iter().all(|at| self.html.same_place(at)) {
            return;
        }
        let mut d = Diagnostic::error("this jump leaves the HTML inside a tag, attribute, comment or `<script>`", span);
        d.add_note(
            "close what was opened before `break`, `continue`, `return` or `throw`: the output that follows \
             would otherwise land in the wrong place",
        );
        self.diags.push(d);
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let ctx = self.html.context();
        // Code between `</` and the rest of an end tag hidden from the tracker
        // could complete it (found by the 1.0.0 audit: a function called there
        // passed `phv check` and stopped the page only at run time).
        let hidden_end_tag = self.html.watched_end_tag_start();
        let start = self.span();
        let mut s = self.stmt_here()?;
        if hidden_end_tag
            && !self.halted
            && !matches!(s.kind, StmtKind::InlineHtml(..) | StmtKind::Echo(..) | StmtKind::HtmlEcho(..))
        {
            let mut d = Diagnostic::error(
                "code right after `<` or `</` in a comment, an attribute value or raw text, where it could complete `</noscript>` (or, inside `<noscript>`, `<svg>` or `<math>`, another raw-text element's end tag)",
                start,
            );
            d.add_note("a browser would read that end tag there (this template or function may be written inside a `<noscript>`); put a space after `<`, or move the code");
            self.diags.push(d);
        }
        // Inside a `<select>` too, functions may not write output: their HTML
        // was checked outside one, where older browsers read a `<title>` or
        // `<style>` in it as markup (found by the 19th assessment). What a
        // statement there writes itself is checked as it is written.
        if ctx != EchoContext::Text && !matches!(s.kind, StmtKind::InlineHtml(..)) {
            s.out = Some(ctx);
        }
        Ok(s)
    }

    fn stmt_here(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::InlineHtml(text) => {
                self.bump();
                let scripts = self.html.feed(&text);
                for (at, what) in self.html.take_inline_code() {
                    let pos = start.start + at as u32;
                    let len = match &what {
                        crate::htmlctx::InlineCode::Handler(name) => name.len() as u32,
                        crate::htmlctx::InlineCode::ScriptUrl
                        | crate::htmlctx::InlineCode::MetaHttpEquiv
                        | crate::htmlctx::InlineCode::ForeignCdata
                        | crate::htmlctx::InlineCode::HiddenEndTag
                        | crate::htmlctx::InlineCode::CommentEnd
                        | crate::htmlctx::InlineCode::ForeignStructure(_)
                        | crate::htmlctx::InlineCode::CdataEnd
                        | crate::htmlctx::InlineCode::RawTextInSelect(_)
                        | crate::htmlctx::InlineCode::TemplateInSelect => 1,
                    };
                    self.inline_code_warning(Span { start: pos, end: pos + len, ..start }, what);
                }
                StmtKind::InlineHtml(text, scripts)
            }
            Tok::EchoOpen => {
                self.bump();
                let ctx = self.html.context();
                self.html.inserted();
                let exprs = self.expr_list()?;
                self.eat(&Tok::Semi);
                if !self.at(&Tok::CloseTag) && !self.at(&Tok::Eof) {
                    return Err(self.error_here("expected `?>` to close `<?=`"));
                }
                StmtKind::HtmlEcho(exprs, ctx)
            }
            Tok::Echo => {
                self.bump();
                // `echo` between pieces of HTML lands in the same context as `<?= ?>`
                // (inside a `<select>`, its `Html` is checked as well).
                let ctx = self.html.context();
                let exprs = self.expr_list()?;
                self.semi()?;
                if ctx == EchoContext::Text {
                    StmtKind::Echo(exprs)
                } else {
                    self.html.inserted();
                    StmtKind::HtmlEcho(exprs, ctx)
                }
            }
            Tok::If => return self.if_stmt(),
            Tok::While => {
                self.bump();
                let cond = self.paren_expr()?;
                let body = self.loop_body(|p| {
                    if p.eat(&Tok::Colon) {
                        let b = p.block_until(&[Tok::Endwhile]);
                        p.expect(Tok::Endwhile)?;
                        p.semi()?;
                        Ok(b)
                    } else {
                        p.stmt_or_block()
                    }
                })?;
                StmtKind::While { cond, body }
            }
            Tok::Do => {
                self.bump();
                let body = self.loop_body(|p| p.stmt_or_block())?;
                self.expect(Tok::While)?;
                let cond = self.paren_expr()?;
                self.semi()?;
                StmtKind::DoWhile { body, cond }
            }
            Tok::For => return self.for_stmt(),
            Tok::Foreach => return self.foreach_stmt(),
            Tok::Try => return self.try_stmt(),
            Tok::Return => {
                let span = self.bump();
                self.check_exit(0, false, false, span);
                let value =
                    if matches!(self.peek(), Tok::Semi | Tok::CloseTag | Tok::Eof) { None } else { Some(self.expr()?) };
                self.semi()?;
                StmtKind::Return(value)
            }
            Tok::Break | Tok::Continue => {
                let is_break = self.at(&Tok::Break);
                let span = self.bump();
                let mut level = 1;
                if let Tok::Int(n) = *self.peek() {
                    self.bump();
                    level = n.max(1) as u32;
                }
                self.check_exit(level, false, is_break, span);
                self.semi()?;
                if is_break {
                    StmtKind::Break(level)
                } else {
                    StmtKind::Continue(level)
                }
            }
            Tok::Throw => {
                let span = self.bump();
                self.check_exit(0, true, false, span);
                let e = self.expr()?;
                self.semi()?;
                StmtKind::Throw(e)
            }
            Tok::Yield => self.yield_stmt()?,
            Tok::LBrace => StmtKind::Block(self.block()?),
            Tok::Semi => {
                self.bump();
                StmtKind::Block(Vec::new())
            }
            Tok::Hash => {
                let attrs = self.attributes()?;
                let mut s = self.stmt()?;
                match &mut s.kind {
                    StmtKind::Function(d) => d.attrs = attrs,
                    StmtKind::Class(d) => d.attrs = attrs,
                    StmtKind::Const(d) => d.attrs = attrs,
                    _ => {
                        let span = attrs[0].span;
                        self.diags
                            .push(Diagnostic::error("annotations can only be written before a declaration", span));
                    }
                }
                return Ok(s);
            }
            Tok::Function if matches!(self.peek_at(1), Tok::Ident(_)) => {
                StmtKind::Function(Box::new(self.function_decl(false, Visibility::Public, false)?))
            }
            Tok::Class | Tok::Interface | Tok::Enum | Tok::Abstract | Tok::Final => {
                StmtKind::Class(Box::new(self.class_decl(false)?))
            }
            Tok::Ident(ref name) if name == "trait" && self.at_trait(0) => {
                StmtKind::Class(Box::new(self.class_decl(false)?))
            }
            Tok::Const => StmtKind::Const(Box::new(self.const_decl(false)?)),
            Tok::Ident(ref name) if name == "type" && self.at_type_alias(0) => {
                StmtKind::TypeAlias(Box::new(self.type_alias(false)?))
            }
            Tok::Export => {
                self.bump();
                match self.peek() {
                    Tok::Function => {
                        StmtKind::Function(Box::new(self.function_decl(true, Visibility::Public, false)?))
                    }
                    Tok::Class | Tok::Interface | Tok::Enum | Tok::Abstract | Tok::Final => {
                        StmtKind::Class(Box::new(self.class_decl(true)?))
                    }
                    Tok::Ident(name) if name == "trait" && self.at_trait(0) => {
                        StmtKind::Class(Box::new(self.class_decl(true)?))
                    }
                    Tok::Const => StmtKind::Const(Box::new(self.const_decl(true)?)),
                    Tok::Ident(name) if name == "type" && self.at_type_alias(0) => {
                        StmtKind::TypeAlias(Box::new(self.type_alias(true)?))
                    }
                    _ => return Err(self.error_here("expected a declaration after `export`")),
                }
            }
            Tok::Import => StmtKind::Import(self.import_decl()?),
            Tok::Ident(ref name) if name == "params" && self.is_template && *self.peek_at(1) == Tok::LParen => {
                self.bump();
                let params = self.params()?;
                self.semi()?;
                StmtKind::Params(params)
            }
            _ => {
                let kind = self.simple_stmt()?;
                self.semi()?;
                kind
            }
        };
        Ok(Stmt { out: None, kind, span: start.to(self.prev_span()) })
    }

    /// `yield $value;`, `yield $key => $value;` or `yield from $values;`. As in
    /// PHP, `yield from` is a pair of keywords: `from` right after `yield`
    /// always delegates (so `yield from($xs);` and `yield from ($xs);` do too).
    /// Elsewhere `from` is a name (`Status::from()`, a function `from()`); a
    /// value starting with a function named `from` is yielded in parentheses:
    /// `yield (from($x));`.
    fn yield_stmt(&mut self) -> PResult<StmtKind> {
        let span = self.bump();
        if self.is_template {
            let mut d = Diagnostic::error("`yield` cannot be used in a template", span);
            d.add_note("write the generator in a `.phv` module and loop over it in the template");
            self.diags.push(d);
        }
        if matches!(self.peek(), Tok::Semi | Tok::CloseTag | Tok::Eof) {
            return Err(self.error_here("`yield` needs a value: `yield $value;`"));
        }
        let from = matches!(self.peek(), Tok::Ident(n) if n == "from");
        let kind = if from {
            self.bump();
            if matches!(self.peek(), Tok::Semi | Tok::CloseTag | Tok::Eof | Tok::FatArrow) {
                let e = self.error_here("`yield from` needs the values it yields: `yield from $values;`");
                self.note_last(
                    "`from` after `yield` is a keyword; to yield a value named `from`, write `yield (from);`",
                );
                return Err(e);
            }
            let values = self.expr()?;
            if self.at(&Tok::FatArrow) {
                let e = self.error_here("`yield from` takes no key: it yields the keys of the values it yields");
                self.note_last(
                    "`from` after `yield` is a keyword; to yield a value of a function named `from`, put it in \
                     parentheses: `yield (from(...)) => ...;`",
                );
                return Err(e);
            }
            StmtKind::YieldFrom(values)
        } else {
            let first = self.expr()?;
            if self.eat(&Tok::FatArrow) {
                StmtKind::Yield { key: Some(first), value: self.expr()? }
            } else {
                StmtKind::Yield { key: None, value: first }
            }
        };
        self.semi()?;
        Ok(kind)
    }

    /// Assignment, increment/decrement, typed declaration or expression statement.
    fn simple_stmt(&mut self) -> PResult<StmtKind> {
        if let Some(kind) = self.try_typed_decl()? {
            return Ok(kind);
        }
        if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
            let inc = self.at(&Tok::PlusPlus);
            self.bump();
            let target = self.expr()?;
            return Ok(StmtKind::IncDec { target, inc });
        }
        let e = self.expr()?;
        if let Some(op) = assign_op(self.peek()) {
            self.bump();
            let value = self.expr()?;
            return Ok(StmtKind::Assign { target: e, op, value });
        }
        if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
            let inc = self.at(&Tok::PlusPlus);
            self.bump();
            return Ok(StmtKind::IncDec { target: e, inc });
        }
        Ok(StmtKind::Expr(e))
    }

    fn try_typed_decl(&mut self) -> PResult<Option<StmtKind>> {
        let looks_like_type = match self.peek() {
            Tok::Ident(_) => matches!(self.peek_at(1), Tok::Var(_) | Tok::Lt | Tok::Pipe),
            Tok::Question => true,
            _ => false,
        };
        if !looks_like_type {
            return Ok(None);
        }
        let saved_pos = self.pos;
        let saved_diags = self.diags.len();
        let saved_splits = self.splits.len();
        if let Ok(ty) = self.ty() {
            if let (Tok::Var(name), Tok::Eq) = (self.peek().clone(), self.peek_at(1).clone()) {
                let name_span = self.bump();
                self.bump();
                let value = self.expr()?;
                return Ok(Some(StmtKind::TypedDecl { ty: Box::new(ty), name, name_span, value }));
            }
        }
        self.pos = saved_pos;
        self.diags.truncate(saved_diags);
        for (i, tok) in self.splits.drain(saved_splits..).rev() {
            self.toks[i] = tok;
        }
        Ok(None)
    }

    fn paren_expr(&mut self) -> PResult<Expr> {
        self.expect(Tok::LParen)?;
        let e = self.expr()?;
        self.expect(Tok::RParen)?;
        Ok(e)
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump();
        let cond = self.paren_expr()?;
        let mut elifs = Vec::new();
        let mut els = None;
        let then;
        if self.eat(&Tok::Colon) {
            // Template-friendly syntax: if (...): ... elseif (...): ... else: ... endif;
            then = self.block_until(&[Tok::Elseif, Tok::Else, Tok::Endif]);
            loop {
                // Each `elseif` nests the tree one level deeper (in the checker and compiler).
                self.enter(elifs.len() as u32)?;
                if self.eat(&Tok::Elseif) {
                    let c = self.paren_expr()?;
                    self.expect(Tok::Colon)?;
                    let b = self.block_until(&[Tok::Elseif, Tok::Else, Tok::Endif]);
                    elifs.push((c, b));
                } else if self.eat(&Tok::Else) {
                    self.expect(Tok::Colon)?;
                    els = Some(self.block_until(&[Tok::Endif]));
                } else {
                    break;
                }
            }
            self.expect(Tok::Endif)?;
            self.semi()?;
        } else {
            then = self.stmt_or_block()?;
            loop {
                self.enter(elifs.len() as u32)?;
                if self.eat(&Tok::Elseif) {
                    let c = self.paren_expr()?;
                    elifs.push((c, self.stmt_or_block()?));
                } else if self.at(&Tok::Else) && *self.peek_at(1) == Tok::If {
                    self.bump();
                    self.bump();
                    let c = self.paren_expr()?;
                    elifs.push((c, self.stmt_or_block()?));
                } else if self.eat(&Tok::Else) {
                    els = Some(self.stmt_or_block()?);
                    break;
                } else {
                    break;
                }
            }
        }
        let kind = StmtKind::If { cond, then, elifs, els };
        Ok(Stmt { out: None, kind, span: start.to(self.prev_span()) })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump();
        self.expect(Tok::LParen)?;
        let mut init = Vec::new();
        while !self.at(&Tok::Semi) {
            let s = self.span();
            let kind = self.simple_stmt()?;
            init.push(Stmt { out: None, kind, span: s.to(self.prev_span()) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::Semi)?;
        let cond = if self.at(&Tok::Semi) { None } else { Some(self.expr()?) };
        self.expect(Tok::Semi)?;
        let mut step = Vec::new();
        while !self.at(&Tok::RParen) {
            let s = self.span();
            let kind = self.simple_stmt()?;
            step.push(Stmt { out: None, kind, span: s.to(self.prev_span()) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let body = self.loop_body(|p| {
            if p.eat(&Tok::Colon) {
                let b = p.block_until(&[Tok::Endfor]);
                p.expect(Tok::Endfor)?;
                p.semi()?;
                Ok(b)
            } else {
                p.stmt_or_block()
            }
        })?;
        let kind = StmtKind::For { init, cond, step, body };
        Ok(Stmt { out: None, kind, span: start.to(self.prev_span()) })
    }

    fn foreach_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump();
        self.expect(Tok::LParen)?;
        let subject = self.expr()?;
        self.expect(Tok::As)?;
        let mut pattern = None;
        let first = self.foreach_var(&mut pattern)?;
        let (key, value) = if self.eat(&Tok::FatArrow) {
            if pattern.is_some() {
                self.diags.push(Diagnostic::error("a key cannot be destructured", first.1));
            }
            (Some(first), self.foreach_var(&mut pattern)?)
        } else {
            (None, first)
        };
        self.expect(Tok::RParen)?;
        let mut body = self.loop_body(|p| {
            if p.eat(&Tok::Colon) {
                let b = p.block_until(&[Tok::Endforeach]);
                p.expect(Tok::Endforeach)?;
                p.semi()?;
                Ok(b)
            } else {
                p.stmt_or_block()
            }
        })?;
        if let Some(p) = pattern {
            body.insert(0, p);
        }
        let kind = StmtKind::Foreach { subject: Box::new(subject), key, value, body };
        Ok(Stmt { out: None, kind, span: start.to(self.prev_span()) })
    }

    /// The variable after `as`. `as [$a, $b]` takes the value into a hidden
    /// variable, and the body starts by destructuring it (`pattern`).
    fn foreach_var(&mut self, pattern: &mut Option<Stmt>) -> PResult<(String, Span)> {
        if !self.at(&Tok::LBracket) {
            return self.expect_var();
        }
        let start = self.span();
        let kind = self.array_literal()?;
        let span = start.to(self.prev_span());
        let name = format!("%v{}", span.start);
        let value = Expr { kind: ExprKind::Var(name.clone()), span };
        *pattern = Some(Stmt {
            out: None,
            kind: StmtKind::Assign { target: Expr { kind, span }, op: AssignOp::Set, value },
            span,
        });
        Ok((name, span))
    }

    fn try_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump();
        let body = self.exit_target(Exit::Try, |p| p.block())?;
        let mut catches = Vec::new();
        while self.at(&Tok::Catch) {
            let cstart = self.bump();
            self.expect(Tok::LParen)?;
            let mut classes = vec![self.expect_ident("an exception class")?];
            while self.eat(&Tok::Pipe) {
                classes.push(self.expect_ident("an exception class")?);
            }
            let var = match self.peek().clone() {
                Tok::Var(name) => Some((name, self.bump())),
                _ => None,
            };
            self.expect(Tok::RParen)?;
            let body = self.block()?;
            catches.push(Catch { classes, var, body, span: cstart.to(self.prev_span()) });
        }
        // `finally` runs after a `break` or `continue` in the `try` block too.
        let finally = if self.eat(&Tok::Finally) {
            self.html.after_any_value();
            Some(self.block()?)
        } else {
            None
        };
        if catches.is_empty() && finally.is_none() {
            return Err(self.error_here("expected `catch` or `finally` after `try` block"));
        }
        let kind = StmtKind::Try { body, catches, finally };
        Ok(Stmt { out: None, kind, span: start.to(self.prev_span()) })
    }

    fn params(&mut self) -> PResult<Vec<Param>> {
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let start = self.span();
            let mut vis = None;
            let mut readonly = false;
            loop {
                match self.peek() {
                    Tok::Public => vis = Some(Visibility::Public),
                    Tok::Protected => vis = Some(Visibility::Protected),
                    Tok::Private => vis = Some(Visibility::Private),
                    Tok::Readonly => readonly = true,
                    _ => break,
                }
                self.bump();
            }
            let promote =
                if vis.is_some() || readonly { Some((vis.unwrap_or(Visibility::Public), readonly)) } else { None };
            let ty = if matches!(self.peek(), Tok::Var(_) | Tok::Ellipsis) { None } else { Some(self.ty()?) };
            let variadic = self.eat(&Tok::Ellipsis);
            let (name, _) = self.expect_var()?;
            let default = if self.eat(&Tok::Eq) { Some(self.expr()?) } else { None };
            params.push(Param { name, ty, default, promote, variadic, span: start.to(self.prev_span()) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(params)
    }

    fn function_decl(&mut self, exported: bool, visibility: Visibility, is_static: bool) -> PResult<FunctionDecl> {
        self.function_or_method(exported, visibility, is_static, false)
    }

    /// A method may be named like a keyword (`match`, `list`), as it is always
    /// called after `->` or `::`.
    fn function_or_method(
        &mut self,
        exported: bool,
        visibility: Visibility,
        is_static: bool,
        method: bool,
    ) -> PResult<FunctionDecl> {
        let start = self.expect(Tok::Function)?;
        let (name, name_span) = if method { self.member_name()? } else { self.expect_ident("a function name")? };
        let (type_params, type_bounds) = self.bounded_type_params()?;
        let params = self.params()?;
        let ret = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
        let (body, is_abstract) = if self.eat(&Tok::Semi) { (Vec::new(), true) } else { (self.body_block()?, false) };
        Ok(FunctionDecl {
            attrs: Vec::new(),
            name,
            name_span,
            is_final: false,
            type_params,
            type_bounds,
            is_abstract,
            params,
            ret,
            body,
            exported,
            is_static,
            visibility,
            span: start.to(self.prev_span()),
        })
    }

    fn class_decl(&mut self, exported: bool) -> PResult<ClassDecl> {
        let start = self.span();
        let mut kind = ClassKind::Class;
        let mut is_final = false;
        while matches!(self.peek(), Tok::Abstract | Tok::Final) {
            if self.at(&Tok::Abstract) {
                kind = ClassKind::Abstract;
            } else {
                is_final = true;
            }
            self.bump();
        }
        if is_final && kind == ClassKind::Abstract {
            self.diags.push(
                Diagnostic::error("a class cannot be both `abstract` and `final`", start)
                    .note("an abstract class must be extended to be used"),
            );
        }
        let is_trait = matches!(self.peek(), Tok::Ident(n) if n == "trait");
        match self.peek() {
            Tok::Interface | Tok::Enum if is_final => {
                let what = if self.at(&Tok::Interface) { "interfaces" } else { "enums" };
                self.diags.push(Diagnostic::error(format!("{what} cannot be `final`"), start));
            }
            _ if is_trait && (is_final || kind == ClassKind::Abstract) => {
                self.diags.push(Diagnostic::error("a trait cannot be `abstract` or `final`", start).note(
                    "a trait is never instantiated or extended: its members are copied into the classes that use it",
                ));
            }
            _ => {}
        }
        match self.peek() {
            Tok::Interface => kind = ClassKind::Interface,
            Tok::Enum => kind = ClassKind::Enum,
            Tok::Class => {}
            _ if is_trait => kind = ClassKind::Trait,
            other => {
                let found = other.describe();
                return Err(self.error_here(format!("expected `class`, `interface` or `enum`, found {found}")));
            }
        }
        self.bump();
        let (name, name_span) = self.expect_ident("a type name")?;
        let (type_params, type_bounds) = self.bounded_type_params()?;
        let mut backing = None;
        if kind == ClassKind::Enum && self.eat(&Tok::Colon) {
            backing = Some(self.ty()?);
        }
        let mut parent = None;
        let mut parent_args = Vec::new();
        let mut interfaces = Vec::new();
        let mut interface_args = Vec::new();
        if kind == ClassKind::Trait && matches!(self.peek(), Tok::Extends | Tok::Implements) {
            self.diags.push(
                Diagnostic::error("a trait cannot extend a class or implement an interface", self.span())
                    .note("the classes that use the trait can: `class C extends B implements I { use T; }`"),
            );
        }
        if self.eat(&Tok::Extends) {
            if kind == ClassKind::Interface {
                loop {
                    interfaces.push(self.expect_ident("an interface")?);
                    interface_args.push(self.type_args()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            } else {
                parent = Some(self.expect_ident("a parent class")?);
                parent_args = self.type_args()?;
            }
        }
        if self.eat(&Tok::Implements) {
            loop {
                interfaces.push(self.expect_ident("an interface")?);
                interface_args.push(self.type_args()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::LBrace)?;
        let mut props = Vec::new();
        let mut methods = Vec::new();
        let mut consts = Vec::new();
        let mut cases = Vec::new();
        let mut uses = Vec::new();
        while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
            let member_start = self.pos;
            let mstart = self.span();
            let attrs = match self.attributes() {
                Ok(a) => a,
                Err(_) => {
                    self.recover(member_start);
                    continue;
                }
            };
            if self.at(&Tok::Use) {
                if let Some(a) = attrs.first() {
                    self.diags.push(Diagnostic::error("annotations cannot be written before `use`", a.span));
                }
                if kind == ClassKind::Interface {
                    self.diags.push(Diagnostic::error("an interface cannot use traits", mstart).note(
                        "an interface declares methods without bodies; use the trait in the classes that implement it",
                    ));
                }
                match self.trait_use() {
                    Ok(u) => uses.push(u),
                    Err(_) => self.recover(member_start),
                }
                continue;
            }
            if let (Some(a), Tok::Case | Tok::RBrace) = (attrs.first(), self.peek()) {
                self.diags.push(Diagnostic::error("annotations cannot be written before an enum case", a.span));
            }
            if self.at(&Tok::Case) {
                let r: PResult<()> = (|| {
                    self.bump();
                    let (cname, _) = self.expect_ident("a case name")?;
                    let value = if self.eat(&Tok::Eq) { Some(self.expr()?) } else { None };
                    self.expect(Tok::Semi)?;
                    cases.push(EnumCase { name: cname, value, span: mstart.to(self.prev_span()) });
                    Ok(())
                })();
                if r.is_err() {
                    self.recover(member_start);
                }
                continue;
            }
            let mut vis = Visibility::Public;
            let mut is_static = false;
            let mut readonly = false;
            let mut is_abstract = kind == ClassKind::Interface;
            let mut is_final = false;
            loop {
                match self.peek() {
                    Tok::Public => vis = Visibility::Public,
                    Tok::Protected => vis = Visibility::Protected,
                    Tok::Private => vis = Visibility::Private,
                    Tok::Static => is_static = true,
                    Tok::Readonly => readonly = true,
                    Tok::Abstract => is_abstract = true,
                    Tok::Final => is_final = true,
                    _ => break,
                }
                self.bump();
            }
            if is_final && !self.at(&Tok::Function) {
                self.diags.push(Diagnostic::error("only classes and methods can be `final`", mstart));
            }
            let result = if self.at(&Tok::Function) {
                self.function_or_method(false, vis, is_static, true).map(|mut f| {
                    f.attrs = attrs;
                    f.is_final = is_final;
                    if is_final && is_abstract {
                        let msg = if kind == ClassKind::Interface {
                            "interface methods cannot be `final`"
                        } else {
                            "a method cannot be both `abstract` and `final`"
                        };
                        self.diags.push(Diagnostic::error(msg, f.name_span));
                    }
                    if f.is_abstract != is_abstract {
                        let msg = if is_abstract {
                            "abstract and interface methods cannot have a body"
                        } else {
                            "this method needs a body (or mark it `abstract`)"
                        };
                        self.diags.push(Diagnostic::error(msg, f.name_span));
                    }
                    methods.push(f);
                })
            } else if self.at(&Tok::Const) {
                self.const_decl(false).map(|mut c| {
                    c.attrs = attrs;
                    consts.push(c)
                })
            } else if matches!(self.peek(), Tok::Var(_)) {
                Err(self.error_here("properties must declare a type, e.g. `public string $name;`"))
            } else {
                (|| {
                    let ty = self.ty()?;
                    let (pname, _) = self.expect_var()?;
                    let default = if self.eat(&Tok::Eq) { Some(self.expr()?) } else { None };
                    self.expect(Tok::Semi)?;
                    if is_static {
                        self.diags.push(
                            Diagnostic::error(
                                "static properties are not supported (they would share state between requests)",
                                mstart,
                            )
                            .note("use a class constant (`const MAX = 10;`) for fixed values"),
                        );
                    }
                    props.push(PropDecl {
                        attrs,
                        name: pname,
                        ty,
                        default,
                        visibility: vis,
                        readonly,
                        span: mstart.to(self.prev_span()),
                    });
                    Ok(())
                })()
            };
            if result.is_err() {
                self.recover(member_start);
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(ClassDecl {
            attrs: Vec::new(),
            name,
            name_span,
            kind,
            is_final,
            type_params,
            type_bounds,
            parent,
            parent_args,
            interfaces,
            interface_args,
            uses,
            props,
            methods,
            consts,
            cases,
            backing,
            exported,
            span: start.to(self.prev_span()),
        })
    }

    /// `trait Name {` or `trait Name<`: `trait` is only a keyword here.
    fn at_trait(&self, at: usize) -> bool {
        matches!(self.peek_at(at + 1), Tok::Ident(_)) && matches!(self.peek_at(at + 2), Tok::LBrace | Tok::Lt)
    }

    /// `use A, B<int>;` or `use A, B { A::m insteadof B; B::m as protected n; }`
    /// in a class body.
    fn trait_use(&mut self) -> PResult<TraitUse> {
        let start = self.expect(Tok::Use)?;
        let mut traits = Vec::new();
        loop {
            let (name, span) = self.expect_ident("a trait name")?;
            let args = self.type_args()?;
            traits.push(TraitRef { name, span, args });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let mut rules = Vec::new();
        if self.eat(&Tok::LBrace) {
            while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
                let rule_start = self.pos;
                match self.trait_rule() {
                    Ok(r) => rules.push(r),
                    Err(_) => self.recover(rule_start),
                }
            }
            self.expect(Tok::RBrace)?;
            // PHP writes no `;` after the block; one is allowed.
            self.eat(&Tok::Semi);
        } else {
            self.expect(Tok::Semi)?;
        }
        Ok(TraitUse { traits, rules, span: start.to(self.prev_span()) })
    }

    /// One rule of a `use` block: `A::m insteadof B, C;` or `[A::]m as [visibility] [alias];`.
    fn trait_rule(&mut self) -> PResult<TraitRule> {
        let start = self.span();
        let (from, method) = if matches!(self.peek_at(1), Tok::DoubleColon) {
            let from = self.expect_ident("a trait name")?;
            self.bump();
            (Some(from), self.member_name()?)
        } else {
            (None, self.member_name()?)
        };
        if matches!(self.peek(), Tok::Ident(n) if n == "insteadof") {
            self.bump();
            let Some(from) = from else {
                return Err(self.error_here("`insteadof` needs the trait whose method is kept: `A::name insteadof B;`"));
            };
            let mut excluded = Vec::new();
            loop {
                excluded.push(self.expect_ident("a trait name")?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Semi)?;
            return Ok(TraitRule::Insteadof { from, method, excluded, span: start.to(self.prev_span()) });
        }
        if !self.eat(&Tok::As) {
            let found = self.peek().describe();
            return Err(self.error_here(format!("expected `insteadof` or `as`, found {found}")));
        }
        let vis = match self.peek() {
            Tok::Public => Some(Visibility::Public),
            Tok::Protected => Some(Visibility::Protected),
            Tok::Private => Some(Visibility::Private),
            _ => None,
        };
        if vis.is_some() {
            self.bump();
        }
        let alias = if self.at(&Tok::Semi) { None } else { Some(self.member_name()?) };
        if vis.is_none() && alias.is_none() {
            return Err(self.error_here("expected a visibility or a new name after `as`"));
        }
        self.expect(Tok::Semi)?;
        Ok(TraitRule::As { from, method, vis, alias, span: start.to(self.prev_span()) })
    }

    /// `type Name =` or `type Name<`: `type` is only a keyword here.
    fn at_type_alias(&self, at: usize) -> bool {
        matches!(self.peek_at(at + 1), Tok::Ident(_)) && matches!(self.peek_at(at + 2), Tok::Eq | Tok::Lt)
    }

    fn type_alias(&mut self, exported: bool) -> PResult<TypeAliasDecl> {
        let start = self.span();
        self.bump();
        let (name, name_span) = self.expect_ident("a type name")?;
        let type_params = self.type_params()?;
        self.expect(Tok::Eq)?;
        let ty = self.ty()?;
        self.semi()?;
        Ok(TypeAliasDecl { name, name_span, type_params, ty, exported, span: start.to(self.prev_span()) })
    }

    fn const_decl(&mut self, exported: bool) -> PResult<ConstDecl> {
        let start = self.expect(Tok::Const)?;
        let ty = if matches!(self.peek_at(1), Tok::Ident(_) | Tok::Lt | Tok::Pipe) || self.at(&Tok::Question) {
            Some(self.ty()?)
        } else {
            None
        };
        let (name, name_span) = self.expect_ident("a constant name")?;
        self.expect(Tok::Eq)?;
        let value = self.expr()?;
        self.semi()?;
        Ok(ConstDecl { attrs: Vec::new(), name, name_span, ty, value, exported, span: start.to(self.prev_span()) })
    }

    fn import_decl(&mut self) -> PResult<ImportDecl> {
        let start = self.expect(Tok::Import)?;
        let mut segments = vec![self.expect_ident("a module name")?];
        let mut names = Vec::new();
        while self.eat(&Tok::Dot) {
            if self.eat(&Tok::LBrace) {
                while !self.at(&Tok::RBrace) {
                    let (name, span) = self.expect_ident("a name to import")?;
                    let alias = self.import_alias()?;
                    names.push(ImportName { name, span, alias });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBrace)?;
                break;
            }
            segments.push(self.expect_ident("a module or name")?);
        }
        if names.is_empty() {
            if segments.len() < 2 {
                return Err(self.error_here("expected `import module.name;` or `import module.{a, b};`"));
            }
            let (name, span) = segments.pop().unwrap();
            let alias = self.import_alias()?;
            names.push(ImportName { name, span, alias });
        }
        self.semi()?;
        let module = segments.into_iter().map(|(s, _)| s).collect();
        Ok(ImportDecl { module, names, span: start.to(self.prev_span()) })
    }

    /// `as Other` after an imported name.
    fn import_alias(&mut self) -> PResult<Option<(String, Span)>> {
        if self.eat(&Tok::As) {
            Ok(Some(self.expect_ident("a name after `as`")?))
        } else {
            Ok(None)
        }
    }

    // ----- types ---------------------------------------------------------

    fn ty(&mut self) -> PResult<TypeExpr> {
        let first = self.ty_atom()?;
        if !self.at(&Tok::Pipe) {
            return Ok(first);
        }
        let start = first.span;
        let mut members = vec![first];
        while self.eat(&Tok::Pipe) {
            members.push(self.ty_atom()?);
        }
        Ok(TypeExpr { kind: TypeExprKind::Union(members), span: start.to(self.prev_span()) })
    }

    fn ty_atom(&mut self) -> PResult<TypeExpr> {
        // `?`, `(...)`, `list<...>` and `fn(...): ...` nest types inside each other.
        self.enter(0)?;
        self.depth += 1;
        let t = self.ty_atom_inner();
        self.depth -= 1;
        t
    }

    fn ty_atom_inner(&mut self) -> PResult<TypeExpr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Question => {
                self.bump();
                TypeExprKind::Nullable(Box::new(self.ty_atom()?))
            }
            Tok::Null => {
                self.bump();
                TypeExprKind::Named("null".into(), Vec::new())
            }
            Tok::Fn => {
                self.bump();
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                while !self.at(&Tok::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
                self.expect(Tok::Colon)?;
                let ret = self.ty_atom()?;
                TypeExprKind::Func(params, Box::new(ret))
            }
            Tok::LParen => {
                self.bump();
                let t = self.ty()?;
                self.expect(Tok::RParen)?;
                return Ok(t);
            }
            Tok::Ident(name) => {
                self.bump();
                let mut args = Vec::new();
                if self.eat(&Tok::Lt) {
                    loop {
                        args.push(self.ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect_gt()?;
                }
                TypeExprKind::Named(name, args)
            }
            other => return Err(self.error_here(format!("expected a type, found {}", other.describe()))),
        };
        Ok(TypeExpr { kind, span: start.to(self.prev_span()) })
    }

    /// `#[Name]`, `#[Name("text", 1)]`, `#[A, B]`: the annotations before a
    /// declaration (empty when there are none).
    fn attributes(&mut self) -> PResult<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.at(&Tok::Hash) {
            self.bump();
            self.expect(Tok::LBracket)?;
            loop {
                let start = self.span();
                let (name, name_span) = self.expect_ident("an annotation name")?;
                let args = if self.at(&Tok::LParen) { self.args()? } else { Vec::new() };
                attrs.push(Attribute { name, name_span, args, span: start.to(self.prev_span()) });
                if !self.eat(&Tok::Comma) || self.at(&Tok::RBracket) {
                    break;
                }
            }
            self.expect(Tok::RBracket)?;
        }
        Ok(attrs)
    }

    /// `<T, U>` after a function or class name (empty when there is none).
    fn type_params(&mut self) -> PResult<Vec<(String, Span)>> {
        let (names, bounds) = self.bounded_type_params()?;
        if let Some(b) = bounds.iter().flatten().next() {
            self.diags.push(Diagnostic::error("type parameters of a type alias cannot have bounds", b.span));
        }
        Ok(names)
    }

    /// `<T, U: Shape>` (empty when there is none).
    #[allow(clippy::type_complexity)]
    fn bounded_type_params(&mut self) -> PResult<(Vec<(String, Span)>, Vec<Option<TypeExpr>>)> {
        let mut names = Vec::new();
        let mut bounds = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                names.push(self.expect_ident("a type parameter")?);
                bounds.push(if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect_gt()?;
        }
        // Each use of a type looks its parameters up by name: a long list would
        // make checking quadratic, so it is refused before anything is resolved.
        if names.len() > 4 {
            self.diags.push(Diagnostic::error("at most 4 type parameters are supported", names[4].1));
            names.truncate(4);
            bounds.truncate(4);
        }
        Ok((names, bounds))
    }

    /// `<int, string>` after a class name (empty when there is none).
    fn type_args(&mut self) -> PResult<Vec<TypeExpr>> {
        let mut args = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                args.push(self.ty()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect_gt()?;
        }
        Ok(args)
    }

    /// Accepts `>`; splits `>>` so `list<list<int>>` parses.
    fn expect_gt(&mut self) -> PResult<()> {
        if self.at(&Tok::Shr) || self.at(&Tok::Ge) {
            self.splits.push((self.pos, self.toks[self.pos].clone()));
            let t = &mut self.toks[self.pos];
            t.tok = if t.tok == Tok::Shr { Tok::Gt } else { Tok::Eq };
            t.span.start += 1;
            return Ok(());
        }
        self.expect(Tok::Gt).map(|_| ())
    }

    // ----- expressions ---------------------------------------------------

    fn expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut exprs = vec![self.expr()?];
        while self.eat(&Tok::Comma) {
            exprs.push(self.expr()?);
        }
        Ok(exprs)
    }

    pub(crate) fn expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.enter(0)?;
        self.depth += 1;
        let r = self.expr_bp_inner(min_bp);
        self.depth -= 1;
        r
    }

    fn expr_bp_inner(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        // Each operator in a chain (`a + b + c ...`) nests the tree one level deeper.
        let mut chain = 0;
        loop {
            self.enter(chain)?;
            chain += 1;
            let tok = self.peek().clone();
            if tok == Tok::Question && min_bp <= 1 {
                self.bump();
                let then = self.expr_bp(0)?;
                self.expect(Tok::Colon)?;
                let els = self.expr_bp(1)?;
                let span = lhs.span.to(els.span);
                lhs = Expr { kind: ExprKind::Ternary(Box::new(lhs), Box::new(then), Box::new(els)), span };
                continue;
            }
            if matches!(tok, Tok::Is | Tok::Instanceof) && min_bp <= BP_IS {
                self.bump();
                let ty = self.ty()?;
                let span = lhs.span.to(ty.span);
                lhs = Expr { kind: ExprKind::Is(Box::new(lhs), Box::new(ty)), span };
                continue;
            }
            if matches!(tok, Tok::DotDot | Tok::DotDotEq) && min_bp <= 18 {
                self.bump();
                let rhs = self.expr_bp(19)?;
                let span = lhs.span.to(rhs.span);
                let kind = ExprKind::Range { from: Box::new(lhs), to: Box::new(rhs), inclusive: tok == Tok::DotDotEq };
                lhs = Expr { kind, span };
                continue;
            }
            let Some((op, lbp, rbp)) = binop(&tok) else { break };
            if lbp < min_bp {
                break;
            }
            if matches!(tok, Tok::EqEqEq | Tok::NotEqEq) {
                let span = self.span();
                let (strict, plain) = if tok == Tok::EqEqEq { ("===", "==") } else { ("!==", "!=") };
                self.diags.push(
                    Diagnostic::error(format!("`{strict}` is not needed in PHV"), span)
                        .label(format!("use `{plain}`"))
                        .note("`==` and `!=` are always strict: values of different types are never equal"),
                );
            }
            self.bump();
            let rhs = self.expr_bp(rbp)?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        self.enter(0)?;
        self.depth += 1;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> PResult<Expr> {
        let start = self.span();
        let un = |op, e: Expr| ExprKind::Unary(op, Box::new(e));
        let kind = match self.peek().clone() {
            Tok::Bang => {
                self.bump();
                un(UnOp::Not, self.expr_bp(BP_UNARY)?)
            }
            Tok::Tilde => {
                self.bump();
                un(UnOp::BitNot, self.expr_bp(BP_UNARY)?)
            }
            Tok::Minus => {
                self.bump();
                let operand = self.expr_bp(BP_UNARY)?;
                match operand.kind {
                    ExprKind::Int(v) => ExprKind::Int(v.wrapping_neg()),
                    ExprKind::Float(v) => ExprKind::Float(-v),
                    _ => un(UnOp::Neg, operand),
                }
            }
            Tok::Plus => {
                self.bump();
                return self.expr_bp(BP_UNARY);
            }
            Tok::LParen => {
                let cast = match self.peek_at(1) {
                    Tok::Ident(n) if *self.peek_at(2) == Tok::RParen => match n.as_str() {
                        "int" => Some(CastKind::Int),
                        "float" => Some(CastKind::Float),
                        "string" => Some(CastKind::String),
                        "bool" => Some(CastKind::Bool),
                        _ => None,
                    },
                    _ => None,
                };
                match cast {
                    Some(c) => {
                        self.bump();
                        self.bump();
                        self.bump();
                        ExprKind::Cast(c, Box::new(self.expr_bp(BP_UNARY)?))
                    }
                    None => return self.postfix(),
                }
            }
            Tok::Throw => {
                self.bump();
                ExprKind::Throw(Box::new(self.expr_bp(0)?))
            }
            Tok::Yield => {
                if !self.halted {
                    let mut d = Diagnostic::error("`yield` is a statement in PHV, not an expression", start);
                    d.add_note(
                        "write `yield $value;` on its own: a generator does not receive values (there is no `send()`)",
                    );
                    self.diags.push(d);
                }
                return Err(PErr);
            }
            _ => return self.postfix(),
        };
        Ok(Expr { kind, span: start.to(self.prev_span()) })
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        let mut chain = 0;
        loop {
            self.enter(chain)?;
            chain += 1;
            match self.peek().clone() {
                Tok::LParen if self.at_func_ref() => {
                    let span = e.span;
                    e = self
                        .func_ref(Expr { kind: ExprKind::Call { callee: Box::new(e), args: Box::default() }, span });
                }
                Tok::LParen => {
                    let args = self.args()?;
                    if let ExprKind::Name(n) = &e.kind {
                        if n == "layout" {
                            self.uses_layout = true;
                            if !self.html.at_top_level() {
                                let inside = if self.html.only_select_open() {
                                    "an open `<select>`"
                                } else {
                                    "a tag, attribute, comment or `<script>`"
                                };
                                let mut d = Diagnostic::error(format!("`layout()` is called inside {inside}"), e.span);
                                d.add_note("the layout's HTML would be written there; call it where the page's HTML is at the top");
                                self.diags.push(d);
                            }
                        }
                        if n == "render" || n == "layout" {
                            if let Some(Arg { name: None, value: Expr { kind: ExprKind::Str(path), span } }) =
                                args.first()
                            {
                                self.template_refs.push((normalize_template_path(path), *span));
                            }
                        }
                    }
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args: args.into() }, span };
                }
                Tok::LBracket => {
                    self.bump();
                    let index = if self.at(&Tok::RBracket) { None } else { Some(Box::new(self.expr()?)) };
                    self.expect(Tok::RBracket)?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Index { base: Box::new(e), index }, span };
                }
                Tok::Arrow | Tok::NullsafeArrow => {
                    let nullsafe = self.at(&Tok::NullsafeArrow);
                    self.bump();
                    let (name, name_span) = self.member_name()?;
                    if self.at_func_ref() {
                        let span = e.span.to(name_span);
                        let kind = ExprKind::MethodCall(Box::new(MethodCall {
                            obj: Box::new(e),
                            name,
                            name_span,
                            args: Vec::new(),
                            nullsafe,
                        }));
                        e = self.func_ref(Expr { kind, span });
                    } else if self.at(&Tok::LParen) {
                        let args = self.args()?;
                        let span = e.span.to(self.prev_span());
                        e = Expr {
                            kind: ExprKind::MethodCall(Box::new(MethodCall {
                                obj: Box::new(e),
                                name,
                                name_span,
                                args,
                                nullsafe,
                            })),
                            span,
                        };
                    } else {
                        let span = e.span.to(self.prev_span());
                        e = Expr {
                            kind: ExprKind::Prop(Box::new(Prop { obj: Box::new(e), name, name_span, nullsafe })),
                            span,
                        };
                    }
                }
                Tok::DoubleColon => {
                    let ExprKind::Name(class) = &e.kind else {
                        return Err(self.error_here("`::` must follow a class name"));
                    };
                    let class = class.clone();
                    let class_span = e.span;
                    self.bump();
                    let (name, name_span) = self.member_name()?;
                    if !self.at(&Tok::LParen) {
                        let span = e.span.to(self.prev_span());
                        e = Expr {
                            kind: ExprKind::ClassConst(Box::new(ClassConst { class, class_span, name, name_span })),
                            span,
                        };
                        continue;
                    }
                    if self.at_func_ref() {
                        let span = e.span.to(name_span);
                        let kind = ExprKind::StaticCall(Box::new(StaticCall {
                            class,
                            class_span,
                            name,
                            name_span,
                            args: Vec::new(),
                        }));
                        e = self.func_ref(Expr { kind, span });
                        continue;
                    }
                    let args = self.args()?;
                    let span = e.span.to(self.prev_span());
                    e = Expr {
                        kind: ExprKind::StaticCall(Box::new(StaticCall { class, class_span, name, name_span, args })),
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// A name after `->` or `::`. Keywords are allowed so `$x->default` works.
    fn member_name(&mut self) -> PResult<(String, Span)> {
        let t = self.peek().clone();
        match t {
            Tok::Ident(name) => Ok((name, self.bump())),
            Tok::Default
            | Tok::Match
            | Tok::Is
            | Tok::New
            | Tok::Echo
            | Tok::Use
            | Tok::Import
            | Tok::Export
            | Tok::Static
            | Tok::Class
            | Tok::As
            | Tok::Do
            | Tok::Try
            | Tok::Catch
            | Tok::Throw
            | Tok::Yield
            | Tok::Interface
            | Tok::Enum
            | Tok::Case
            | Tok::Const
            | Tok::Abstract
            | Tok::Final
            | Tok::Implements => {
                let span = self.bump();
                let text = t.describe().trim_matches('`').to_string();
                Ok((text, span))
            }
            other => Err(self.error_here(format!("expected a member name, found {}", other.describe()))),
        }
    }

    fn args(&mut self) -> PResult<Vec<Arg>> {
        self.expect(Tok::LParen)?;
        let mut args = Vec::new();
        while !self.at(&Tok::RParen) {
            let name = match (self.peek().clone(), self.peek_at(1)) {
                (Tok::Ident(n), Tok::Colon) => {
                    let span = self.bump();
                    self.bump();
                    Some((n, span))
                }
                _ => None,
            };
            let value = self.spreadable()?;
            args.push(Arg { name, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        args.shrink_to_fit();
        Ok(args)
    }

    /// An expression, or `...expr` (an argument or list item to spread).
    fn spreadable(&mut self) -> PResult<Expr> {
        if !self.at(&Tok::Ellipsis) {
            return self.expr();
        }
        let start = self.bump();
        let inner = self.expr()?;
        let span = start.to(inner.span);
        Ok(Expr { kind: ExprKind::Spread(Box::new(inner)), span })
    }

    /// `(...)` after a function, method or static method name: the function as a value.
    fn at_func_ref(&self) -> bool {
        self.at(&Tok::LParen) && *self.peek_at(1) == Tok::Ellipsis && *self.peek_at(2) == Tok::RParen
    }

    fn func_ref(&mut self, call: Expr) -> Expr {
        for _ in 0..3 {
            self.bump();
        }
        let span = call.span.to(self.prev_span());
        Expr { kind: ExprKind::FuncRef(Box::new(call)), span }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Var(name) => {
                self.bump();
                ExprKind::Var(name)
            }
            Tok::Int(v) => {
                self.bump();
                ExprKind::Int(v)
            }
            Tok::Float(v) => {
                self.bump();
                ExprKind::Float(v)
            }
            Tok::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            Tok::Interp(parts) => {
                self.bump();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        StrPart::Lit(s) => out.push(InterpPart::Lit(s)),
                        StrPart::Var(name, span) => {
                            out.push(InterpPart::Expr(Expr { kind: ExprKind::Var(name), span }))
                        }
                        StrPart::Expr(code, offset) => out.push(InterpPart::Expr(self.sub_expr(&code, offset)?)),
                    }
                }
                ExprKind::Interp(out)
            }
            Tok::True => {
                self.bump();
                ExprKind::Bool(true)
            }
            Tok::False => {
                self.bump();
                ExprKind::Bool(false)
            }
            Tok::Null => {
                self.bump();
                ExprKind::Null
            }
            Tok::Ident(name) => {
                self.bump();
                ExprKind::Name(name)
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                let span = start.to(self.prev_span());
                let kind = match e.kind {
                    ExprKind::MethodCall { .. } | ExprKind::Prop { .. } | ExprKind::Index { .. } => {
                        ExprKind::Paren(Box::new(Expr { kind: e.kind, span: e.span }))
                    }
                    other => other,
                };
                return Ok(Expr { kind, span });
            }
            Tok::Sql(parts) => {
                self.bump();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        StrPart::Lit(s) => out.push(InterpPart::Lit(s)),
                        StrPart::Var(name, span) => {
                            out.push(InterpPart::Expr(Expr { kind: ExprKind::Var(name), span }))
                        }
                        StrPart::Expr(code, offset) => out.push(InterpPart::Expr(self.sub_expr(&code, offset)?)),
                    }
                }
                ExprKind::Sql(out)
            }
            Tok::LBracket => self.array_literal()?,
            Tok::New => {
                self.bump();
                let (class, class_span) = self.expect_ident("a class name")?;
                let type_args = self.type_args()?;
                let args = if self.at(&Tok::LParen) { self.args()? } else { Vec::new() };
                ExprKind::New(Box::new(New { class, class_span, type_args, args }))
            }
            Tok::Fn => {
                self.bump();
                let params = self.params()?;
                let ret = if self.eat(&Tok::Colon) { Some(self.ty_atom()?) } else { None };
                self.expect(Tok::FatArrow)?;
                let body = self.expr_bp(0)?;
                ExprKind::Closure(Box::new(Closure {
                    params,
                    ret,
                    uses: Vec::new(),
                    body: ClosureBody::Expr(Box::new(body)),
                    arrow: true,
                }))
            }
            Tok::Function => {
                self.bump();
                let params = self.params()?;
                let mut uses = Vec::new();
                if self.eat(&Tok::Use) {
                    self.expect(Tok::LParen)?;
                    while !self.at(&Tok::RParen) {
                        uses.push(self.expect_var()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                }
                let ret = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
                let body = self.body_block()?;
                ExprKind::Closure(Box::new(Closure { params, ret, uses, body: ClosureBody::Block(body), arrow: false }))
            }
            Tok::Match => self.match_expr()?,
            other => return Err(self.error_here(format!("expected an expression, found {}", other.describe()))),
        };
        Ok(Expr { kind, span: start.to(self.prev_span()) })
    }

    fn sub_expr(&mut self, code: &str, offset: u32) -> PResult<Expr> {
        if self.halted {
            return Err(PErr);
        }
        let (toks, diags) = lex(code, self.file, offset, Mode::Code);
        self.diags.extend(diags);
        // The code inside strings counts toward the file's tokens.
        self.tokens += toks.len() - 1;
        if self.tokens > self.limit {
            if self.limit == MAX_TOKENS {
                let span = Span::new(self.file, offset, offset);
                self.diags.push(too_many_tokens(span));
            }
            self.halted = true;
            self.pos = self.toks.len() - 1;
            return Err(PErr);
        }
        let mut p = Parser::new(toks, self.file, false);
        p.depth = self.depth;
        // Strings inside the expression count toward the file's tokens too.
        p.tokens = self.tokens;
        p.limit = self.limit;
        let e = p.expr();
        self.tokens = p.tokens;
        if p.halted {
            self.halted = true;
            self.pos = self.toks.len() - 1;
        }
        if e.is_ok() && !matches!(p.peek(), Tok::Eof) {
            let _ = p.error_here("unexpected tokens in string interpolation");
        }
        self.diags.append(&mut p.diags);
        e
    }

    fn array_literal(&mut self) -> PResult<ExprKind> {
        self.expect(Tok::LBracket)?;
        if self.eat(&Tok::RBracket) {
            return Ok(ExprKind::List(Vec::new()));
        }
        let first = self.spreadable()?;
        if !matches!(first.kind, ExprKind::Spread(_)) && self.eat(&Tok::FatArrow) {
            let mut pairs = vec![(first, self.expr()?)];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let k = self.expr()?;
                if !self.eat(&Tok::FatArrow) {
                    return Err(self.error_here("expected `=>`: all entries of a map literal need a key"));
                }
                pairs.push((k, self.expr()?));
            }
            self.expect(Tok::RBracket)?;
            Ok(ExprKind::Map(pairs))
        } else {
            let mut items = vec![first];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBracket) {
                    break;
                }
                items.push(self.spreadable()?);
                if self.at(&Tok::FatArrow) {
                    return Err(self.error_here("a list literal cannot contain `key => value` entries"));
                }
            }
            self.expect(Tok::RBracket)?;
            Ok(ExprKind::List(items))
        }
    }

    fn match_expr(&mut self) -> PResult<ExprKind> {
        self.expect(Tok::Match)?;
        let subject = self.paren_expr()?;
        let subject = match subject.kind {
            ExprKind::Bool(true) => None,
            _ => Some(Box::new(subject)),
        };
        self.expect(Tok::LBrace)?;
        let mut arms = Vec::new();
        while !self.at(&Tok::RBrace) {
            let start = self.span();
            let mut conds = Vec::new();
            if !self.eat(&Tok::Default) {
                loop {
                    conds.push(self.expr()?);
                    if !self.eat(&Tok::Comma) || self.at(&Tok::FatArrow) {
                        break;
                    }
                }
            }
            self.expect(Tok::FatArrow)?;
            let body = self.expr()?;
            arms.push(MatchArm { conds, body, span: start.to(self.prev_span()) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(ExprKind::Match { subject, arms: arms.into() })
    }
}
