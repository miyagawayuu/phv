//! Regular expressions (`std.regex`), with the `regex` crate: matching takes
//! time linear in the input, whatever the pattern, so a pattern or a text from
//! a visitor cannot make a page hang (no catastrophic backtracking).
//!
//! Compiled patterns are kept per thread (a few dozen), so a `Regex` used in a
//! loop or on every request is compiled once.

use super::s;
use phv_vm::{BuiltinFn, Value, Vm, VmError};
use regex::{Regex, RegexBuilder};
use std::cell::RefCell;
use std::collections::HashMap;

/// Longest pattern accepted.
const MAX_PATTERN: usize = 64 * 1024;
/// Memory one compiled pattern may take.
const SIZE_LIMIT: usize = 4 << 20;
/// Compiled patterns kept per thread.
const CACHE: usize = 64;

thread_local! {
    static COMPILED: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

fn compile(pattern: &str) -> Result<Regex, String> {
    if let Some(r) = COMPILED.with(|c| c.borrow().get(pattern).cloned()) {
        return Ok(r);
    }
    if pattern.len() > MAX_PATTERN {
        return Err("the pattern is longer than 64 KB".into());
    }
    let r =
        RegexBuilder::new(pattern).size_limit(SIZE_LIMIT).dfa_size_limit(SIZE_LIMIT).build().map_err(|e| match e {
            regex::Error::Syntax(msg) => msg.lines().last().unwrap_or("invalid pattern").trim().to_string(),
            regex::Error::CompiledTooBig(_) => "the pattern is too large once compiled".to_string(),
            _ => "invalid pattern".to_string(),
        })?;
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE {
            c.clear();
        }
        c.insert(pattern.to_string(), r.clone());
    });
    Ok(r)
}

fn regex(vm: &mut Vm, pattern: &Value) -> Result<Regex, VmError> {
    compile(s(pattern)).map_err(|e| vm.throw("ValueError", format!("invalid regular expression: {e}")))
}

/// The groups of a match: 0 is the whole match; a group that took no part is "".
fn groups(caps: &regex::Captures) -> Value {
    Value::list(caps.iter().map(|m| Value::str(m.map_or("", |m| m.as_str()))).collect())
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__re_check" => |vm, a| {
            regex(vm, &a[0])?;
            Ok(Value::Null)
        },
        "__re_test" => |vm, a| {
            let r = regex(vm, &a[0])?;
            Ok(Value::Bool(r.is_match(s(&a[1]))))
        },
        "__re_find" => |vm, a| {
            let r = regex(vm, &a[0])?;
            Ok(r.find(s(&a[1])).map_or(Value::Null, |m| Value::str(m.as_str())))
        },
        "__re_match" => |vm, a| {
            let r = regex(vm, &a[0])?;
            Ok(r.captures(s(&a[1])).map_or(Value::Null, |c| groups(&c)))
        },
        "__re_match_all" => |vm, a| {
            let r = regex(vm, &a[0])?;
            let mut out = Vec::new();
            for (i, c) in r.captures_iter(s(&a[1])).enumerate() {
                if i % 1024 == 1023 {
                    vm.check_limits()?;
                }
                out.push(groups(&c));
            }
            Ok(Value::list(out))
        },
        "__re_replace" => |vm, a| {
            let r = regex(vm, &a[0])?;
            // `$1`, `${name}`, `$$` for a dollar sign.
            Ok(Value::str(r.replace_all(s(&a[1]), s(&a[2])).into_owned()))
        },
        "__re_replace_with" => |vm, a| {
            let r = regex(vm, &a[0])?;
            let (text, f) = (s(&a[1]).to_string(), a[2].clone());
            let mut out = String::with_capacity(text.len());
            let mut last = 0;
            for c in r.captures_iter(&text) {
                let whole = c.get(0).expect("group 0 is the match");
                out.push_str(&text[last..whole.start()]);
                let with = vm.call_value(&f, &[groups(&c)])?;
                out.push_str(s(&with));
                last = whole.end();
            }
            out.push_str(&text[last..]);
            Ok(Value::str(out))
        },
        "__re_split" => |vm, a| {
            let r = regex(vm, &a[0])?;
            Ok(Value::list(r.split(s(&a[1])).map(Value::str).collect()))
        },
        "__re_escape" => |_, a| Ok(Value::str(regex::escape(s(&a[0])))),
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_compile_once_and_stay_bounded() {
        assert!(compile(r"^(\d+)-(\w+)$").is_ok());
        assert!(compile("(unclosed").is_err());
        assert!(compile(&"a".repeat(MAX_PATTERN + 1)).is_err());
        // A pattern that would compile to something huge is refused, not built.
        assert!(compile(r"\w{1000}\w{1000}\w{1000}\w{1000}").is_err());
        for i in 0..CACHE * 2 {
            compile(&format!("x{i}")).unwrap();
        }
        assert!(COMPILED.with(|c| c.borrow().len()) <= CACHE);
        // Linear time: the classic backtracking bomb finishes at once.
        let start = std::time::Instant::now();
        assert!(!compile("^(a+)+$").unwrap().is_match(&format!("{}!", "a".repeat(100_000))));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
