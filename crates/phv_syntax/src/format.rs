//! `phv fmt`: normalizes indentation (4 spaces per level) and whitespace of
//! `.phv` files, and wraps lines longer than the width (100 by default).
//! Lines inside multi-line strings, block comments and inline HTML, and lines
//! that mix code with `<?phv` / `?>` tags, are never changed, so formatting
//! cannot change what a program does.

use crate::lexer::{lex_with_comments, Mode};
use crate::span::Span;
use crate::token::{Tok, Token};

/// Lines longer than this many characters are wrapped (`phv fmt --width`, `[fmt] width`).
pub const DEFAULT_WIDTH: usize = 100;

/// Bracket levels and `if (...):` blocks open at once: what the parser allows
/// (`code is nested too deeply`). Each open level indents the lines after it,
/// so a file of unclosed brackets grew by its length squared (700 KB of
/// `$a = [` lines needed 20 GB; 21st assessment).
const MAX_DEPTH: usize = 500;

/// The largest formatted file: the largest source file that is read.
pub const MAX_FORMATTED: usize = 8 << 20;

/// Tokens looked at before formatting gives up: each pass over the lines
/// looks at every token once, and a pass breaks each over-long line once, so
/// a line that takes thousands of breaks in a large file is not worth it.
const MAX_WORK: u64 = 200_000_000;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Open,
    Close,
    /// `else:` / `elseif (...):` close one block and open the next.
    CloseOpen,
    None,
}

/// One line of the file. The lines are lexed once; breaking a line makes
/// lines of its pieces, which take its tokens with them.
struct Line<'a> {
    /// The line without its line ending (a piece of a broken line: trimmed).
    text: std::borrow::Cow<'a, str>,
    /// Its own line ending, kept where it is part of a string or of the output.
    ending: &'a str,
    /// The tokens that start on the line; their spans are relative to `text`.
    toks: Vec<Token>,
    /// The line starts inside a multi-line string, a comment or inline HTML:
    /// it is kept as written.
    inside: bool,
    /// The end of the line is inside a string or inline HTML: its trailing
    /// spaces and its line ending are part of the value or the output.
    tail_kept: bool,
    /// A string, comment or HTML that must stay as it is touches the line: it is never broken.
    touches: bool,
    /// The bytes of `text` that are its content (without the spaces around it).
    content: std::ops::Range<usize>,
    /// The characters of the content.
    chars: usize,
    /// Code mixed with `<?phv` / `?>` tags: kept as written.
    mixed: bool,
    /// Indentation levels, set by `indent`.
    level: usize,
    /// No place to break it was found (which does not depend on its indentation).
    stuck: bool,
}

impl<'a> Line<'a> {
    fn new(
        text: std::borrow::Cow<'a, str>,
        ending: &'a str,
        toks: Vec<Token>,
        (inside, tail_kept, touches): (bool, bool, bool),
    ) -> Line<'a> {
        let lead = text.len() - text.trim_start().len();
        let end = if tail_kept { text.len() } else { text.trim_end().len().max(lead) };
        let mixed =
            text[lead..end].starts_with("<?") || toks.iter().any(|t| matches!(t.tok, Tok::CloseTag | Tok::EchoOpen));
        let chars = text[lead..end].chars().count();
        Line {
            text,
            ending,
            toks,
            inside,
            tail_kept,
            touches,
            content: lead..end,
            chars,
            mixed,
            level: 0,
            stuck: false,
        }
    }

    fn content(&self) -> &str {
        &self.text[self.content.clone()]
    }

    /// One more space before a line starting with `*` (a line of a block comment).
    fn extra(&self) -> &'static str {
        if self.content().starts_with('*') {
            " "
        } else {
            ""
        }
    }

    /// Whether the line is written as indentation and content (not kept as it is, or blank).
    fn plain(&self) -> bool {
        !self.inside && !self.mixed && !self.content.is_empty()
    }
}

/// Formats a `.phv` source file with the default width. Returns an error if it does not lex.
pub fn format_source(src: &str) -> Result<String, String> {
    format_source_width(src, DEFAULT_WIDTH)
}

/// Formats a `.phv` source file, wrapping lines longer than `width` characters.
///
/// Returns an error, and no text, for a file that does not lex, one nested
/// (or left open) deeper than `MAX_DEPTH`, one whose formatted text would be
/// larger than `MAX_FORMATTED`, and one that takes more than `MAX_WORK`.
pub fn format_source_width(src: &str, width: usize) -> Result<String, String> {
    format_checked(src, width).map_err(|e| match e {
        FormatError::Lex(e) | FormatError::Refused(e) => e,
    })
}

/// Why a file was not formatted.
#[derive(Debug, PartialEq)]
pub enum FormatError {
    /// It does not lex: there is nothing to format yet.
    Lex(String),
    /// It is nested too deeply, would be too large, or takes too long.
    Refused(String),
}

/// `format_source_width`, telling a file that does not lex from one that is refused.
pub fn format_checked(src: &str, width: usize) -> Result<String, FormatError> {
    let mut lines = lines_of(src).map_err(FormatError::Lex)?;
    layout(&mut lines, width).map_err(FormatError::Refused)?;
    render(&lines, if src.contains("\r\n") { "\r\n" } else { "\n" }).map_err(FormatError::Refused)
}

fn layout(lines: &mut Vec<Line>, width: usize) -> Result<(), String> {
    let mut work = 0u64;
    // Lines are indented, then each over-long line is broken once and the
    // lines are indented again, until no line is broken: every pass that
    // changes something splits a line and lines are never joined, so this
    // ends. A pass looks at the lines, not at the text: 1,600 nested brackets
    // took a minute and 160,000 comments four while every pass wrote and
    // lexed the whole text again (21st assessment).
    loop {
        indent(lines, &mut work)?;
        if !wrap(lines, width, &mut work)? {
            return Ok(());
        }
    }
}

