//! `phenix fmt`: normalizes indentation (4 spaces per level) and whitespace of
//! `.phx` files. Lines inside multi-line strings, block comments and inline
//! HTML, and lines that mix code with `<?phx` / `?>` tags, are never changed,
//! so formatting cannot change what a program does.

use crate::lexer::{lex_with_comments, Mode};
use crate::span::line_index;
use crate::token::{Tok, Token};

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

/// Formats a `.phx` source file. Returns an error if it does not lex.
pub fn format_source(src: &str) -> Result<String, String> {
    let (toks, comments, diags) = lex_with_comments(src, 0, 0, Mode::Code);
    if let Some(d) = diags.first() {
        return Err(d.message.clone());
    }
    let toks: Vec<Token> = toks.into_iter().filter(|t| t.tok != Tok::Eof).collect();
    let starts = line_starts(src);
    let line_of = |off: u32| line_index(&starts, off);
    let roles = roles(&toks, &line_of);

    // Byte ranges whose lines must be kept verbatim. The flag marks HTML,
    // where the first line is protected too (not only the inner lines).
    let mut protected: Vec<(usize, usize, bool)> = Vec::new();
    for t in &toks {
        let multi_line = src[t.span.start as usize..t.span.end as usize].contains('\n');
        let html = matches!(t.tok, Tok::InlineHtml(_));
        if html || (multi_line && matches!(t.tok, Tok::Str(_) | Tok::Interp(_) | Tok::Sql(_))) {
            protected.push((t.span.start as usize, t.span.end as usize, html));
        }
    }
    for c in &comments {
        protected.push((c.start as usize, c.end as usize, false));
    }
    let mut html_from: Option<usize> = None;
    for t in &toks {
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

    let newline = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(src.len());
    // Line numbers of the unmatched openers; each distinct line is one level.
    let mut stack: Vec<usize> = Vec::new();
    let levels = |s: &[usize]| {
        let mut n = 0;
        let mut prev = usize::MAX;
        for &l in s {
            if l != prev {
                n += 1;
                prev = l;
            }
        }
        n
    };
    let mut ti = 0;
    let mut blank_run = 0;
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
        for k in line_toks.clone() {
            match roles[k] {
                Role::Open => stack.push(line_no),
                Role::Close => {
                    stack.pop();
                }
                Role::CloseOpen => {
                    stack.pop();
                    stack.push(line_no);
                }
                Role::None => {}
            }
        }

        let inside =
            protected.iter().any(|(s, e, html)| (line_start > *s || (*html && line_start == *s)) && line_start < *e);
        let content = raw.trim_end_matches(['\n', '\r']).trim();
        let mixed = content.starts_with("<?") || line_toks.clone().any(|k| matches!(toks[k].tok, Tok::CloseTag | Tok::EchoOpen));
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
        if let Some(t) = line_toks.clone().next().map(|k| &toks[k].tok) {
            if matches!(t, Tok::Arrow | Tok::NullsafeArrow | Tok::Dot | Tok::AndAnd | Tok::OrOr | Tok::QQ) {
                level += 1;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_code() {
        let src = "<?phx\nfunction f(int $x): int {\nif ($x > 0) {\n  return $x;   \n}\n\n\n\n        return 0;\n}\n$s = \"multi\n   line\";\n$y = $obj\n->a()\n->b();\n";
        let want = "<?phx\nfunction f(int $x): int {\n    if ($x > 0) {\n        return $x;\n    }\n\n    return 0;\n}\n$s = \"multi\n   line\";\n$y = $obj\n    ->a()\n    ->b();\n";
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
        let src = "<?phx $x = 1; ?>\n  <p>\n    keep\n  </p>\n<?phx\n/*\n   block\n */\n  echo $x;\n";
        let out = format_source(src).unwrap();
        assert!(out.contains("  <p>\n    keep\n  </p>"));
        assert!(out.contains("/*\n   block\n */"));
        assert!(out.contains("\necho $x;\n"));
        let crlf = format_source("<?phx\r\nif (true) {\r\necho 1;\r\n}\r\n").unwrap();
        assert_eq!(crlf, "<?phx\r\nif (true) {\r\n    echo 1;\r\n}\r\n");
    }
}
