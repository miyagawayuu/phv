//! What the checker records for editor support (`phv lsp`): where each name
//! is defined, what can follow `->` and `::`, and the variables of each function.
//! It is only built when asked for (`check_for_editor`).

use crate::types::{ClassId, Type};
use phv_syntax::ast::Visibility;
use phv_syntax::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Function,
    Method,
    Property,
    Class,
    Interface,
    Enum,
    EnumCase,
    Constant,
    Variable,
    Keyword,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub kind: ItemKind,
    /// A signature or a type, shown next to the name.
    pub detail: String,
    /// Where it is defined (`None` for built-ins).
    pub def: Option<Span>,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub item: Item,
    /// Static methods, constants and enum cases (listed after `::`).
    pub is_static: bool,
    pub vis: Visibility,
    /// The class that declares it (a parent for inherited members).
    pub owner: ClassId,
}

/// A `->name` or `::name`: the span of the name and what it is looked up on.
#[derive(Clone, Debug)]
pub struct MemberSite {
    pub span: Span,
    pub recv: Type,
    pub is_static: bool,
    /// The class whose code contains the site (private members are listed there).
    pub class: Option<ClassId>,
}

/// The variables of a function body (or of the top level of a script or template).
#[derive(Clone, Debug)]
pub struct VarScope {
    pub span: Span,
    /// An arrow function, which can also use the variables around it.
    pub sees_outer: bool,
    /// Each variable with the place it is first assigned or declared.
    pub vars: Vec<Item>,
}

#[derive(Clone, Debug, Default)]
pub struct EditorIndex {
    /// Each use of a name and the span of its definition.
    pub refs: Vec<(Span, Span)>,
    pub sites: Vec<MemberSite>,
    pub scopes: Vec<VarScope>,
    /// Members of each class, by class id (inherited ones included), with types
    /// in terms of the class's own type parameters.
    pub classes: Vec<Vec<Member>>,
    /// Names of the classes and of their type parameters, by class id.
    pub class_names: Vec<String>,
    pub type_params: Vec<Vec<String>>,
    /// Functions, classes and constants visible in the entry module, built-ins included.
    pub globals: Vec<Item>,
    pub string_methods: Vec<Item>,
    pub list_methods: Vec<Item>,
    pub map_methods: Vec<Item>,
    /// Every function, method, class and module constant of the program (all
    /// modules), for what a definition is (semantic tokens, signatures).
    pub defs: Vec<Item>,
    /// Itself, its ancestors and its interfaces, by class id.
    pub supers: Vec<Vec<ClassId>>,
}

/// Longest completion detail: a signature is shortened past this (a huge type
/// argument repeated in every parameter would otherwise make huge responses).
const MAX_DETAIL: usize = 2000;

/// `s` cut to `max` bytes (at a character boundary), with `…` when it was longer.
fn shorten(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

/// Replaces the type parameter names `names` in a rendered signature by
/// `values` (whole words only; `$name` parameter names are left alone).
/// The result is at most about `MAX_DETAIL` bytes.
fn subst_words(s: &str, names: &[String], values: &[String]) -> String {
    let values: Vec<String> = values.iter().map(|v| shorten(v.clone(), 200)).collect();
    let mut out = String::new();
    let mut word = String::new();
    let mut after_dollar = false;
    let flush = |word: &mut String, out: &mut String, after_dollar: bool| {
        match names.iter().position(|n| n == word) {
            Some(i) if !after_dollar => out.push_str(values.get(i).map_or(word.as_str(), |v| v.as_str())),
            _ => out.push_str(word),
        }
        word.clear();
    };
    for ch in s.chars() {
        if out.len() > MAX_DETAIL {
            break;
        }
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
            continue;
        }
        flush(&mut word, &mut out, after_dollar);
        after_dollar = ch == '$';
        out.push(ch);
    }
    flush(&mut word, &mut out, after_dollar);
    shorten(out, MAX_DETAIL)
}

fn contains(span: Span, file: u32, offset: u32) -> bool {
    span.file == file && span.start <= offset && offset <= span.end
}

impl EditorIndex {
    /// The definition of the name at `offset` (the innermost use around it).
    pub fn definition(&self, file: u32, offset: u32) -> Option<Span> {
        self.refs
            .iter()
            .filter(|(use_, _)| contains(*use_, file, offset))
            .min_by_key(|(use_, _)| use_.end - use_.start)
            .map(|(_, def)| *def)
    }

    /// What is defined at `def` (a function, class, member, constant or variable).
    pub fn item_at_def(&self, def: Span) -> Option<&Item> {
        let same = |i: &&Item| i.def == Some(def);
        self.defs
            .iter()
            .find(same)
            .or_else(|| self.classes.iter().flatten().map(|m| &m.item).find(same))
            .or_else(|| self.scopes.iter().flat_map(|s| &s.vars).find(same))
    }