fn spend(work: &mut u64, n: usize) -> Result<(), String> {
    *work += n as u64;
    if *work > MAX_WORK {
        return Err("this file takes too long to format: break its longest lines by hand".into());
    }
    Ok(())
}

/// Tokens (without `Eof`) and the byte ranges whose lines must be kept
/// verbatim. The flag marks HTML, where the first line is protected too (not
/// only the inner lines).
#[allow(clippy::type_complexity)]
fn lex_protected(src: &str) -> Result<(Vec<Token>, Vec<(usize, usize, bool)>), String> {
    let (toks, comments, diags) = lex_with_comments(src, 0, 0, Mode::Code);
    if let Some(d) = diags.first() {
        return Err(d.message.clone());
    }
    let toks: Vec<Token> = toks.into_iter().filter(|t| t.tok != Tok::Eof).collect();
    let protected = protected_ranges(src, &toks, &comments);
    Ok((toks, protected))
}

/// The protected ranges, sorted by where they start and then by where they
/// end, without those that lie strictly inside another (which protect nothing
/// more): their ends are then in order too, so the ranges near a line are
/// found without looking at all of them.
fn protected_ranges(src: &str, toks: &[Token], comments: &[Span]) -> Vec<(usize, usize, bool)> {
    let mut protected: Vec<(usize, usize, bool)> = Vec::new();
    for t in toks {
        let multi_line = src[t.span.start as usize..t.span.end as usize].contains('\n');
        let html = matches!(t.tok, Tok::InlineHtml(_));
        if html || (multi_line && matches!(t.tok, Tok::Str(_) | Tok::Interp(_) | Tok::Sql(_))) {
            protected.push((t.span.start as usize, t.span.end as usize, html));
        }
    }
    for c in comments {
        protected.push((c.start as usize, c.end as usize, false));
    }
    let mut html_from: Option<usize> = None;
    for t in toks {
        match (&t.tok, html_from) {
            (Tok::CloseTag, _) => html_from = Some(t.span.end as usize),
            (_, Some(from)) => {
                protected.push((from, t.span.start as usize, true));
                html_from = None;
            }
            _ => {}
        }
    }
    if let Some(from) = html_from {
        protected.push((from, src.len(), true));
    }
    protected.sort();
    // The furthest end of the ranges that start before the current one.
    let (mut before, mut group_start, mut group_end) = (0, usize::MAX, 0);
    protected.retain(|&(s, e, _)| {
        if s != group_start {
            before = before.max(group_end);
            group_start = s;
            group_end = 0;
        }
        group_end = group_end.max(e);
        e >= before
    });
    protected
}

/// The lines of `src` with their tokens and what protects them.
fn lines_of(src: &str) -> Result<Vec<Line<'_>>, String> {
    let (toks, protected) = lex_protected(src)?;
    let mut toks = toks.into_iter().peekable();
    let mut lines = Vec::new();
    let mut line_start = 0;
    // The first protected range that ends at or after the line's start.
    let mut near = 0;
    for raw in src.split_inclusive('\n') {
        let line_end = line_start + raw.len();
        let text = raw.trim_end_matches(['\n', '\r']);
        let text_end = line_start + text.len();
        let mut line_toks = Vec::new();
        while let Some(mut t) = toks.next_if(|t| (t.span.start as usize) < line_end) {
            t.span.start -= line_start as u32;
            t.span.end -= line_start as u32;
            line_toks.push(t);
        }
        while near < protected.len() && protected[near].1 < line_start {
            near += 1;
        }
        let (mut inside, mut tail_kept, mut touches) = (false, false, false);
        for &(s, e, html) in &protected[near..] {
            if s > line_end {
                break;
            }
            inside |= (line_start > s || (html && line_start == s)) && line_start < e;
            tail_kept |= s <= text_end && (text_end < e || (html && text_end == e));
            touches |= s < line_end && e > line_start;
        }
        lines.push(Line::new(text.into(), &raw[text.len()..], line_toks, (inside, tail_kept, touches)));
        line_start = line_end;
    }
    Ok(lines)
}

/// The bracket roles of a line's tokens, including the template-style
/// `if (...): ... endif;` blocks.
fn roles(toks: &[Token]) -> Vec<Role> {
    let mut roles: Vec<Role> = toks
        .iter()
        .map(|t| match t.tok {
            Tok::LBrace | Tok::LParen | Tok::LBracket => Role::Open,
            Tok::RBrace | Tok::RParen | Tok::RBracket => Role::Close,
            Tok::Endif | Tok::Endforeach | Tok::Endwhile | Tok::Endfor => Role::Close,
            _ => Role::None,
        })
        .collect();
    // A `:` ending a line that starts with a control keyword opens an alt-syntax block.
    if let Some(j) = toks.len().checked_sub(1) {
        let last = if matches!(toks[j].tok, Tok::CloseTag) && j > 0 { j - 1 } else { j };
        if toks[last].tok == Tok::Colon {
            match toks[0].tok {
                Tok::If | Tok::Foreach | Tok::For | Tok::While => roles[last] = Role::Open,
                Tok::Else | Tok::Elseif => roles[0] = Role::CloseOpen,
                _ => {}
            }
        }
    }
    roles
}

