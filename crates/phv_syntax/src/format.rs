//! `phv fmt`: normalizes indentation (4 spaces per level) and whitespace of
//! `.phv` files, and wraps lines longer than the width (100 by default).
//! Lines inside multi-line strings, block comments and inline HTML, and lines
//! that mix code with `<?phv` / `?>` tags, are never changed, so formatting
//! cannot change what a program does.

use crate::lexer::{lex_with_comments, Mode};
use crate::span::{line_index, Span};
use crate::token::{Tok, Token};

/// Lines longer than this many characters are wrapped (`phv fmt --width`, `[fmt] width`).
pub const DEFAULT_WIDTH: usize = 100;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Open,
    Close,
    /// `else:` / `elseif (...):` close one block and open the next.
    CloseOpen,
    None,
}

fn line_starts(src: &str) -> Vec<u32> {
    let mut v = vec![0];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            v.push(i as u32 + 1);
        }
    }
    v
}

/// Assigns bracket roles to tokens, including the template-style
/// `if (...): ... endif;` blocks.
fn roles(toks: &[Token], line_of: &dyn Fn(u32) -> usize) -> Vec<Role> {
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
    let mut i = 0;
    while i < toks.len() {
        let line = line_of(toks[i].span.start);
        let mut j = i;
        while j + 1 < toks.len() && line_of(toks[j + 1].span.start) == line {
            j += 1;
        }
        let last = if matches!(toks[j].tok, Tok::CloseTag) && j > i { j - 1 } else { j };
        let first = &toks[i].tok;
        if toks[last].tok == Tok::Colon {
            match first {
                Tok::If | Tok::Foreach | Tok::For | Tok::While => roles[last] = Role::Open,
                Tok::Else | Tok::Elseif => {
                    roles[i] = Role::CloseOpen;
                }
                _ => {}
            }
        }
        i = j + 1;
    }
    roles
}

/// Formats a `.phv` source file with the default width. Returns an error if it does not lex.
pub fn format_source(src: &str) -> Result<String, String> {
    format_source_width(src, DEFAULT_WIDTH)
}

/// Formats a `.phv` source file, wrapping lines longer than `width` characters.
pub fn format_source_width(src: &str, width: usize) -> Result<String, String> {
    let mut out = reindent(src)?;
    // Every pass that changes something splits a line, and lines are never
    // joined, so this ends; the bound is only a safety net.
    for _ in 0..=src.len() {
        let wrapped = wrap(&out, width)?;
        if wrapped == out {
            break;
        }
        out = reindent(&wrapped)?;
    }
    Ok(out)
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
    protected
}

