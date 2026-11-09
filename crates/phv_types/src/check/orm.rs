//! std.orm: `new Repo<User>($db)` is given a description of `User` (its
//! table, key and columns, from `#[Table]`, `#[Id]` and `#[Column]`), which
//! the runtime (`phv_std::orm`) uses to build the SQL and to turn rows into
//! objects and back.
//!
//! The description is text, one item per line:
//!
//! ```text
//! E <class id> <table> <index of the key among the P lines> <1: the database makes the key>
//! P <property> <column> <kind> <the property's type, for messages>
//! c<class id> ...      (classes inside JSON columns, as `json_decode_as` describes them)
//! ```
//!
//! The fields of a line are separated by tabs. A kind is `i`, `f`, `s`, `b`,
//! `eN` (backed enum N, by its value), `nN` (pure enum N, by its case name) or
//! `j` and a `json_decode_as` shape (a list, map, set, class or `any`, kept
//! as JSON text), after `?` when the column may be NULL.

use super::expr::{err, hconst};
use super::Checker;
use crate::hir::*;
use crate::types::*;
use phv_syntax::ast::{Arg, ClassKind, Visibility};
use phv_syntax::Span;

/// The longest table or column name (PostgreSQL's limit; MySQL allows 64).
pub(super) const MAX_SQL_NAME: usize = 63;

/// Whether `name` can name a table or column: letters, digits and `_`, not
/// starting with a digit (it is quoted in the SQL, so keywords are fine).
pub(super) fn sql_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let plain = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        return Err("a name may hold only letters, digits and `_`, and may not start with a digit".into());
    }
    if name.len() > MAX_SQL_NAME {
        return Err(format!("a name may be at most {MAX_SQL_NAME} characters long"));
    }
    Ok(())
}