/// Sets the indentation level of every line; never moves tokens between lines.
fn indent(lines: &mut [Line], work: &mut u64) -> Result<(), String> {
    // The unmatched openers: their line number, whether that line is a
    // continuation (indented one more), and the levels up to and including
    // this one. Each distinct line is one level, two for a continuation line.
    let mut stack: Vec<(usize, bool, usize)> = Vec::new();
    // Openers unmatched when the current `echo` statement (or `use` list of
    // traits, `trait_list`) began.
    let mut echo_at: Option<usize> = None;
    let mut trait_list = false;
    for (line_no, line) in lines.iter_mut().enumerate() {
        spend(work, line.toks.len() + 1)?;
        let roles = roles(&line.toks);
        // Indentation: closers at the start of the line belong to the outer level.
        let leading_close = roles.iter().take_while(|r| matches!(r, Role::Close | Role::CloseOpen)).count();
        let keep = stack.len().saturating_sub(leading_close);
        let mut level = keep.checked_sub(1).map_or(0, |k| stack[k].2);
        // A closer lines up with the continuation line that opened it.
        if let Some(&(opened, true, _)) = stack.get(keep) {
            if keep == 0 || stack[keep - 1].0 != opened {
                level += 1;
            }
        }
        // Lines starting with `->`, `.`, `&&`..., and later lines of `echo a,\n b;`.
        let first = line.toks.first().map(|t| &t.tok);
        let continues =
            matches!(first, Some(Tok::Arrow | Tok::NullsafeArrow | Tok::Dot | Tok::AndAnd | Tok::OrOr | Tok::QQ))
                || (echo_at == Some(stack.len()) && leading_close == 0);
        let push = |stack: &mut Vec<(usize, bool, usize)>| {
            let levels = match stack.last() {
                Some(&(l, _, n)) if l == line_no => n,
                Some(&(_, _, n)) => n + 1 + usize::from(continues),
                None => 1 + usize::from(continues),
            };
            stack.push((line_no, continues, levels));
        };
        // The most open at once, also inside the line: such a line is longer
        // than any width, and breaking it level by level would get here.
        let mut deepest = stack.len();
        for (k, t) in line.toks.iter().enumerate() {
            deepest = deepest.max(stack.len());
            match roles[k] {
                Role::Open => push(&mut stack),
                Role::Close => {
                    stack.pop();
                }
                Role::CloseOpen => {
                    stack.pop();
                    push(&mut stack);
                }
                Role::None => {}
            }
            match t.tok {
                Tok::Echo if echo_at.is_none() => {
                    echo_at = Some(stack.len());
                    trait_list = false;
                }
                // `use A, B` of traits in a class body: its later lines continue
                // it, as those of `echo`; a `{` of rules ends the list.
                Tok::Use if echo_at.is_none() && k == 0 => {
                    echo_at = Some(stack.len());
                    trait_list = true;
                }
                Tok::LBrace if trait_list && echo_at == Some(stack.len() - 1) => echo_at = None,
                Tok::Semi | Tok::CloseTag if echo_at == Some(stack.len()) => echo_at = None,
                _ if echo_at.is_some_and(|n| stack.len() < n) => echo_at = None,
                _ => {}
            }
        }
        if deepest.max(stack.len()) > MAX_DEPTH {
            return Err(format!(
                "code is nested too deeply to format (more than {MAX_DEPTH} brackets and blocks are open at once)"
            ));
        }
        line.level = level + usize::from(continues);
    }
    Ok(())
}

/// The formatted text: every line with its indentation, blank lines normalized.
fn render(lines: &[Line], newline: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut blank_run = 0;
    let mut last_kept = false;
    for line in lines {
        if out.len() > MAX_FORMATTED {
            break;
        }
        let text: &str = &line.text;
        last_kept = line.tail_kept || line.inside;
        if line.inside {
            out.push_str(text);
            out.push_str(if line.tail_kept { line.ending } else { newline });
            blank_run = 0;
            continue;
        }
        let newline_here = if line.tail_kept { line.ending } else { newline };
        if line.content.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push_str(newline);
            }
            continue;
        }
        blank_run = 0;
        if line.mixed {
            out.push_str(if line.tail_kept { text } else { text.trim_end() });
            out.push_str(newline_here);
            continue;
        }
        for _ in 0..line.level {
            out.push_str("    ");
        }
        out.push_str(line.extra());
        out.push_str(line.content());
        out.push_str(newline_here);
    }
    if out.len() > MAX_FORMATTED {
        return Err(format!("the formatted file would be larger than {} MB", MAX_FORMATTED >> 20));
    }
    // A file that ends in inline HTML (or inside a string) ends as it did:
    // its last characters are output.
    if !last_kept {
        while out.ends_with(&format!("{newline}{newline}")) {
            out.truncate(out.len() - newline.len());
        }
        if !out.ends_with(newline) {
            out.push_str(newline);
        }
    }
    Ok(out)
}

/// Breaks each line longer than `width` characters once (see `break_line`);
/// true if a line was broken (the caller indents the lines again).
fn wrap(lines: &mut Vec<Line>, width: usize, work: &mut u64) -> Result<bool, String> {
    // What each line to break becomes, and the lines found to have no place to break.
    let mut plans: Vec<(usize, Vec<Piece>)> = Vec::new();
    let mut stuck: Vec<usize> = Vec::new();
    // The last two tokens before the current line (the last one first).
    let mut before: [Option<&Tok>; 2] = [None, None];
    for (i, line) in lines.iter().enumerate() {
        let indent = line.level * 4 + line.extra().len();
        if line.plain() && !line.touches && !line.stuck && indent + line.chars > width {
            let mut text = " ".repeat(indent);
            text.push_str(line.content());
            // Where a token of the line is in `text`.
            let shift = indent as i64 - line.content.start as i64;
            match break_line(&text, &line.toks, shift, before, width, work)? {
                Some(pieces) => plans.push((i, pieces)),
                None => stuck.push(i),
            }
        }
        for t in &line.toks {
            before = [Some(&t.tok), before[0]];
        }
    }
    for i in stuck {
        lines[i].stuck = true;
    }
    if plans.is_empty() {
        return Ok(false);
    }
    let mut plans = plans.into_iter().peekable();
    let old = std::mem::take(lines);
    for (i, line) in old.into_iter().enumerate() {
        let Some((_, pieces)) = plans.next_if(|(at, _)| *at == i) else {
            lines.push(line);
            continue;
        };
        let file = line.toks.first().map_or(0, |t| t.span.file);
        let mut toks = line.toks.into_iter().peekable();
        for p in pieces {
            let mut piece_toks = Vec::new();
            while let Some(mut t) = toks.next_if(|t| (t.span.start as usize) < p.until) {
                t.span.start = t.span.start.saturating_sub(p.from as u32);
                t.span.end = t.span.end.saturating_sub(p.from as u32);
                piece_toks.push(t);
            }
            let mut text = p.text;
            if p.comma {
                let at = text.len() as u32;
                text.push(',');
                piece_toks.push(Token { tok: Tok::Comma, span: Span::new(file, at, at + 1) });
            }
            lines.push(Line::new(text.into(), "", piece_toks, (false, false, false)));
        }
    }
    Ok(true)
}