/// Re-indents every line and normalizes blank lines; never moves tokens between lines.
fn reindent(src: &str) -> Result<String, String> {
    let (toks, protected) = lex_protected(src)?;
    let starts = line_starts(src);
    let line_of = |off: u32| line_index(&starts, off);
    let roles = roles(&toks, &line_of);

    let newline = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(src.len());
    // The unmatched openers: their line number, and whether that line is a
    // continuation (indented one more). Each distinct line is one level, two
    // for a continuation line.
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let levels = |s: &[(usize, bool)]| {
        let mut n = 0;
        let mut prev = usize::MAX;
        for &(l, continued) in s {
            if l != prev {
                n += 1 + usize::from(continued);
                prev = l;
            }
        }
        n
    };
    let mut ti = 0;
    let mut blank_run = 0;
    // Openers unmatched when the current `echo` statement began.
    let mut echo_at: Option<usize> = None;
    for (line_no, raw) in src.split_inclusive('\n').enumerate() {
        let line_start = starts[line_no] as usize;
        let line_end = line_start + raw.len();
        let first_tok = ti;
        while ti < toks.len() && (toks[ti].span.start as usize) < line_end {
            ti += 1;
        }
        let line_toks = first_tok..ti;
        // Indentation: closers at the start of the line belong to the outer level.
        let mut leading_close = 0;
        for k in line_toks.clone() {
            match roles[k] {
                Role::Close | Role::CloseOpen => leading_close += 1,
                _ => break,
            }
        }
        let keep = stack.len().saturating_sub(leading_close);
        let mut level = levels(&stack[..keep]);
        // A closer lines up with the continuation line that opened it.
        if let Some(&(opened, true)) = stack.get(keep) {
            if keep == 0 || stack[keep - 1].0 != opened {
                level += 1;
            }
        }
        // Lines starting with `->`, `.`, `&&`..., and later lines of `echo a,\n b;`.
        let first = line_toks.clone().next().map(|k| &toks[k].tok);
        let continues =
            matches!(first, Some(Tok::Arrow | Tok::NullsafeArrow | Tok::Dot | Tok::AndAnd | Tok::OrOr | Tok::QQ))
                || (echo_at == Some(stack.len()) && leading_close == 0);
        for k in line_toks.clone() {
            match roles[k] {
                Role::Open => stack.push((line_no, continues)),
                Role::Close => {
                    stack.pop();
                }
                Role::CloseOpen => {
                    stack.pop();
                    stack.push((line_no, continues));
                }
                Role::None => {}
            }
            match toks[k].tok {
                Tok::Echo if echo_at.is_none() => echo_at = Some(stack.len()),
                Tok::Semi | Tok::CloseTag if echo_at == Some(stack.len()) => echo_at = None,
                _ if echo_at.is_some_and(|n| stack.len() < n) => echo_at = None,
                _ => {}
            }
        }

        let inside =
            protected.iter().any(|(s, e, html)| (line_start > *s || (*html && line_start == *s)) && line_start < *e);
        let content = raw.trim_end_matches(['\n', '\r']).trim();
        let mixed = content.starts_with("<?")
            || line_toks.clone().any(|k| matches!(toks[k].tok, Tok::CloseTag | Tok::EchoOpen));
        if inside {
            out.push_str(raw.trim_end_matches(['\n', '\r']));
            out.push_str(newline);
            blank_run = 0;
            continue;
        }
        if content.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push_str(newline);
            }
            continue;
        }
        blank_run = 0;
        if mixed {
            out.push_str(raw.trim_end_matches(['\n', '\r']).trim_end());
            out.push_str(newline);
            continue;
        }
        if continues {
            level += 1;
        }
        let extra = if content.starts_with('*') { " " } else { "" };
        out.push_str(&"    ".repeat(level));
        out.push_str(extra);
        out.push_str(content);
        out.push_str(newline);
    }
    while out.ends_with(&format!("{newline}{newline}")) {
        out.truncate(out.len() - newline.len());
    }
    if !out.ends_with(newline) {
        out.push_str(newline);
    }
    Ok(out)
}