    /// Every use of the definition at `def` (in the files this index covers).
    pub fn uses_of(&self, def: Span) -> impl Iterator<Item = Span> + '_ {
        self.refs.iter().filter(move |(_, d)| *d == def).map(|(u, _)| *u)
    }

    /// The definitions that must be renamed with the member defined at `def`:
    /// the members of that name in the classes it overrides or that override
    /// it (and so on), so that overriding stays overriding. Just `def` for
    /// anything else.
    pub fn related_defs(&self, def: Span) -> Vec<Span> {
        let declared = |c: usize, name: &str| {
            self.classes.get(c).and_then(|ms| {
                ms.iter()
                    .find(|m| m.owner as usize == c && m.item.name == name && !m.is_static)
                    .and_then(|m| m.item.def)
            })
        };
        let Some(m) = self.classes.iter().flatten().find(|m| m.item.def == Some(def)) else { return vec![def] };
        if m.is_static || !matches!(m.item.kind, ItemKind::Method | ItemKind::Property) {
            return vec![def];
        }
        let name = m.item.name.clone();
        let related = |a: usize, b: usize| {
            self.supers.get(a).is_some_and(|s| s.contains(&(b as ClassId)))
                || self.supers.get(b).is_some_and(|s| s.contains(&(a as ClassId)))
        };
        let mut set = vec![m.owner as usize];
        let mut changed = true;
        while changed {
            changed = false;
            for c in 0..self.classes.len() {
                if !set.contains(&c) && declared(c, &name).is_some() && set.iter().any(|&s| related(c, s)) {
                    set.push(c);
                    changed = true;
                }
            }
        }
        let mut out: Vec<Span> = set.into_iter().filter_map(|c| declared(c, &name)).collect();
        if !out.contains(&def) {
            out.push(def);
        }
        out
    }

    /// What can be written at the member site that starts at `start`.
    pub fn members_at(&self, file: u32, start: u32) -> Option<Vec<Item>> {
        let site = self.sites.iter().find(|s| s.span.file == file && s.span.start == start)?;
        Some(self.members_of(&site.recv, site.is_static, site.class))
    }

    fn members_of(&self, ty: &Type, is_static: bool, from: Option<ClassId>) -> Vec<Item> {
        let mut out: Vec<Item> = Vec::new();
        let types: Vec<&Type> = match ty {
            Type::Union(ms) => ms.iter().filter(|t| **t != Type::Null).collect(),
            t => vec![t],
        };
        for t in types {
            let items: Vec<Item> = match t {
                Type::Str if !is_static => self.string_methods.clone(),
                Type::List(_) if !is_static => self.list_methods.clone(),
                Type::Map(..) if !is_static => self.map_methods.clone(),
                Type::Class(c, args) if !args.is_empty() => {
                    let names = self.type_params.get(*c as usize).cloned().unwrap_or_default();
                    let values: Vec<String> = args.iter().map(|a| a.display(&self.class_names).to_string()).collect();
                    self.members_of(&Type::Class(*c, Vec::new()), is_static, from)
                        .into_iter()
                        .map(|i| Item { detail: subst_words(&i.detail, &names, &values), ..i })
                        .collect()
                }
                Type::Class(c, _) => self
                    .classes
                    .get(*c as usize)
                    .into_iter()
                    .flatten()
                    .filter(|m| m.is_static == is_static)
                    .filter(|m| m.vis == Visibility::Public || from.is_some_and(|f| f == m.owner || f == *c))
                    .filter(|m| m.item.name != "__construct")
                    .map(|m| m.item.clone())
                    .collect(),
                _ => Vec::new(),
            };
            for i in items {
                if !out.iter().any(|o| o.name == i.name) {
                    out.push(i);
                }
            }
        }
        out
    }

    /// The variables assigned before `offset` in the innermost function around it
    /// (and around that, for arrow functions).
    pub fn variables_at(&self, file: u32, offset: u32) -> Vec<Item> {
        let mut scopes: Vec<&VarScope> = self.scopes.iter().filter(|s| contains(s.span, file, offset)).collect();
        scopes.sort_by_key(|s| s.span.end - s.span.start);
        let mut out: Vec<Item> = Vec::new();
        for scope in scopes {
            for v in &scope.vars {
                let before = v.def.is_none_or(|d| d.file != file || d.start <= offset);
                if before && !out.iter().any(|o| o.name == v.name) {
                    out.push(v.clone());
                }
            }
            if !scope.sees_outer {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A huge type argument repeated in many parameters gives a short detail.
    #[test]
    fn substituted_details_are_bounded() {
        let names = vec!["T".to_string()];
        let detail = format!("({})", vec!["T $x"; 10_000].join(", "));
        let out = subst_words(&detail, &names, &["N".repeat(100_000)]);
        assert!(out.len() <= MAX_DETAIL + 300, "{}", out.len());
        assert!(out.ends_with('…'));
        assert_eq!(subst_words("(T $T): T", &names, &["int".into()]), "(int $T): int");
    }
}