/// A piece of a broken line.
struct Piece {
    text: String,
    /// Where `text` starts in the line as its tokens count (see `break_line`).
    from: usize,
    /// The tokens of the line that start before this go into the piece (and the pieces before it).
    until: usize,
    /// A trailing comma is added.
    comma: bool,
}

/// Operators a long line may be broken before, loosest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OpClass {
    Coalesce,
    Or,
    And,
    Concat,
    Arrow,
}

fn op_class(t: &Tok) -> Option<OpClass> {
    Some(match t {
        Tok::QQ => OpClass::Coalesce,
        Tok::OrOr => OpClass::Or,
        Tok::AndAnd => OpClass::And,
        Tok::Dot => OpClass::Concat,
        Tok::Arrow | Tok::NullsafeArrow => OpClass::Arrow,
        _ => return None,
    })
}

/// Where the over-long line `text` is split, at its outermost level. Its
/// tokens are `lt`, each `shift` bytes further in `text` than its span says;
/// `before` are the two tokens before the line (the nearest first).
/// The line is split at:
/// - commas outside any bracket on the line (`echo a, b;`, several elements
///   per line): after the last one that keeps the line within `width`;
/// - the longest `(...)` / `[...]` that opens and closes on the line:
///   `head(` / one element per line / `)tail`, with a trailing comma where
///   the grammar takes one;
/// - before each `??`, `||`, `&&` or `.` of the loosest kind, when at least
///   as shallow as the bracket, and before `->` / `?->` (all but the first)
///   when there is no bracket.
///
/// None when the line has no such place or must be kept as written.
fn break_line(
    text: &str,
    lt: &[Token],
    shift: i64,
    before: [Option<&Tok>; 2],
    width: usize,
    work: &mut u64,
) -> Result<Option<Vec<Piece>>, String> {
    let Some(last) = lt.last() else { return Ok(None) };
    if text.trim_start().starts_with("<?") || lt.iter().any(|t| matches!(t.tok, Tok::CloseTag | Tok::EchoOpen)) {
        return Ok(None);
    }
    spend(work, lt.len())?;
    let piece = |from: usize, to: usize, comma: bool| {
        let s = &text[from..to];
        let start = from + (s.len() - s.trim_start().len());
        // A trailing comma is added by `wrap`, as a token of the piece.
        Piece {
            text: s.trim().to_string(),
            from: (start as i64 - shift) as usize,
            until: (to as i64 - shift) as usize,
            comma,
        }
    };
    // `if (...):` opens a block only while its `:` ends the line starting with `if`.
    if matches!(lt[0].tok, Tok::If | Tok::Elseif | Tok::Else | Tok::Foreach | Tok::For | Tok::While)
        && last.tok == Tok::Colon
    {
        return Ok(None);
    }
    // Bracket depth before each token, relative to the start of the line, and
    // the bracket pairs that open and close on it.
    let mut depth = Vec::with_capacity(lt.len());
    let mut d = 0i32;
    let mut open: Vec<usize> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (k, t) in lt.iter().enumerate() {
        depth.push(d);
        match t.tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => {
                d += 1;
                open.push(k);
            }
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                d -= 1;
                let closes = |o: usize| {
                    matches!(
                        (&lt[o].tok, &t.tok),
                        (Tok::LParen, Tok::RParen) | (Tok::LBracket, Tok::RBracket) | (Tok::LBrace, Tok::RBrace)
                    )
                };
                match open.pop() {
                    Some(o) if closes(o) => pairs.push((o, k)),
                    _ => open.clear(),
                }
            }
            _ => {}
        }
    }
    // Commas that separate something, not those of a type like `Map<string, int>`.
    let mut separator: Vec<bool> = lt.iter().map(|t| t.tok == Tok::Comma).collect();
    for k in 1..lt.len() {
        if lt[k].tok != Tok::Lt || !matches!(lt[k - 1].tok, Tok::Ident(_)) {
            continue;
        }
        let mut angles = 1;
        for j in k + 1..lt.len() {
            // A line of `a<a<a<...` is scanned to its end from each `<`.
            spend(work, 1)?;
            match lt[j].tok {
                Tok::Lt => angles += 1,
                Tok::Gt => angles -= 1,
                Tok::Shr => angles -= 2,
                Tok::Ident(_) | Tok::Comma | Tok::Question | Tok::Pipe | Tok::Null => continue,
                _ => break,
            }
            if angles <= 0 {
                separator[k..j].iter_mut().for_each(|s| *s = false);
                break;
            }
        }
    }
    let commas = |o: usize, c: usize| -> Vec<usize> {
        (o + 1..c).filter(|&k| separator[k] && depth[k] == depth[o] + 1).collect()
    };
    let start = |k: usize| (lt[k].span.start as i64 + shift) as usize;
    let end = |k: usize| (lt[k].span.end as i64 + shift) as usize;
    // The outermost, then longest, then first pair with at least two tokens
    // inside and no empty element (`f(a,,b)`) but a trailing comma. The pairs
    // are tried in that order, so that the commas of the nested ones are not
    // looked for once per level.
    pairs.sort_by_key(|&(o, c)| (depth[o], std::cmp::Reverse(end(c) - start(o)), o));
    let mut pair = None;
    for &(o, c) in &pairs {
        if !matches!(lt[o].tok, Tok::LParen | Tok::LBracket) || c - o <= 2 {
            continue;
        }
        spend(work, c - o)?;
        let mut bounds = vec![o];
        bounds.extend(commas(o, c));
        if bounds.windows(2).all(|w| w[1] - w[0] > 1) {
            pair = Some((o, c));
            break;
        }
    }

    let mut ops: Vec<(i32, OpClass, usize)> =
        lt.iter().enumerate().filter_map(|(k, t)| Some((depth[k], op_class(&t.tok)?, k))).collect();
    let loosest = ops.iter().map(|&(d, c, _)| (d, c)).min();
    if let Some((d, c)) = loosest {
        ops.retain(|&(od, oc, _)| od == d && oc == c);
        if c == OpClass::Arrow {
            ops.remove(0);
        }
        ops.retain(|&(_, _, k)| k > 0);
    }

    // Commas (but one ending the line) at least as shallow as any bracket or operator.
    let outer: Vec<usize> = (0..lt.len() - 1).filter(|&k| separator[k]).collect();
    if let Some(cd) = outer.iter().map(|&k| depth[k]).min() {
        let shallowest = pair.map(|(o, _)| depth[o]).into_iter().chain(loosest.map(|(d, _)| d)).min();
        if shallowest.is_none_or(|d| cd <= d) {
            let at_cd: Vec<usize> = outer.into_iter().filter(|&k| depth[k] == cd).collect();
            // The last comma within the width: where `width` characters end,
            // found once (not by counting the characters before each comma).
            let within = text.char_indices().nth(width).map_or(text.len(), |(at, _)| at);
            let k = *at_cd.iter().rfind(|k| end(**k) <= within).unwrap_or(&at_cd[0]);
            return Ok(Some(vec![piece(0, end(k), false), piece(end(k), text.len(), false)]));
        }
    }

    let use_ops = match (pair, loosest) {
        _ if ops.is_empty() => false,
        (None, _) => true,
        (Some((o, _)), Some((d, c))) => d < depth[o] || (d == depth[o] && c != OpClass::Arrow),
        (Some(_), None) => false,
    };
    let mut pieces = Vec::new();
    if use_ops {
        let mut from = 0;
        for &(_, _, k) in &ops {
            pieces.push(piece(from, start(k), false));
            from = start(k);
        }
        pieces.push(piece(from, text.len(), false));
    } else {
        let Some((o, c)) = pair else { return Ok(None) };
        pieces.push(piece(0, end(o), false));
        let mut from = end(o);
        for k in commas(o, c) {
            pieces.push(piece(from, end(k), false));
            from = end(k);
        }
        if !text[from..start(c)].trim().is_empty() {
            pieces.push(piece(from, start(c), trailing_comma_ok(lt, o, before)));
        }
        pieces.push(piece(start(c), text.len(), false));
    }
    Ok(Some(pieces))
}

