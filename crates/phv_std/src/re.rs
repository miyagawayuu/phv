//! Regular expressions (`std.regex`), with the `regex` crate: no backtracking,
//! so a search takes time proportional to the text times the size of the
//! compiled pattern, never more.
//!
//! The regex crate cannot be interrupted, so the time and memory limits of a
//! run are applied around it:
//! - A search whose worst case (states of the compiled pattern × length of the
//!   text) could not finish in the time the run has left is refused before it
//!   starts (`RuntimeException`). Small patterns are never refused in practice;
//!   a large pattern from a visitor cannot hold a worker past the time limit.
//! - Results are accounted for (`Vm::reserve`) before they grow, and the limits
//!   are checked between matches.
//! - Patterns are at most 64 KB, 2 MB once compiled, with at most 1000 groups
//!   and 16 MB for the table a search for groups uses (states × groups).
//!
//! Compiled patterns are kept per thread, up to 64 of them and 32 MB counted
//! generously, so a `Regex` used in a loop or on every request is compiled once.

use super::s;
use phv_vm::{BuiltinFn, Value, Vm, VmError};
use regex::{Regex, RegexBuilder};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

/// Longest pattern accepted.
const MAX_PATTERN: usize = 64 * 1024;
/// Memory the compiled pattern (its NFA) may take.
const SIZE_LIMIT: usize = 2 << 20;
/// Memory of the lazy DFA's cache, per direction.
const DFA_LIMIT: usize = 512 << 10;
/// Capture groups a pattern may have (each match builds a list of them), and
/// the memory a search for them may need.
const MAX_GROUPS: usize = 1000;
const MAX_GROUPS_BYTES: usize = 16 << 20;
/// Compiled patterns kept per thread, and the memory they may hold.
const CACHE: usize = 64;
const CACHE_BYTES: usize = 32 << 20;
/// Steps of the slowest search (one NFA state for one byte of text) assumed
/// per second: measured at about 1.3 × 10^8, so a search allowed by the
/// budget ends within the time left even on a slower machine.
const STEPS_PER_SEC: f64 = 1e8;
/// Memory of one item of a list, and of a new string or list beyond its contents.
const VALUE: usize = std::mem::size_of::<Value>();
const OVERHEAD: usize = 64;

#[derive(Clone)]
struct Compiled {
    regex: Regex,
    /// States of the pattern's NFA: the cost of one byte of text in the worst case.
    states: usize,
    /// Memory a search for groups needs (the slot table of the PikeVM:
    /// two sets of states × slots), reserved before such a search.
    groups_bytes: usize,
}

#[derive(Default)]
struct Cache {
    patterns: HashMap<String, Compiled>,
    bytes: usize,
}

thread_local! {
    static COMPILED: RefCell<Cache> = RefCell::new(Cache::default());
}

fn compile(pattern: &str) -> Result<Compiled, String> {
    if let Some(c) = COMPILED.with(|c| c.borrow().patterns.get(pattern).cloned()) {
        return Ok(c);
    }
    if pattern.len() > MAX_PATTERN {
        return Err("the pattern is longer than 64 KB".into());
    }
    let too_big = || "the pattern is too large once compiled".to_string();
    let regex =
        RegexBuilder::new(pattern).size_limit(SIZE_LIMIT).dfa_size_limit(DFA_LIMIT).build().map_err(|e| match e {
            regex::Error::Syntax(msg) => msg.lines().last().unwrap_or("invalid pattern").trim().to_string(),
            regex::Error::CompiledTooBig(_) => too_big(),
            _ => "invalid pattern".to_string(),
        })?;
    if regex.captures_len() > MAX_GROUPS + 1 {
        return Err(format!("the pattern has more than {MAX_GROUPS} groups"));
    }
    let config = regex_automata::nfa::thompson::Config::new().nfa_size_limit(Some(SIZE_LIMIT));
    let nfa = regex_automata::nfa::thompson::Compiler::new().configure(config).build(pattern).map_err(|_| too_big())?;
    let states = nfa.states().len();
    let groups_bytes = states.saturating_mul(regex.captures_len()).saturating_mul(32);
    if groups_bytes > MAX_GROUPS_BYTES {
        return Err("the pattern has too many groups for its size".into());
    }
    // The forward and reverse NFAs, the search caches, both lazy DFAs and the groups' table.
    let bytes = nfa.memory_usage() * 3 + DFA_LIMIT * 2 + (256 << 10) + groups_bytes;
    let compiled = Compiled { regex, states, groups_bytes };
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if bytes > CACHE_BYTES / 4 {
            return;
        }
        if c.patterns.len() >= CACHE || c.bytes + bytes > CACHE_BYTES {
            c.patterns.clear();
            c.bytes = 0;
        }
        c.bytes += bytes;
        c.patterns.insert(pattern.to_string(), compiled.clone());
    });
    Ok(compiled)
}

fn regex(vm: &mut Vm, pattern: &Value) -> Result<Compiled, VmError> {
    compile(s(pattern)).map_err(|e| vm.throw("ValueError", format!("invalid regular expression: {e}")))
}

/// Refuses a search whose worst case would not end in the time the run has left.
fn budget(vm: &mut Vm, c: &Compiled, text: &str) -> Result<(), VmError> {
    let Some(deadline) = vm.deadline() else { return Ok(()) };
    let left = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
    let worst = c.states as f64 * (text.len() as f64 + 1.0);
    if worst > left * STEPS_PER_SEC {
        return Err(vm.throw(
            "RuntimeException",
            format!(
                "the regular expression is too large to search {} bytes of text in the time left (use a smaller pattern or a shorter text)",
                text.len()
            ),
        ));
    }
    Ok(())
}