/// Splits each line longer than `width` characters once (see `break_line`);
/// the caller re-indents the result.
fn wrap(src: &str, width: usize) -> Result<String, String> {
    let (toks, protected) = lex_protected(src)?;
    let starts = line_starts(src);
    let newline = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(src.len() + 64);
    let mut ti = 0;
    for (line_no, raw) in src.split_inclusive('\n').enumerate() {
        let line_start = starts[line_no] as usize;
        let line_end = line_start + raw.len();
        let first_tok = ti;
        while ti < toks.len() && (toks[ti].span.start as usize) < line_end {
            ti += 1;
        }
        let text = raw.trim_end_matches(['\n', '\r']);
        let touches_protected = protected.iter().any(|&(s, e, _)| s < line_end && e > line_start);
        let pieces = if text.chars().count() > width && !touches_protected {
            break_line(src, &toks, first_tok..ti, line_start..line_start + text.len(), width)
        } else {
            None
        };
        match pieces {
            Some(pieces) => {
                for p in pieces {
                    out.push_str(&p);
                    out.push_str(newline);
                }
            }
            None => out.push_str(raw),
        }
    }
    Ok(out)
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

/// Where the over-long line made of `toks[line]` (bytes `bytes` of `src`) is
/// split, at its outermost level:
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
    src: &str,
    toks: &[Token],
    line: std::ops::Range<usize>,
    bytes: std::ops::Range<usize>,
    width: usize,
) -> Option<Vec<String>> {
    let lt = &toks[line.clone()];
    let last = lt.last()?;
    if src[bytes.clone()].trim_start().starts_with("<?")
        || lt.iter().any(|t| matches!(t.tok, Tok::CloseTag | Tok::EchoOpen))
    {
        return None;
    }
    // `if (...):` opens a block only while its `:` ends the line starting with `if`.
    if matches!(lt[0].tok, Tok::If | Tok::Elseif | Tok::Else | Tok::Foreach | Tok::For | Tok::While)
        && last.tok == Tok::Colon
    {
        return None;
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
    let start = |k: usize| lt[k].span.start as usize;
    let end = |k: usize| lt[k].span.end as usize;
    // At least two tokens inside, and no empty element (`f(a,,b)`) but a trailing comma.
    let pair = pairs
        .into_iter()
        .filter(|&(o, c)| {
            let mut bounds = vec![o];
            bounds.extend(commas(o, c));
            matches!(lt[o].tok, Tok::LParen | Tok::LBracket) && c - o > 2 && bounds.windows(2).all(|w| w[1] - w[0] > 1)
        })
        .min_by_key(|&(o, c)| (depth[o], std::cmp::Reverse(end(c) - start(o)), o));

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
            let fits = |k: &&usize| src[bytes.start..end(**k)].chars().count() <= width;
            let k = *at_cd.iter().rfind(fits).unwrap_or(&at_cd[0]);
            return Some(vec![
                src[bytes.start..end(k)].trim_end().to_string(),
                src[end(k)..bytes.end].trim().to_string(),
            ]);
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
        let mut from = bytes.start;
        for &(_, _, k) in &ops {
            pieces.push(src[from..start(k)].trim_end().to_string());
            from = start(k);
        }
        pieces.push(src[from..bytes.end].trim().to_string());
    } else {
        let (o, c) = pair?;
        pieces.push(src[bytes.start..end(o)].trim_end().to_string());
        let mut from = end(o);
        for k in commas(o, c) {
            pieces.push(src[from..end(k)].trim().to_string());
            from = end(k);
        }
        let rest = src[from..start(c)].trim();
        if !rest.is_empty() {
            let comma = if trailing_comma_ok(toks, line.start + o) { "," } else { "" };
            pieces.push(format!("{rest}{comma}"));
        }
        pieces.push(src[start(c)..bytes.end].trim().to_string());
    }
    Some(pieces)
}

/// Whether the bracket `toks[o]` takes a trailing comma: call arguments,
/// parameter lists, closure `use` lists and list/map literals do; grouping,
/// conditions, indexing and anything unsure do not.
fn trailing_comma_ok(toks: &[Token], o: usize) -> bool {
    let prev = |n: usize| o.checked_sub(n).map(|i| &toks[i].tok);
    // `$x->default(` / `$x->list[`: a member name may be a keyword.
    let member = matches!(prev(2), Some(Tok::Arrow | Tok::NullsafeArrow | Tok::DoubleColon));
    match toks[o].tok {
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

    #[test]
    fn one_level_per_line_and_alt_syntax() {
        let src = "$xs->sortBy(function ($a, $b): int {\nreturn $a <=> $b;\n});\nif ($ok):\necho 1;\nelse:\necho 2;\nendif;\n?string $x = null;\n";
        let want = "$xs->sortBy(function ($a, $b): int {\n    return $a <=> $b;\n});\nif ($ok):\n    echo 1;\nelse:\n    echo 2;\nendif;\n?string $x = null;\n";
        assert_eq!(format_source(src).unwrap(), want);
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
}
