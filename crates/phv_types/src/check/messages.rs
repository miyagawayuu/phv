//! The keys of `std.i18n` checked at compile time: a call of `t()`, `tn()` or
//! `t_html()` (or of `Translator`'s `t()`, `tn()` and `html()`) whose key is
//! a string literal must name a message of the default catalog, of the kind
//! the function takes (plural for `tn()`), and, when its parameters are a
//! map literal with literal keys (`[]` too, or left out), give each placeholder of the
//! message. Keys and parameters computed at run time are checked when the
//! page runs.

use super::{Checker, FuncId};
use phv_syntax::ast::{Arg, Expr, ExprKind};
use std::collections::HashMap;

/// The messages of the default catalog, for the checks above.
#[derive(Debug, Clone, Default)]
pub struct Messages {
    /// The default catalog's file (`lang/en.toml`), for messages.
    pub catalog: String,
    pub keys: HashMap<String, MessageKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageKey {
    pub plural: bool,
    /// The placeholders of the message, in order.
    pub params: Vec<String>,
}

/// The argument named `name` (or at `index` when positional).
fn arg<'a>(args: &'a [Arg], index: usize, name: &str) -> Option<&'a Expr> {
    args.iter()
        .find(|a| a.name.as_ref().is_some_and(|(n, _)| n == name))
        .or_else(|| args.get(index).filter(|a| a.name.is_none()))
        .map(|a| &a.value)
}

impl Checker<'_> {
    /// The checks of the module's comment on a call of `fid` with `args`.
    pub(crate) fn check_message_call(&mut self, fid: FuncId, args: &[Arg]) {
        let input = self.input;
        let Some(messages) = &input.messages else { return };
        let Some(&module) = input.imports.get("std.i18n") else { return };
        let f = &self.funcs[fid as usize];
        if f.module != module {
            return;
        }
        // Whether the function takes a plural message, and where its parameters are.
        let (plural, params_at) = match f.name.as_str() {
            "t" | "t_html" | "Translator::t" | "Translator::html" => (false, 1),
            "tn" | "Translator::tn" => (true, 2),
            _ => return,
        };
        let Some(key_expr) = arg(args, 0, "key") else { return };
        let ExprKind::Str(key) = &key_expr.kind else { return };
        let span = key_expr.span;
        let catalog = messages.catalog.clone();
        let Some(m) = messages.keys.get(key).cloned() else {
            self.error(format!("there is no message `{key}` in {catalog}"), span)
                .add_note("keys are checked against the catalog of the default locale of [i18n]");
            return;
        };
        if m.plural != plural {
            let msg = if m.plural {
                format!("`{key}` is a plural message: use tn() with a count")
            } else {
                format!("`{key}` is not a plural message: use t()")
            };
            self.error(msg, span);
            return;
        }
        // The parameters given, when all are known here.
        let given: Option<Vec<&str>> = match arg(args, params_at, "params").map(|e| &e.kind) {
            None => Some(Vec::new()),
            // `[]` is read as an empty list, and is an empty map here.
            Some(ExprKind::List(items)) if items.is_empty() => Some(Vec::new()),
            Some(ExprKind::Map(entries)) => entries
                .iter()
                .map(|(k, _)| match &k.kind {
                    ExprKind::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect(),
            Some(_) => None,
        };
        let Some(given) = given else { return };
        let missing: Vec<&String> =
            m.params.iter().filter(|p| !given.contains(&p.as_str()) && !(plural && *p == "count")).collect();
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|p| format!("`{p}`")).collect();
            let s = if missing.len() == 1 { "" } else { "s" };
            self.error(format!("the message `{key}` needs the parameter{s} {}", names.join(", ")), span);
        }
    }
}