/// `budget`, and for a search for groups the memory it needs.
fn budget_groups(vm: &mut Vm, c: &Compiled, text: &str) -> Result<(), VmError> {
    budget(vm, c, text)?;
    vm.reserve(c.groups_bytes)
}

/// The groups of a match: 0 is the whole match; a group that took no part is "".
fn groups(vm: &Vm, caps: &regex::Captures) -> Result<Value, VmError> {
    let text: usize = caps.iter().map(|m| m.map_or(0, |m| m.len())).sum();
    vm.reserve(caps.len().saturating_mul(VALUE + OVERHEAD).saturating_add(text))?;
    Ok(Value::list(caps.iter().map(|m| Value::str(m.map_or("", |m| m.as_str()))).collect()))
}

/// Checks the limits after every match. One search is bounded by `budget`,
/// but each search for the next match may read on to the end of the text:
/// many short matches cost the pattern's states times the text's length
/// squared (found by the 1.0 audit: 97 seconds with a 2-second limit).
fn tick(vm: &Vm, _i: usize) -> Result<(), VmError> {
    vm.check_limits()
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__re_check" => |vm, a| {
            regex(vm, &a[0])?;
            Ok(Value::Null)
        },
        "__re_test" => |vm, a| {
            let c = regex(vm, &a[0])?;
            budget(vm, &c, s(&a[1]))?;
            Ok(Value::Bool(c.regex.is_match(s(&a[1]))))
        },
        "__re_find" => |vm, a| {
            let c = regex(vm, &a[0])?;
            budget(vm, &c, s(&a[1]))?;
            Ok(c.regex.find(s(&a[1])).map_or(Value::Null, |m| Value::str(m.as_str())))
        },
        "__re_match" => |vm, a| {
            let c = regex(vm, &a[0])?;
            budget_groups(vm, &c, s(&a[1]))?;
            match c.regex.captures(s(&a[1])) {
                Some(caps) => groups(vm, &caps),
                None => Ok(Value::Null),
            }
        },
        "__re_match_all" => |vm, a| {
            let c = regex(vm, &a[0])?;
            budget_groups(vm, &c, s(&a[1]))?;
            let mut out = Vec::new();
            for (i, caps) in c.regex.captures_iter(s(&a[1])).enumerate() {
                tick(vm, i)?;
                out.push(groups(vm, &caps)?);
            }
            Ok(Value::list(out))
        },
        "__re_replace" => |vm, a| {
            let c = regex(vm, &a[0])?;
            let (text, with) = (s(&a[1]), s(&a[2]));
            budget_groups(vm, &c, text)?;
            // `$1`, `${name}`, `$$` for a dollar sign. Each `$` adds at most the
            // match (a group lies inside it), so the growth is known beforehand.
            let refs = with.matches('$').count();
            let mut out = String::new();
            let mut last = 0;
            for (i, caps) in c.regex.captures_iter(text).enumerate() {
                tick(vm, i)?;
                let m = caps.get(0).expect("group 0 is the match");
                let grow = (m.start() - last).saturating_add(with.len()).saturating_add(refs.saturating_mul(m.len()));
                vm.reserve(grow)?;
                out.reserve(grow);
                out.push_str(&text[last..m.start()]);
                caps.expand(with, &mut out);
                last = m.end();
            }
            vm.reserve(text.len() - last)?;
            out.push_str(&text[last..]);
            Ok(Value::str(out))
        },
        "__re_replace_with" => |vm, a| {
            let c = regex(vm, &a[0])?;
            let (text, f) = (s(&a[1]).to_string(), a[2].clone());
            budget_groups(vm, &c, &text)?;
            let mut out = String::new();
            let mut last = 0;
            for caps in c.regex.captures_iter(&text) {
                let whole = caps.get(0).expect("group 0 is the match");
                let with = vm.call_value(&f, &[groups(vm, &caps)?])?;
                let grow = (whole.start() - last).saturating_add(s(&with).len());
                vm.reserve(grow)?;
                out.reserve(grow);
                out.push_str(&text[last..whole.start()]);
                out.push_str(s(&with));
                last = whole.end();
            }
            vm.reserve(text.len() - last)?;
            out.push_str(&text[last..]);
            Ok(Value::str(out))
        },
        "__re_split" => |vm, a| {
            let c = regex(vm, &a[0])?;
            budget(vm, &c, s(&a[1]))?;
            let mut out = Vec::new();
            for (i, piece) in c.regex.split(s(&a[1])).enumerate() {
                tick(vm, i)?;
                vm.reserve((VALUE + OVERHEAD).saturating_add(piece.len()))?;
                out.push(Value::str(piece));
            }
            Ok(Value::list(out))
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
        assert!(compile(&"()".repeat(MAX_GROUPS + 1)).is_err());
        assert!(compile(&"()".repeat(MAX_GROUPS)).is_err(), "too many groups for its size");
        assert!(compile(&"(a)".repeat(100)).is_ok());
        for i in 0..CACHE * 2 {
            compile(&format!("x{i}")).unwrap();
        }
        COMPILED.with(|c| {
            let c = c.borrow();
            assert!(c.patterns.len() <= CACHE && c.bytes <= CACHE_BYTES);
        });
        // The worst case of a search is counted in NFA states.
        assert!(compile(r"(?-u)(\w{1,100}){20}b").unwrap().states > 1000);
        assert!(compile(r"(?i)hello\s+world").unwrap().states < 100);
        // Linear time: the classic backtracking bomb finishes at once.
        let start = std::time::Instant::now();
        assert!(!compile("^(a+)+$").unwrap().regex.is_match(&format!("{}!", "a".repeat(100_000))));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}
