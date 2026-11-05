//! `phv migrate file.php`: mechanical PHP-to-PHV conversion. It
//! rewrites what can be rewritten safely and leaves `TODO(migrate)` comments
//! for the rest; the type checker then lists what needs manual work.

use std::collections::BTreeMap;

#[derive(PartialEq)]
enum Close {
    Paren,
    /// `array(` became `[`.
    ArrayParen,
    /// `isset(` became `(`; append ` != null` before `)`.
    Isset,
    Bracket,
    /// `$_GET[` became `request()->query(`.
    SuperGlobal,
}

pub struct Migration {
    pub text: String,
    pub changes: BTreeMap<&'static str, usize>,
    pub todos: usize,
}

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

pub fn convert(src: &str) -> Migration {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 64);
    let mut changes: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut stack: Vec<Close> = Vec::new();
    let mut todos = 0;
    let mut uses_request = false;
    let mut uses_session = false;
    let mut i = 0;
    let mut in_php = false;
    // Byte comparisons: `i` may sit inside a multibyte character while
    // scanning comments and strings.
    let starts = |i: usize, pat: &str| b.get(i..).is_some_and(|s| s.starts_with(pat.as_bytes()));
    let word_at = |i: usize, w: &str| {
        b.get(i..i + w.len()).is_some_and(|s| s.eq_ignore_ascii_case(w.as_bytes()))
            && (i == 0 || !ident_char(b[i - 1]))
            && b.get(i + w.len()).is_none_or(|c| !ident_char(*c))
    };
    while i < b.len() {
        if !in_php {
            // PHP's open tag is case-insensitive (`<?PHP`, `<?Php`).
            if b.get(i..i + 5).is_some_and(|s| s.eq_ignore_ascii_case(b"<?php")) {
                out.push_str("<?phv");
                *changes.entry("<?php -> <?phv").or_default() += 1;
                i += 5;
                in_php = true;
            } else if starts(i, "<?=") {
                out.push_str("<?=");
                i += 3;
                in_php = true;
            } else if starts(i, "<?") {
                // A short open tag (or `<?xml`) would run as PHP with short_open_tag
                // on; it is not converted, so it must not pass silently as text.
                out.push_str("<?phv TODO_migrate(\"`<?` short open tag: convert this block by hand\"); ?>");
                todos += 1;
                out.push_str("<?");
                i += 2;
            } else {
                let ch = src[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }
        let c = b[i];
        // strings and comments are copied unchanged
        if c == b'\'' || c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&src[start..i]);
            continue;
        }
        if starts(i, "//") || c == b'#' || starts(i, "/*") {
            let start = i;
            if starts(i, "/*") {
                i = src[i + 2..].find("*/").map(|p| i + 2 + p + 2).unwrap_or(b.len());
            } else {
                while i < b.len() && b[i] != b'\n' && !starts(i, "?>") {
                    i += 1;
                }
            }
            out.push_str(&src[start..i]);
            continue;
        }
        if starts(i, "?>") {
            out.push_str("?>");
            i += 2;
            in_php = false;
            continue;
        }
        let mut rewrite = |from: usize, text: &str, label: &'static str, out: &mut String| {
            out.push_str(text);
            *changes.entry(label).or_default() += 1;
            from
        };
        if starts(i, "===") {
            i = rewrite(i + 3, "==", "=== -> ==", &mut out);
        } else if starts(i, "!==") {
            i = rewrite(i + 3, "!=", "!== -> !=", &mut out);
        } else if word_at(i, "array") && src[i + 5..].trim_start().starts_with('(') {
            let paren = i + 5 + (src[i + 5..].len() - src[i + 5..].trim_start().len());
            stack.push(Close::ArrayParen);
            i = rewrite(paren + 1, "[", "array(...) -> [...]", &mut out);
        } else if word_at(i, "isset") && src[i + 5..].trim_start().starts_with('(') {
            let paren = i + 5 + (src[i + 5..].len() - src[i + 5..].trim_start().len());
            stack.push(Close::Isset);
            i = rewrite(paren + 1, "(", "isset(x) -> (x != null)", &mut out);
        } else if (word_at(i, "var_dump") || word_at(i, "print_r")) && !src[i..].starts_with("print_r_") {
            let len = if word_at(i, "var_dump") { 8 } else { 7 };
            i = rewrite(i + len, "dump", "var_dump/print_r -> dump", &mut out);
        } else if word_at(i, "print") {
            i = rewrite(i + 5, "echo", "print -> echo", &mut out);
        } else if starts(i, "$_SERVER['REQUEST_METHOD']") || starts(i, "$_SERVER[\"REQUEST_METHOD\"]") {
            uses_request = true;
            i = rewrite(i + 26, "request()->method", "$_SERVER -> request()", &mut out);
        } else if starts(i, "$_GET[") || starts(i, "$_POST[") || starts(i, "$_COOKIE[") {
            uses_request = true;
            let (len, call) = if starts(i, "$_GET[") {
                (6, "request()->query(")
            } else if starts(i, "$_POST[") {
                (7, "request()->form(")
            } else {
                (9, "request()->cookie(")
            };
            stack.push(Close::SuperGlobal);
            i = rewrite(i + len, call, "$_GET/$_POST/$_COOKIE -> request()", &mut out);
        } else if starts(i, "$_SESSION[") {
            uses_session = true;
            stack.push(Close::SuperGlobal);
            i = rewrite(i + 10, "session()->get(", "$_SESSION -> session()", &mut out);
        } else if ["require_once", "include_once", "require", "include"].iter().any(|w| word_at(i, w)) {
            // Left as a call that does not compile: an include may be an access
            // check (e.g. `require 'auth.php'`) that must not silently disappear.
            let end = src[i..].find(';').map(|p| i + p + 1).unwrap_or(b.len());
            out.push_str("TODO_migrate(\"replace with an import: ");
            out.push_str(&src[i..end].replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$"));
            out.push_str("\");");
            todos += 1;
            i = end;
        } else if word_at(i, "global") {
            let end = src[i..].find(';').map(|p| i + p + 1).unwrap_or(b.len());
            out.push_str("TODO_migrate(\"pass this value as a parameter: ");
            out.push_str(&src[i..end].replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$"));
            out.push_str("\");");
            todos += 1;
            i = end;
        } else if word_at(i, "new")
            && src[i + 3..].trim_start().get(..8).is_some_and(|w| w.eq_ignore_ascii_case("stdclass"))
        {
            // Only the next word is looked at (found by the 1.0 audit: lowering
            // the rest of the file at each `new` was quadratic).
            out.push_str("[] /* TODO(migrate): stdClass -> a map or a class */ ");
            todos += 1;
            let rest = src[i + 3..].trim_start();
            i = src.len() - rest.len() + 8;
            if src[i..].starts_with("()") {
                i += 2;
            }
        } else if c == b'(' {
            stack.push(Close::Paren);
            out.push('(');
            i += 1;
        } else if c == b')' {
            match stack.pop() {
                Some(Close::ArrayParen) => out.push(']'),
                Some(Close::Isset) => out.push_str(" != null)"),
                _ => out.push(')'),
            }
            i += 1;
        } else if c == b'[' {
            stack.push(Close::Bracket);
            out.push('[');
            i += 1;
        } else if c == b']' {
            match stack.pop() {
                Some(Close::SuperGlobal) => out.push(')'),
                _ => out.push(']'),
            }
            i += 1;
        } else {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let mut imports = Vec::new();
    if uses_request {
        imports.push("request");
    }
    if uses_session {
        imports.push("session");
    }
    if !imports.is_empty() {
        let line = format!("\nimport std.http.{{{}}};\n", imports.join(", "));
        match out.find("<?phv") {
            Some(p) => out.insert_str(p + 5, &line),
            None => out.insert_str(0, &line),
        }
    }
    Migration { text: out, changes, todos }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_php() {
        let src = "<?php\nrequire 'db.php';\n$xs = array(1, array(2, 3));\nif (isset($_GET['id']) && $x === 'a') {\n    var_dump($_POST['name']);\n}\necho '=== in string';\n";
        let m = convert(src);
        assert!(m.text.starts_with("<?phv\nimport std.http.{request};\n"));
        assert!(m.text.contains("$xs = [1, [2, 3]];"));
        assert!(m.text.contains("if ((request()->query('id') != null) && $x == 'a')"));
        assert!(m.text.contains("dump(request()->form('name'));"));
        assert!(m.text.contains("echo '=== in string';"));
        assert!(m.text.contains("TODO_migrate(\"replace with an import: require 'db.php';\");"));
        assert_eq!(m.todos, 1);
    }
}