/// Whether the bracket `toks[o]` takes a trailing comma: call arguments,
/// parameter lists, closure `use` lists and list/map literals do; grouping,
/// conditions, indexing and anything unsure do not.
fn trailing_comma_ok(toks: &[Token], o: usize, before: [Option<&Tok>; 2]) -> bool {
    // The token `n` before the bracket: on the line, or among those before it.
    let prev = |n: usize| match o.checked_sub(n) {
        Some(i) => Some(&toks[i].tok),
        None => before[n - o - 1],
    };
    // `$x->default(` / `$x->list[`: a member name may be a keyword.
    let member = matches!(prev(2), Some(Tok::Arrow | Tok::NullsafeArrow | Tok::DoubleColon));
    match toks[o].tok {
        // `yield from (...)`: `from` after `yield` is a keyword, not a call.
        Tok::LParen if matches!(prev(1), Some(Tok::Ident(n)) if n == "from") && matches!(prev(2), Some(Tok::Yield)) => {
            false
        }
        Tok::LParen => {
            member
                || matches!(
                    prev(1),
                    Some(Tok::Ident(_) | Tok::Var(_) | Tok::RBracket | Tok::Function | Tok::Fn | Tok::Use)
                )
        }
        Tok::LBracket => {
            !member
                && !matches!(
                    prev(1),
                    Some(
                        Tok::Var(_)
                            | Tok::Ident(_)
                            | Tok::Int(_)
                            | Tok::Float(_)
                            | Tok::Str(_)
                            | Tok::Interp(_)
                            | Tok::Sql(_)
                            | Tok::True
                            | Tok::False
                            | Tok::Null
                            | Tok::Static
                            | Tok::RParen
                            | Tok::RBracket
                            | Tok::RBrace
                    )
                )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_code() {
        let src = "<?phv\nfunction f(int $x): int {\nif ($x > 0) {\n  return $x;   \n}\n\n\n\n        return 0;\n}\n$s = \"multi\n   line\";\n$y = $obj\n->a()\n->b();\n";
        let want = "<?phv\nfunction f(int $x): int {\n    if ($x > 0) {\n        return $x;\n    }\n\n    return 0;\n}\n$s = \"multi\n   line\";\n$y = $obj\n    ->a()\n    ->b();\n";
        assert_eq!(format_source(src).unwrap(), want);
        assert_eq!(format_source(want).unwrap(), want);
    }

    /// What strings hold and what inline HTML outputs stays as it is: spaces
    /// before a newline inside a string, each line ending inside a string,
    /// and the end of a file that ends in HTML.
    #[test]
    fn keeps_string_contents_and_trailing_html() {
        let src = "<?phv\n$s = \"abc   \nxyz\";\n";
        assert_eq!(format_source(src).unwrap(), src);
        let src = "<?phv\r\n$s = \"a\nb\";\r\necho $s;\r\n";
        assert_eq!(format_source(src).unwrap(), src);
        for src in ["<?phv echo 1; ?>tail", "<?phv echo 1; ?>tail  ", "<?phv echo 1; ?>\n\n\n"] {
            assert_eq!(format_source(src).unwrap(), src);
        }
        let src = "<?phv echo 1; ?>text  \n<?phv echo 2;\n";
        assert_eq!(format_source(src).unwrap(), src);
    }

    #[test]
    fn one_level_per_line_and_alt_syntax() {
        let src = "$xs->sortBy(function ($a, $b): int {\nreturn $a <=> $b;\n});\nif ($ok):\necho 1;\nelse:\necho 2;\nendif;\n?string $x = null;\n";
        let want = "$xs->sortBy(function ($a, $b): int {\n    return $a <=> $b;\n});\nif ($ok):\n    echo 1;\nelse:\n    echo 2;\nendif;\n?string $x = null;\n";
        assert_eq!(format_source(src).unwrap(), want);
    }

    /// A trait declaration is a block like a class; a `use` list of traits
    /// continues on the lines it is wrapped on, and its `{ ... }` of rules is a
    /// block.
    #[test]
    fn formats_traits_and_use_lists() {
        let src = "<?phv\ntrait Hello<T> {\npublic function hi(): string {\nreturn \"hi\";\n}\n  abstract protected function name(): string;\n}\nclass Talker {\nuse Hello<int>, Hey {\nHello::hi insteadof Hey;\n      Hey::hi as protected hey;\n}\nuse One,\nTwo;\npublic function f(): void {\n}\n}\n";
        let want = "<?phv\ntrait Hello<T> {\n    public function hi(): string {\n        return \"hi\";\n    }\n    abstract protected function name(): string;\n}\nclass Talker {\n    use Hello<int>, Hey {\n        Hello::hi insteadof Hey;\n        Hey::hi as protected hey;\n    }\n    use One,\n        Two;\n    public function f(): void {\n    }\n}\n";
        assert_eq!(format_source(src).unwrap(), want);
        assert_eq!(format_source(want).unwrap(), want);
        // A long list is wrapped after a comma, and its next line indented.
        wraps(
            40,
            "class C {\n    use FirstTrait, SecondTrait, ThirdTrait;\n}\n",
            "class C {\n    use FirstTrait, SecondTrait,\n        ThirdTrait;\n}\n",
        );
        // A closure's `use` is not a list of traits.
        let src = "$f = function () use ($x): int {\nreturn $x;\n};\n$g = 1;\n";
        assert_eq!(format_source(src).unwrap(), "$f = function () use ($x): int {\n    return $x;\n};\n$g = 1;\n");
    }

    #[test]
    fn keeps_html_comments_and_line_endings() {
        let src = "<?phv $x = 1; ?>\n  <p>\n    keep\n  </p>\n<?phv\n/*\n   block\n */\n  echo $x;\n";
        let out = format_source(src).unwrap();
        assert!(out.contains("  <p>\n    keep\n  </p>"));
        assert!(out.contains("/*\n   block\n */"));
        assert!(out.contains("\necho $x;\n"));
        let crlf = format_source("<?phv\r\nif (true) {\r\necho 1;\r\n}\r\n").unwrap();
        assert_eq!(crlf, "<?phv\r\nif (true) {\r\n    echo 1;\r\n}\r\n");
    }

    /// Formats `src` at `width`, expects `want`, and checks that `want` stays as it is.
    fn wraps(width: usize, src: &str, want: &str) {
        assert_eq!(format_source_width(src, width).unwrap(), want);
        assert_eq!(format_source_width(want, width).unwrap(), want, "not idempotent");
    }

    #[test]
    fn wraps_call_arguments() {
        wraps(
            30,
            "$total = add($first, $second, $third);\n",
            "$total = add(\n    $first,\n    $second,\n    $third,\n);\n",
        );
        // A trailing comma is kept, not doubled; named arguments and `new` are calls too.
        wraps(30, "$p = new Point(x: $left, y: $top,);\n", "$p = new Point(\n    x: $left,\n    y: $top,\n);\n");
        wraps(30, "$this->default($aaaa, $bbbb, $c);\n", "$this->default(\n    $aaaa,\n    $bbbb,\n    $c,\n);\n");
    }

    #[test]
    fn wraps_literals_and_parameters() {
        wraps(20, "$xs = [1, 2, 3, 4, 5, 6];\n", "$xs = [\n    1,\n    2,\n    3,\n    4,\n    5,\n    6,\n];\n");
        wraps(25, "$m = [\"a\" => 1, \"b\" => $two];\n", "$m = [\n    \"a\" => 1,\n    \"b\" => $two,\n];\n");
        wraps(
            40,
            "function area(int $width, int $height): int {\nreturn $width * $height;\n}\n",
            "function area(\n    int $width,\n    int $height,\n): int {\n    return $width * $height;\n}\n",
        );
        wraps(
            40,
            "$f = function (int $a) use ($first, $second) {\nreturn $a;\n};\n",
            "$f = function (int $a) use (\n    $first,\n    $second,\n) {\n    return $a;\n};\n",
        );
    }

    #[test]
    fn wraps_nested_elements() {
        wraps(
            30,
            "<?phv\nif ($ok) {\nsave($id, [\"name\" => $name, \"mail\" => $mail]);\n}\n",
            "<?phv\nif ($ok) {\n    save(\n        $id,\n        [\n            \"name\" => $name,\n            \"mail\" => $mail,\n        ],\n    );\n}\n",
        );
    }

    #[test]
    fn wraps_before_operators() {
        // No bracket: the loosest operator at the outermost level.
        wraps(30, "$s = $greeting . $name . $suffix;\n", "$s = $greeting\n    . $name\n    . $suffix;\n");
        wraps(25, "$ok = $aaaa && $bbbb || $cccc;\n", "$ok = $aaaa && $bbbb\n    || $cccc;\n");
        wraps(20, "$v = $this->first->second;\n", "$v = $this->first\n    ->second;\n");
        // An operator as shallow as the bracket wins, except `->`.
        wraps(30, "$s = implode($sep, $xs) . $tail;\n", "$s = implode($sep, $xs)\n    . $tail;\n");
        wraps(
            30,
            "$rows = $db->query($sql, $args)->all();\n",
            "$rows = $db->query(\n    $sql,\n    $args,\n)->all();\n",
        );
        // A condition is a bracket; what is still too long is split again.
        wraps(
            20,
            "if ($first && $second) {\necho 1;\n}\n",
            "if (\n    $first\n        && $second\n) {\n    echo 1;\n}\n",
        );
    }

    #[test]
    fn no_trailing_comma_where_the_grammar_has_none() {
        wraps(20, "$x = ($aaaa + $bbbb) * 2;\n", "$x = (\n    $aaaa + $bbbb\n) * 2;\n");
        wraps(20, "$x = $map[$aaaa + $bbbb];\n", "$x = $map[\n    $aaaa + $bbbb\n];\n");
        wraps(20, "while ($aaaa < $bbbb) {\n}\n", "while (\n    $aaaa < $bbbb\n) {\n}\n");
        // Nothing to split: the line stays long.
        wraps(10, "$averyveryverylongname = 1;\n", "$averyveryverylongname = 1;\n");
        wraps(10, "f($averyveryverylongname);\n", "f($averyveryverylongname);\n");
    }

    /// The syntax tree of `src`, without positions.
    fn tree(src: &str) -> String {
        let (module, diags) = crate::parser::parse_module(src, 0, false);
        assert!(diags.is_empty(), "{src}: {:?}", diags.iter().map(|d| &d.message).collect::<Vec<_>>());
        let mut t = format!("{module:?}");
        // `Span { start: 1, end: 2 }` and the like.
        while let Some(at) = t.find("Span {") {
            let end = at + t[at..].find('}').unwrap() + 1;
            t.replace_range(at..end, "Span");
        }
        t
    }

    /// `yield from (...)` is a delegation over a parenthesized value, not a
    /// call of `from`: wrapping it adds no trailing comma (it gave
    /// `yield from ([...],);`, which does not parse), and the program it
    /// gives parses to the same tree.
    #[test]
    fn yield_from_takes_no_trailing_comma() {
        let src = "<?phv\nfunction g(): Generator<int> {\nyield from ([300000, 400000]);\nyield from([500000, 600000]);\nyield f(100000, 200000);\n}\n";
        let once = format_source_width(src, 12).unwrap();
        assert_eq!(format_source_width(&once, 12).unwrap(), once, "not idempotent");
        assert!(once.contains("yield from (\n        [\n"), "{once}");
        assert!(!once.contains("],\n    );"), "{once}");
        // A call is still a call.
        assert!(once.contains("yield f(\n        100000,\n        200000,\n    );"), "{once}");
        assert_eq!(tree(&once), tree(src));
    }

    #[test]
    fn width_boundary() {
        let line = |n: usize| format!("$x = f($a, \"{}\");\n", "a".repeat(n - 15));
        assert_eq!(line(100).trim_end().chars().count(), 100);
        assert_eq!(format_source(&line(100)).unwrap(), line(100));
        let want = format!("$x = f(\n    $a,\n    \"{}\",\n);\n", "a".repeat(86));
        assert_eq!(format_source(&line(101)).unwrap(), want);
        // Characters count, not bytes, and so does the indentation.
        let wide = format!("$x = f($a, \"{}\");\n", "é".repeat(85));
        assert_eq!(format_source(&wide).unwrap(), wide);
        let indented = format!("if ($c) {{\n    {}}}\n", line(96));
        assert_eq!(format_source(&indented).unwrap(), indented);
        let indented = format!("if ($c) {{\n    {}}}\n", line(97));
        assert_ne!(format_source(&indented).unwrap(), indented);
    }

    #[test]
    fn keeps_protected_lines() {
        for src in [
            "f($first, $second, $third); // a comment\n",
            "f($first, \"two\nlines\", $third);\n",
            "<?phv f($first, $second, $third); ?>\n",
            "<?phv\n?><p><?= f($first, $second, $third) ?></p>\n",
            "if ($first && $second && $third):\necho 1;\nendif;\n",
        ] {
            let once = format_source_width(src, 20).unwrap();
            let line = src.lines().find(|l| l.contains("$first")).unwrap();
            assert!(once.contains(line), "{src:?} became {once:?}");
            assert_eq!(format_source_width(&once, 20).unwrap(), once);
        }
    }

    #[test]
    fn wraps_echo_arguments_to_the_width() {
        // Commas outside brackets: as many as fit, then a continuation line.
        wraps(
            30,
            "echo $first, \" \", $second, \" \", $third, \"\\n\";\n",
            "echo $first, \" \", $second,\n    \" \", $third, \"\\n\";\n",
        );
        // Operators inside one argument are not split across arguments.
        wraps(25, "echo $a ?? \"none\", \" \", $b ?? \"-\";\n", "echo $a ?? \"none\", \" \",\n    $b ?? \"-\";\n");
        // Commas of a generic type are not separators.
        wraps(
            30,
            "function f(Map<string, int> $m, int $n): void {\n}\n",
            "function f(\n    Map<string, int> $m,\n    int $n,\n): void {\n}\n",
        );
    }

    #[test]
    fn brackets_opened_on_continuation_lines() {
        let src = "return $xs\n->filter(fn($x) => $x > 0)\n->map(function ($x) {\nreturn $x * 2;\n});\necho $a,\nf(\n$b,\n), $c;\n";
        let want = "return $xs\n    ->filter(fn($x) => $x > 0)\n    ->map(function ($x) {\n        return $x * 2;\n    });\necho $a,\n    f(\n        $b,\n    ), $c;\n";
        assert_eq!(format_source(src).unwrap(), want);
        assert_eq!(format_source(want).unwrap(), want);
        wraps(
            32,
            "$ys = $xs\n->sortBy(fn($a, $b) => $a <=> $b);\n",
            "$ys = $xs\n    ->sortBy(\n        fn($a, $b) => $a <=> $b,\n    );\n",
        );
    }

    #[test]
    fn wraps_with_crlf() {
        wraps(15, "<?phv\r\nf($first, $second);\r\n", "<?phv\r\nf(\r\n    $first,\r\n    $second,\r\n);\r\n");
    }

    /// Runs `f` and tells how long it took. The limits are far above what
    /// the formatter takes (milliseconds), for a slow or busy machine.
    fn timed<T>(f: impl FnOnce() -> T) -> (T, std::time::Duration) {
        let start = std::time::Instant::now();
        let out = f();
        (out, start.elapsed())
    }

    /// 21st assessment: every bracket left open indented the lines after it
    /// (700 KB of `$a = [` lines needed 20 GB), and 1,600 nested brackets on
    /// one line took a minute and became 10 MB. Past the parser's depth the
    /// file is refused, whether the brackets are closed or not.
    #[test]
    fn deep_nesting_is_refused() {
        let nested = |n: usize| format!("<?phv\n$b = {}1{};\n", "[".repeat(n), "]".repeat(n));
        let open = |n: usize| format!("<?phv\n{}", "$a = [\n".repeat(n));
        for src in [nested(1600), nested(MAX_DEPTH + 2), open(MAX_DEPTH + 1), open(100_000)] {
            let (out, took) = timed(|| format_checked(&src, DEFAULT_WIDTH));
            let Err(FormatError::Refused(e)) = out else { panic!("formatted {} bytes", src.len()) };
            assert!(e.contains("nested too deeply"), "{e}");
            assert!(took.as_secs() < 30, "took {took:?}");
        }
        // What the parser takes is formatted, each level on its line.
        let (out, took) = timed(|| format_source(&nested(240)).unwrap());
        assert!(out.lines().count() > 2 * 230, "{} lines", out.lines().count());
        assert_eq!(format_source(&out).unwrap(), out);
        assert!(took.as_secs() < 30, "took {took:?}");
        let out = format_source(&open(MAX_DEPTH)).unwrap();
        assert!(out.ends_with(&format!("{}$a = [\n", "    ".repeat(MAX_DEPTH - 1))));
    }

    /// A program the checker accepts (100 statements of 240 nested calls,
    /// 71 KB) became a 22 MB file that could not be read again.
    #[test]
    fn output_larger_than_a_source_file_is_refused() {
        let stmt = format!("$b = {}1{};\n", "f(".repeat(240), ")".repeat(240));
        let (out, took) = timed(|| format_checked(&format!("<?phv\n{}", stmt.repeat(100)), DEFAULT_WIDTH));
        assert_eq!(out, Err(FormatError::Refused("the formatted file would be larger than 8 MB".into())));
        assert!(took.as_secs() < 60, "took {took:?}");
        let out = format_source(&format!("<?phv\n{}", stmt.repeat(20))).unwrap();
        assert!(out.len() > 4 << 20 && out.len() < MAX_FORMATTED);
        // A file that does not lex is not a refusal: there is nothing to format yet.
        assert!(matches!(format_checked("<?phv\n$s = \"open", DEFAULT_WIDTH), Err(FormatError::Lex(_))));
    }

    /// Each line looked at every comment, string and piece of HTML of the
    /// file (160,000 comments took four minutes), each pass wrote and lexed
    /// the whole text again, and a list was broken one line per pass (one
    /// `match` arm of 40,000 values took four minutes).
    #[test]
    fn many_comments_and_long_lists_are_formatted_at_once() {
        let comments = format!("<?phv\n{}echo 1;\n", "// c\n/* d */\n/** e */\n".repeat(60_000));
        let (out, took) = timed(|| format_source(&comments).unwrap());
        assert_eq!(out, comments);
        assert!(took.as_secs() < 30, "took {took:?}");

        let values: Vec<String> = (0..4_000).map(|i| i.to_string()).collect();
        let list = format!("<?phv\n$x = match ($s) {{\n{} => 1, default => 0 }};\n", values.join(", "));
        let (out, took) = timed(|| format_source(&list).unwrap());
        assert!(out.lines().all(|l| l.chars().count() <= DEFAULT_WIDTH), "{}", &out[..300]);
        assert_eq!(format_source(&out).unwrap(), out);
        assert!(took.as_secs() < 30, "took {took:?}");

        // A line that would take more looks than `MAX_WORK` is given up (or
        // formatted), not worked on for hours.
        let angles = format!("<?phv\n$x = {}1;\n", "a<".repeat(200_000));
        let (out, took) = timed(|| format_checked(&angles, DEFAULT_WIDTH));
        assert!(matches!(out, Ok(_) | Err(FormatError::Refused(_))), "{out:?}");
        assert!(took.as_secs() < 120, "took {took:?}");
    }
}
