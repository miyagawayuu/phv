//! The `{...}` placeholders of `format()`: read by the checker, which checks
//! each value's type against its spec, and by the runtime, which formats.

/// One `{...}` of a format string, as the checker accepted it.
#[derive(Default, Debug, PartialEq)]
pub struct Spec {
    pub fill: char,
    pub align: Option<char>,
    pub zero: bool,
    pub width: usize,
    pub grouping: bool,
    pub precision: Option<usize>,
    pub kind: Option<char>,
}

/// Widths and precisions above this are refused (a format string cannot ask for gigabytes).
pub const MAX_WIDTH: usize = 1000;

/// Parses what follows `:` in `{:...}`: `[[fill]align][0][width][,][.precision][type]`,
/// align `<` `>` `^`, type `x` `X` `o` `b` `e`.
pub fn parse_spec(text: &str) -> Result<Spec, String> {
    let mut spec = Spec { fill: ' ', ..Spec::default() };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^') {
        spec.fill = chars[0];
        spec.align = Some(chars[1]);
        i = 2;
    } else if chars.first().is_some_and(|c| matches!(c, '<' | '>' | '^')) {
        spec.align = Some(chars[0]);
        i = 1;
    }
    if chars.get(i) == Some(&'0') {
        spec.zero = true;
        i += 1;
    }
    let num = |i: &mut usize| -> Option<usize> {
        let start = *i;
        while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
            *i += 1;
        }
        (start < *i).then(|| chars[start..*i].iter().collect::<String>().parse().unwrap_or(usize::MAX))
    };
    spec.width = num(&mut i).unwrap_or(0);
    if chars.get(i) == Some(&',') {
        spec.grouping = true;
        i += 1;
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        spec.precision = Some(num(&mut i).ok_or("expected digits after `.`")?);
    }
    if let Some(&c) = chars.get(i) {
        if !matches!(c, 'x' | 'X' | 'o' | 'b' | 'e') {
            return Err(format!("unknown format type `{c}` (use x, X, o, b or e)"));
        }
        spec.kind = Some(c);
        i += 1;
    }
    if i != chars.len() {
        return Err(format!("unexpected `{}` in the format", chars[i..].iter().collect::<String>()));
    }
    if spec.width > MAX_WIDTH || spec.precision.is_some_and(|p| p > MAX_WIDTH) {
        return Err(format!("widths and precisions are at most {MAX_WIDTH}"));
    }
    Ok(spec)
}

/// The pieces of a format string: literal text, and placeholders (`{}` or
/// `{:spec}`, the spec text after `:`). `{{` and `}}` are literal braces.
pub enum Piece {
    Text(String),
    Value(String),
}

pub fn pieces(fmt: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => {
                it.next();
                text.push('{');
            }
            '}' if it.peek() == Some(&'}') => {
                it.next();
                text.push('}');
            }
            '{' => {
                let mut inner = String::new();
                loop {
                    match it.next() {
                        Some('}') => break,
                        Some(ch) => inner.push(ch),
                        None => return Err("a `{` is not closed (write `{{` for a brace)".into()),
                    }
                }
                let spec = match inner.strip_prefix(':') {
                    Some(s) => s.to_string(),
                    None if inner.is_empty() => String::new(),
                    None => {
                        return Err(format!("`{{{inner}}}`: write `{{}}` or `{{:spec}}`; values are given in order"))
                    }
                };
                parse_spec(&spec)?;
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(Piece::Value(spec));
            }
            '}' => return Err("a lone `}` (write `}}` for a brace)".into()),
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}