impl Checker<'_> {
    /// `Repo` of std.orm.
    pub(super) fn is_orm_repo(&self, cid: ClassId) -> bool {
        let ci = &self.classes[cid as usize];
        ci.name == "Repo"
            && self.input.modules[ci.module].kind == super::ModuleKind::Std
            && self.input.imports.get("std.orm") == Some(&ci.module)
    }

    /// The arguments of `new Repo<T>($db)`: the database the program gave, then
    /// the description of `T` and the defaults it refers to.
    pub(super) fn orm_repo_args(
        &mut self,
        mut hargs: Vec<HExpr>,
        targs: &[Type],
        args: &[Arg],
        span: Span,
    ) -> Vec<HExpr> {
        if args.len() != 1 || args[0].name.as_ref().is_some_and(|(n, _)| n != "db") || hargs.len() != 3 {
            self.error("`new Repo<...>()` takes the database only, e.g. `new Repo<User>($db)`", span);
            return hargs;
        }
        let Some(t) = targs.first().filter(|t| !t.has_unknown() && **t != Type::Error).cloned() else {
            return hargs;
        };
        match self.orm_meta(&t) {
            Ok((desc, defaults)) => {
                hargs[1] = hconst(HConst::Str(desc.as_str().into()), Type::Str, span);
                let defaults = defaults.into_iter().map(|c| hconst(c, Type::Any, span)).collect();
                hargs[2] = HExpr { kind: HExprKind::List(defaults), ty: Type::List(Box::new(Type::Any)), span };
            }
            Err(msg) => {
                let name = self.tn(&t);
                self.error(format!("`Repo<{name}>`: {msg}"), span);
                hargs[1] = err(span);
            }
        }
        hargs
    }

    /// The description of entity class `t` (see the top of this file), and
    /// the constructor defaults its JSON columns refer to.
    fn orm_meta(&mut self, t: &Type) -> Result<(String, Vec<HConst>), String> {
        let Type::Class(cid, args) = t else {
            return Err(format!("`{}` is not a class; an entity is a class marked `#[Table(\"...\")]`", self.tn(t)));
        };
        let cid = *cid;
        let ci = &self.classes[cid as usize];
        let name = self.class_names[cid as usize].clone();
        if ci.kind != ClassKind::Class {
            return Err(format!(
                "`{name}` is not a plain class (interfaces, abstract classes and enums are not entities)"
            ));
        }
        if !args.is_empty() {
            return Err(format!("`{name}` is generic"));
        }
        let Some(table) = self.orm_tables.get(&cid).cloned() else {
            return Err(format!("`{name}` names no table; write `#[Table(\"...\")]` before `class {name}`"));
        };
        // Rows become objects without a call to the constructor, so it must
        // not skip a check the constructor makes (as for `json_decode_as`).
        if ci.ctor.is_some() {
            let (vis, owner) = ci.ctor_access;
            if vis != Visibility::Public {
                return Err(format!("`{name}` has a non-public constructor"));
            }
            let runs_code = self.classes[owner as usize]
                .decl
                .and_then(|d| d.methods.iter().find(|m| m.name == "__construct"))
                .is_some_and(|m| !m.body.is_empty());
            if runs_code {
                return Err(format!(
                    "the constructor of `{}` runs code, which reading a row would skip (an entity's constructor may only promote its parameters)",
                    self.class_names[owner as usize]
                ));
            }
        }
        let ctor = ci.ctor;
        let fields = ci.fields.clone();
        let mut defs: Vec<(ClassId, String)> = Vec::new();
        let mut defaults: Vec<HConst> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        let mut ids: Vec<usize> = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            if f.vis != Visibility::Public {
                return Err(format!(
                    "`{name}` has the non-public property `${}`; an entity's properties are its columns, and are public",
                    f.name
                ));
            }
            let promoted = ctor
                .and_then(|fid| self.funcs[fid as usize].params.iter().find(|p| p.name == f.name))
                .is_some_and(|p| p.promote.is_some());
            if f.readonly && !promoted {
                return Err(format!(
                    "`{name}` has the readonly property `${}`, which is not a constructor parameter and so cannot be read from a row",
                    f.name
                ));
            }
            let column = self.orm_columns.get(&(f.owner, f.name.clone())).cloned().unwrap_or_else(|| f.name.clone());
            if sql_name(&column).is_err() {
                return Err(format!(
                    "the name of `{name}::${}` is not a plain SQL name; name its column with `#[Column(\"...\")]`",
                    f.name
                ));
            }
            if let Some(other) = columns.iter().position(|c| c.eq_ignore_ascii_case(&column)) {
                return Err(format!(
                    "`{name}::${}` and `{name}::${}` are both the column `{column}`",
                    fields[other].name, f.name
                ));
            }
            let kind =
                self.orm_kind(&f.ty, &mut defs, &mut defaults).map_err(|m| format!("`{name}::${}`: {m}", f.name))?;
            if self.orm_ids.contains(&(f.owner, f.name.clone())) {
                ids.push(i);
            }
            lines.push(format!("P\t{}\t{column}\t{kind}\t{}", f.name, self.tn(&f.ty)));
            columns.push(column);
        }
        let key = match ids.as_slice() {
            [k] => *k,
            [] => fields.iter().position(|f| f.name == "id").ok_or_else(|| {
                format!("`{name}` has no key; mark the property that holds it `#[Id]`, or name it `$id`")
            })?,
            _ => return Err(format!("more than one property of `{name}` is marked `#[Id]`")),
        };
        let kf = &fields[key];
        let generated = kf.ty.is_nullable();
        if !matches!(kf.ty.without_null(), Type::Int | Type::Str) {
            return Err(format!(
                "the key `{name}::${}` must be an `int` or a `string` (`?int` or `?string` when the database makes it), not `{}`",
                kf.name,
                self.tn(&kf.ty)
            ));
        }
        if generated && kf.readonly {
            return Err(format!(
                "the key `{name}::${}` may be made by the database (it is nullable) and is then set by `insert()`, so it cannot be readonly",
                kf.name
            ));
        }
        let mut desc = format!("E\t{cid}\t{table}\t{key}\t{}", generated as u8);
        for l in lines {
            desc.push('\n');
            desc.push_str(&l);
        }
        for (c, fields) in defs {
            desc.push_str(&format!("\nc{c} {fields}"));
        }
        Ok((desc, defaults))
    }

    /// The kind of a column holding a property of type `t` (see the top of this file).
    fn orm_kind(
        &self,
        t: &Type,
        defs: &mut Vec<(ClassId, String)>,
        defaults: &mut Vec<HConst>,
    ) -> Result<String, String> {
        if t.members().iter().any(|m| m.is_html()) {
            return Err(format!(
                "`{}` cannot be read from a database: what a column holds would be trusted as HTML; store a `string` (it is escaped when it is output)",
                self.tn(t)
            ));
        }
        // `any` holds null too, kept as NULL.
        if *t == Type::Any {
            return Ok(format!("?j{}", self.json_shape(t, defs, defaults)?));
        }
        let (null, inner) = if t.is_nullable() && !matches!(t.without_null(), Type::Union(_)) {
            ("?", t.without_null())
        } else {
            ("", t.clone())
        };
        let kind = match &inner {
            Type::Int => "i".to_string(),
            Type::Float => "f".to_string(),
            Type::Str => "s".to_string(),
            Type::Bool => "b".to_string(),
            Type::Class(cid, _) if self.classes[*cid as usize].kind == ClassKind::Enum => {
                let by_value = self.classes[*cid as usize].backing.is_some();
                format!("{}{cid}", if by_value { "e" } else { "n" })
            }
            Type::List(_) | Type::Map(..) | Type::Set(_) | Type::Class(..) => {
                format!("j{}", self.json_shape(&inner, defs, defaults)?)
            }
            other => return Err(format!("`{}` cannot be kept in a column", self.tn(other))),
        };
        Ok(format!("{null}{kind}"))
    }
}
