use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{lex, Mode};
use crate::span::Span;
use crate::token::{StrPart, Tok, Token};

pub fn parse_module(src: &str, file: u32, is_template: bool) -> (Module, Vec<Diagnostic>) {
    let mode = if is_template { Mode::Html } else { Mode::Code };
    let (toks, mut diags) = lex(src, file, 0, mode);
    let mut p = Parser::new(toks, file, is_template);
    let stmts = p.block_until(&[]);
    if is_template && !p.halted && !p.html.at_top_level() {
        let span = p.span();
        let mut d = Diagnostic::error("the template ends inside a tag, comment, `<script>`/`<style>` or `<svg>`", span);
        d.add_note(
            "close everything the template opens: its output is `Html`, which can be joined with other `Html` values",
        );
        p.diags.push(d);
    }
    diags.append(&mut p.diags);
    let module = Module { file, is_template, stmts, template_refs: p.template_refs };
    (module, diags)
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
    /// Current nesting of expressions and blocks (see `enter`).
    depth: u32,
    /// Set when the nesting limit was hit: the rest of the file is skipped and
    /// no further errors are reported.
    halted: bool,
    /// Tokens that `expect_gt` split (`>>` into `>` `>`), with their original
    /// form, so a parse that backtracks can put them back.
    splits: Vec<(usize, Token)>,
    /// The loops and `try` blocks around the statement being parsed (in the
    /// current function), with the HTML state where each began: `break`,
    /// `continue`, `return` and `throw` must leave the HTML where the code
    /// they jump to expects it.
    exits: Vec<(Exit, crate::htmlctx::HtmlTracker)>,
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
            depth: 0,
            halted: false,
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

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.advance();
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

    fn expect(&mut self, tok: Tok) -> PResult<Span> {
        if self.at(&tok) {
            Ok(self.advance().span)
        } else {
            let found = self.peek().describe();
            Err(self.error_here(format!("expected {}, found {found}", tok.describe())))
        }
    }

    fn expect_ident(&mut self, what: &str) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let span = self.advance().span;
                Ok((name, span))
            }
            other => Err(self.error_here(format!("expected {what}, found {}", other.describe()))),
        }
    }

    fn expect_var(&mut self) -> PResult<(String, Span)> {
        match self.peek().clone() {
            Tok::Var(name) => {
                let span = self.advance().span;
                Ok((name, span))
            }
            other => Err(self.error_here(format!("expected a variable, found {}", other.describe()))),
        }
    }

    /// Statement terminator: `;`, or a `?>` close tag (left for the block loop).
    fn semi(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Semi => {
                self.advance();
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
            self.advance();
        }
        loop {
            match self.peek() {
                Tok::Eof | Tok::RBrace | Tok::CloseTag | Tok::InlineHtml(_) => return,
                Tok::Semi => {
                    self.advance();
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
                    self.advance();
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
        let stmts = self.block_stmts(ends);
        if let Some(entry) = entry {
            self.check_block_html(&entry);
        }
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
        };
        let mut d = Diagnostic::warning(msg, span);
        d.add_note(note);
        d.add_note("or allow inline code with `[security] csp` in phv.toml (not recommended)");
        d.code = Some(crate::diag::CSP_INLINE);
        self.diags.push(d);
    }

    fn check_block_html(&mut self, entry: &crate::htmlctx::HtmlTracker) {
        if !self.html.end_block(entry) && !self.halted {
            let span = self.span();
            let mut d = Diagnostic::error("the HTML in this block does not end where it started", span);
            d.add_note(
                "close every tag, attribute and `<script>`/`<style>` opened inside a block, loop or function before it ends",
            );
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
                self.advance();
                continue;
            }
            if ends.is_empty() && *tok == Tok::RBrace {
                let _ = self.error_here("unexpected `}`");
                self.advance();
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
            let entry = self.html.clone();
            let s = self.stmt()?;
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
        self.exits.push((kind, self.html.clone()));
        let r = body(self);
        self.exits.pop();
        r
    }

    /// Checks a `break`/`continue` (`loop_level` > 0) or a `return`/`throw`:
    /// the HTML must be where the code it jumps to began (the loop, the `try`
    /// block, or the start of the function), and where every `try` block it
    /// leaves began (their `finally` runs on the way out).
    fn check_exit(&mut self, loop_level: u32, throw: bool, span: Span) {
        let mut checked = Vec::new();
        let mut loops = 0;
        let mut target = None;
        for (kind, at) in self.exits.iter().rev() {
            match kind {
                Exit::Try if throw => {
                    target = Some(at.clone());
                    break;
                }
                Exit::Try => checked.push(at.clone()),
                Exit::Loop if loop_level > 0 => {
                    loops += 1;
                    if loops == loop_level {
                        target = Some(at.clone());
                        break;
                    }
                }
                Exit::Loop => {}
            }
        }
        if loop_level > 0 && target.is_none() {
            return; // Not inside that many loops: the type checker reports it.
        }
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
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::InlineHtml(text) => {
                self.advance();
                let scripts = self.html.feed(&text);
                for (at, what) in self.html.take_inline_code() {
                    let pos = start.start + at as u32;
                    let len = match &what {
                        crate::htmlctx::InlineCode::Handler(name) => name.len() as u32,
                        crate::htmlctx::InlineCode::ScriptUrl => 1,
                    };
                    self.inline_code_warning(Span { start: pos, end: pos + len, ..start }, what);
                }
                StmtKind::InlineHtml(text, scripts)
            }
            Tok::EchoOpen => {
                self.advance();
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
                self.advance();
                // `echo` between pieces of HTML lands in the same context as `<?= ?>`.
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
                self.advance();
                let cond = self.paren_expr()?;
                let body = self.exit_target(Exit::Loop, |p| {
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
                self.advance();
                let body = self.exit_target(Exit::Loop, |p| p.stmt_or_block())?;
                self.expect(Tok::While)?;
                let cond = self.paren_expr()?;
                self.semi()?;
                StmtKind::DoWhile { body, cond }
            }
            Tok::For => return self.for_stmt(),
            Tok::Foreach => return self.foreach_stmt(),
            Tok::Try => return self.try_stmt(),
            Tok::Return => {
                let span = self.advance().span;
                self.check_exit(0, false, span);
                let value =
                    if matches!(self.peek(), Tok::Semi | Tok::CloseTag | Tok::Eof) { None } else { Some(self.expr()?) };
                self.semi()?;
                StmtKind::Return(value)
            }
            Tok::Break | Tok::Continue => {
                let is_break = self.at(&Tok::Break);
                let span = self.advance().span;
                let mut level = 1;
                if let Tok::Int(n) = *self.peek() {
                    self.advance();
                    level = n.max(1) as u32;
                }
                self.check_exit(level, false, span);
                self.semi()?;
                if is_break {
                    StmtKind::Break(level)
                } else {
                    StmtKind::Continue(level)
                }
            }
            Tok::Throw => {
                let span = self.advance().span;
                self.check_exit(0, true, span);
                let e = self.expr()?;
                self.semi()?;
                StmtKind::Throw(e)
            }
            Tok::LBrace => StmtKind::Block(self.block()?),
            Tok::Semi => {
                self.advance();
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
                StmtKind::Function(self.function_decl(false, Visibility::Public, false)?)
            }
            Tok::Class | Tok::Interface | Tok::Enum | Tok::Abstract | Tok::Final => {
                StmtKind::Class(self.class_decl(false)?)
            }
            Tok::Const => StmtKind::Const(self.const_decl(false)?),
            Tok::Ident(ref name) if name == "type" && self.at_type_alias(0) => {
                StmtKind::TypeAlias(self.type_alias(false)?)
            }
            Tok::Export => {
                self.advance();
                match self.peek() {
                    Tok::Function => StmtKind::Function(self.function_decl(true, Visibility::Public, false)?),
                    Tok::Class | Tok::Interface | Tok::Enum | Tok::Abstract | Tok::Final => {
                        StmtKind::Class(self.class_decl(true)?)
                    }
                    Tok::Const => StmtKind::Const(self.const_decl(true)?),
                    Tok::Ident(name) if name == "type" && self.at_type_alias(0) => {
                        StmtKind::TypeAlias(self.type_alias(true)?)
                    }
                    _ => return Err(self.error_here("expected a declaration after `export`")),
                }
            }
            Tok::Import => StmtKind::Import(self.import_decl()?),
            Tok::Ident(ref name) if name == "params" && self.is_template && *self.peek_at(1) == Tok::LParen => {
                self.advance();
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
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
    }

    /// Assignment, increment/decrement, typed declaration or expression statement.
    fn simple_stmt(&mut self) -> PResult<StmtKind> {
        if let Some(kind) = self.try_typed_decl()? {
            return Ok(kind);
        }
        if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
            let inc = self.at(&Tok::PlusPlus);
            self.advance();
            let target = self.expr()?;
            return Ok(StmtKind::IncDec { target, inc });
        }
        let e = self.expr()?;
        if let Some(op) = assign_op(self.peek()) {
            self.advance();
            let value = self.expr()?;
            return Ok(StmtKind::Assign { target: e, op, value });
        }
        if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
            let inc = self.at(&Tok::PlusPlus);
            self.advance();
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
                let name_span = self.advance().span;
                self.advance();
                let value = self.expr()?;
                return Ok(Some(StmtKind::TypedDecl { ty, name, name_span, value }));
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
        let start = self.advance().span;
        let cond = self.paren_expr()?;
        let mut elifs = Vec::new();
        let mut els = None;
        let then;
        if self.eat(&Tok::Colon) {
            // Template-friendly syntax: if (...): ... elseif (...): ... else: ... endif;
            then = self.block_until(&[Tok::Elseif, Tok::Else, Tok::Endif]);
            loop {
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
                if self.eat(&Tok::Elseif) {
                    let c = self.paren_expr()?;
                    elifs.push((c, self.stmt_or_block()?));
                } else if self.at(&Tok::Else) && *self.peek_at(1) == Tok::If {
                    self.advance();
                    self.advance();
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
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.advance().span;
        self.expect(Tok::LParen)?;
        let mut init = Vec::new();
        while !self.at(&Tok::Semi) {
            let s = self.span();
            let kind = self.simple_stmt()?;
            init.push(Stmt { kind, span: s.to(self.prev_span()) });
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
            step.push(Stmt { kind, span: s.to(self.prev_span()) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let body = self.exit_target(Exit::Loop, |p| {
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
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
    }

    fn foreach_stmt(&mut self) -> PResult<Stmt> {
        let start = self.advance().span;
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
        let mut body = self.exit_target(Exit::Loop, |p| {
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
        let kind = StmtKind::Foreach { subject, key, value, body };
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
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
        *pattern =
            Some(Stmt { kind: StmtKind::Assign { target: Expr { kind, span }, op: AssignOp::Set, value }, span });
        Ok((name, span))
    }

    fn try_stmt(&mut self) -> PResult<Stmt> {
        let start = self.advance().span;
        let body = self.exit_target(Exit::Try, |p| p.block())?;
        let mut catches = Vec::new();
        while self.at(&Tok::Catch) {
            let cstart = self.advance().span;
            self.expect(Tok::LParen)?;
            let mut classes = vec![self.expect_ident("an exception class")?];
            while self.eat(&Tok::Pipe) {
                classes.push(self.expect_ident("an exception class")?);
            }
            let var = match self.peek().clone() {
                Tok::Var(name) => Some((name, self.advance().span)),
                _ => None,
            };
            self.expect(Tok::RParen)?;
            let body = self.block()?;
            catches.push(Catch { classes, var, body, span: cstart.to(self.prev_span()) });
        }
        let finally = if self.eat(&Tok::Finally) { Some(self.block()?) } else { None };
        if catches.is_empty() && finally.is_none() {
            return Err(self.error_here("expected `catch` or `finally` after `try` block"));
        }
        let kind = StmtKind::Try { body, catches, finally };
        Ok(Stmt { kind, span: start.to(self.prev_span()) })
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
                self.advance();
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
            self.advance();
        }
        if is_final && kind == ClassKind::Abstract {
            self.diags.push(
                Diagnostic::error("a class cannot be both `abstract` and `final`", start)
                    .note("an abstract class must be extended to be used"),
            );
        }
        match self.peek() {
            Tok::Interface | Tok::Enum if is_final => {
                let what = if self.at(&Tok::Interface) { "interfaces" } else { "enums" };
                self.diags.push(Diagnostic::error(format!("{what} cannot be `final`"), start));
            }
            _ => {}
        }
        match self.peek() {
            Tok::Interface => kind = ClassKind::Interface,
            Tok::Enum => kind = ClassKind::Enum,
            Tok::Class => {}
            other => {
                let found = other.describe();
                return Err(self.error_here(format!("expected `class`, `interface` or `enum`, found {found}")));
            }
        }
        self.advance();
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
            if let (Some(a), Tok::Case | Tok::RBrace) = (attrs.first(), self.peek()) {
                self.diags.push(Diagnostic::error("annotations cannot be written before an enum case", a.span));
            }
            if self.at(&Tok::Case) {
                let r: PResult<()> = (|| {
                    self.advance();
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
                self.advance();
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
            props,
            methods,
            consts,
            cases,
            backing,
            exported,
            span: start.to(self.prev_span()),
        })
    }

    /// `type Name =` or `type Name<`: `type` is only a keyword here.
    fn at_type_alias(&self, at: usize) -> bool {
        matches!(self.peek_at(at + 1), Tok::Ident(_)) && matches!(self.peek_at(at + 2), Tok::Eq | Tok::Lt)
    }

    fn type_alias(&mut self, exported: bool) -> PResult<TypeAliasDecl> {
        let start = self.span();
        self.advance();
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
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Question => {
                self.advance();
                TypeExprKind::Nullable(Box::new(self.ty_atom()?))
            }
            Tok::Null => {
                self.advance();
                TypeExprKind::Named("null".into(), Vec::new())
            }
            Tok::Fn => {
                self.advance();
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
                self.advance();
                let t = self.ty()?;
                self.expect(Tok::RParen)?;
                return Ok(t);
            }
            Tok::Ident(name) => {
                self.advance();
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
            self.advance();
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
                self.advance();
                let then = self.expr_bp(0)?;
                self.expect(Tok::Colon)?;
                let els = self.expr_bp(1)?;
                let span = lhs.span.to(els.span);
                lhs = Expr { kind: ExprKind::Ternary(Box::new(lhs), Box::new(then), Box::new(els)), span };
                continue;
            }
            if matches!(tok, Tok::Is | Tok::Instanceof) && min_bp <= BP_IS {
                self.advance();
                let ty = self.ty()?;
                let span = lhs.span.to(ty.span);
                lhs = Expr { kind: ExprKind::Is(Box::new(lhs), ty), span };
                continue;
            }
            if matches!(tok, Tok::DotDot | Tok::DotDotEq) && min_bp <= 18 {
                self.advance();
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
            self.advance();
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
                self.advance();
                un(UnOp::Not, self.expr_bp(BP_UNARY)?)
            }
            Tok::Tilde => {
                self.advance();
                un(UnOp::BitNot, self.expr_bp(BP_UNARY)?)
            }
            Tok::Minus => {
                self.advance();
                let operand = self.expr_bp(BP_UNARY)?;
                match operand.kind {
                    ExprKind::Int(v) => ExprKind::Int(v.wrapping_neg()),
                    ExprKind::Float(v) => ExprKind::Float(-v),
                    _ => un(UnOp::Neg, operand),
                }
            }
            Tok::Plus => {
                self.advance();
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
                        self.advance();
                        self.advance();
                        self.advance();
                        ExprKind::Cast(c, Box::new(self.expr_bp(BP_UNARY)?))
                    }
                    None => return self.postfix(),
                }
            }
            Tok::Throw => {
                self.advance();
                ExprKind::Throw(Box::new(self.expr_bp(0)?))
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
                    e = self.func_ref(Expr { kind: ExprKind::Call { callee: Box::new(e), args: Vec::new() }, span });
                }
                Tok::LParen => {
                    let args = self.args()?;
                    if let ExprKind::Name(n) = &e.kind {
                        if n == "render" || n == "layout" {
                            if let Some(Arg { name: None, value: Expr { kind: ExprKind::Str(path), span } }) =
                                args.first()
                            {
                                self.template_refs.push((normalize_template_path(path), *span));
                            }
                        }
                    }
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args }, span };
                }
                Tok::LBracket => {
                    self.advance();
                    let index = if self.at(&Tok::RBracket) { None } else { Some(Box::new(self.expr()?)) };
                    self.expect(Tok::RBracket)?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Index { base: Box::new(e), index }, span };
                }
                Tok::Arrow | Tok::NullsafeArrow => {
                    let nullsafe = self.at(&Tok::NullsafeArrow);
                    self.advance();
                    let (name, name_span) = self.member_name()?;
                    if self.at_func_ref() {
                        let span = e.span.to(name_span);
                        let kind =
                            ExprKind::MethodCall { obj: Box::new(e), name, name_span, args: Vec::new(), nullsafe };
                        e = self.func_ref(Expr { kind, span });
                    } else if self.at(&Tok::LParen) {
                        let args = self.args()?;
                        let span = e.span.to(self.prev_span());
                        e = Expr {
                            kind: ExprKind::MethodCall { obj: Box::new(e), name, name_span, args, nullsafe },
                            span,
                        };
                    } else {
                        let span = e.span.to(self.prev_span());
                        e = Expr { kind: ExprKind::Prop { obj: Box::new(e), name, name_span, nullsafe }, span };
                    }
                }
                Tok::DoubleColon => {
                    let ExprKind::Name(class) = &e.kind else {
                        return Err(self.error_here("`::` must follow a class name"));
                    };
                    let class = class.clone();
                    let class_span = e.span;
                    self.advance();
                    let (name, name_span) = self.member_name()?;
                    if !self.at(&Tok::LParen) {
                        let span = e.span.to(self.prev_span());
                        e = Expr { kind: ExprKind::ClassConst { class, class_span, name, name_span }, span };
                        continue;
                    }
                    if self.at_func_ref() {
                        let span = e.span.to(name_span);
                        let kind = ExprKind::StaticCall { class, class_span, name, name_span, args: Vec::new() };
                        e = self.func_ref(Expr { kind, span });
                        continue;
                    }
                    let args = self.args()?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::StaticCall { class, class_span, name, name_span, args }, span };
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
            Tok::Ident(name) => Ok((name, self.advance().span)),
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
            | Tok::Interface
            | Tok::Enum
            | Tok::Case
            | Tok::Const
            | Tok::Abstract
            | Tok::Final
            | Tok::Implements => {
                let span = self.advance().span;
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
                    let span = self.advance().span;
                    self.advance();
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
        Ok(args)
    }

    /// An expression, or `...expr` (an argument or list item to spread).
    fn spreadable(&mut self) -> PResult<Expr> {
        if !self.at(&Tok::Ellipsis) {
            return self.expr();
        }
        let start = self.advance().span;
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
            self.advance();
        }
        let span = call.span.to(self.prev_span());
        Expr { kind: ExprKind::FuncRef(Box::new(call)), span }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Var(name) => {
                self.advance();
                ExprKind::Var(name)
            }
            Tok::Int(v) => {
                self.advance();
                ExprKind::Int(v)
            }
            Tok::Float(v) => {
                self.advance();
                ExprKind::Float(v)
            }
            Tok::Str(s) => {
                self.advance();
                ExprKind::Str(s)
            }
            Tok::Interp(parts) => {
                self.advance();
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
                self.advance();
                ExprKind::Bool(true)
            }
            Tok::False => {
                self.advance();
                ExprKind::Bool(false)
            }
            Tok::Null => {
                self.advance();
                ExprKind::Null
            }
            Tok::Ident(name) => {
                self.advance();
                ExprKind::Name(name)
            }
            Tok::LParen => {
                self.advance();
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
                self.advance();
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
                self.advance();
                let (class, class_span) = self.expect_ident("a class name")?;
                let type_args = self.type_args()?;
                let args = if self.at(&Tok::LParen) { self.args()? } else { Vec::new() };
                ExprKind::New { class, class_span, type_args, args }
            }
            Tok::Fn => {
                self.advance();
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
                self.advance();
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
        let (toks, diags) = lex(code, self.file, offset, Mode::Code);
        self.diags.extend(diags);
        let mut p = Parser::new(toks, self.file, false);
        p.depth = self.depth;
        let e = p.expr();
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
        Ok(ExprKind::Match { subject, arms })
    }
}
