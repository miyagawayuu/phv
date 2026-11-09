//! Traits (`trait T { ... }`, `use T;` in a class body). A trait is not a
//! type: its members are copied into each class that uses it, and its methods
//! are checked and compiled as methods of that class (`$this`, `self` and
//! `static` are the class), so the VM and the JIT see ordinary methods.
//!
//! Precedence, as in PHP: a method the class declares wins over one of a
//! trait, which wins over an inherited one (and must then be a compatible
//! override). Two traits giving a method of the same name are an error unless
//! `insteadof` picks one. Abstract methods of a trait are requirements the
//! class meets with a compatible method (its own, another trait's or an
//! inherited one).

use super::*;

/// Bytes of trait methods copied into classes, all classes together: each
/// copy is checked and compiled, so this is limited as a program's source is.
pub(super) const MAX_TRAIT_CODE: usize = 32 << 20;

/// A method a class takes from a trait.
#[derive(Clone)]
pub(super) struct TraitMethod<'a> {
    pub(super) decl: &'a FunctionDecl,
    /// Its name in the class (an alias given with `as`).
    pub(super) name: String,
    pub(super) vis: Visibility,
    /// The trait that declares it, and what the trait's type parameters stand for.
    pub(super) origin: ClassId,
    pub(super) subst: Vec<(Name, Type)>,
}

#[derive(Clone)]
pub(super) struct TraitProp<'a> {
    pub(super) decl: &'a ast::PropDecl,
    pub(super) origin: ClassId,
    pub(super) subst: Vec<(Name, Type)>,
}

#[derive(Clone)]
pub(super) struct TraitConst<'a> {
    pub(super) decl: &'a ast::ConstDecl,
    pub(super) origin: ClassId,
}

/// Everything a class takes from the traits it uses (and they from theirs).
#[derive(Default)]
pub(super) struct TraitMembers<'a> {
    /// Methods with a body, one per name.
    pub(super) methods: Vec<TraitMethod<'a>>,
    /// Abstract methods: the class must have a compatible method of that name.
    pub(super) required: Vec<TraitMethod<'a>>,
    /// Properties and constants; two of the same name must be the same.
    pub(super) props: Vec<TraitProp<'a>>,
    pub(super) consts: Vec<TraitConst<'a>>,
}

impl TraitMembers<'_> {
    fn has_method(&self, name: &str) -> bool {
        self.methods.iter().chain(&self.required).any(|m| m.name == name)
    }
}

/// The note added to a diagnostic found in a trait's member, checked as a
/// member of the class that uses it.
fn used_by(trait_name: &str, class_name: &str) -> String {
    format!("in trait `{trait_name}`, used by `{class_name}`")
}

impl<'a> Checker<'a> {
    pub(super) fn is_trait(&self, c: ClassId) -> bool {
        self.classes[c as usize].kind == ClassKind::Trait
    }

    /// Reports `T` (a trait) used where a class or a type is needed: `what`
    /// says what was tried.
    pub(super) fn refuse_trait(&mut self, c: ClassId, what: &str, span: Span) {
        let name = self.class_names[c as usize].clone();
        self.error(format!("`{name}` is a trait: {what}"), span).add_note(format!(
            "a trait is not a class or a type; its members are copied into the classes that `use {name};`, \
             and an interface they implement can be the type"
        ));
    }

    /// The diagnostics from `mark` on were found in a member of trait `t`
    /// checked as a member of class `user`: each gets a note naming them, and
    /// one already reported (the same member checked for another class) is
    /// dropped.
    pub(super) fn note_trait(&mut self, mark: usize, t: ClassId, user: ClassId) {
        let note = used_by(&self.class_names[t as usize], &self.class_names[user as usize]);
        let mut i = mark;
        while i < self.diags.len() {
            let d = &self.diags[i];
            let seen = self.diags[..mark]
                .iter()
                .any(|e| e.severity == d.severity && e.message == d.message && e.span == d.span);
            if seen {
                self.diags.remove(i);
                continue;
            }
            // The innermost trait is named (one used by another is noted once).
            if t != user && !self.diags[i].notes.iter().any(|n| n.starts_with("in trait `")) {
                self.diags[i].notes.push(note.clone());
            }
            i += 1;
        }
    }

    /// The checks of a trait's own declaration, made once whether or not a
    /// class uses it. Its members are resolved and checked in each class that
    /// uses it.
    pub(super) fn check_trait_decl(&mut self, cid: ClassId, decl: &'a ClassDecl) {
        let ci = cid as usize;
        self.classes[ci].supers = HashSet::from([cid]);
        let tname = self.class_names[ci].clone();
        if let Some(case) = decl.cases.first() {
            self.error("`case` can only be used in an enum", case.span);
        }
        let mut names: HashSet<&str> = HashSet::new();
        for m in &decl.methods {
            if m.name == "__construct" {
                self.error("a trait cannot declare a constructor", m.name_span).add_note(
                    "declare the constructor in the class that uses the trait; it initializes the trait's properties",
                );
                continue;
            }
            if !names.insert(&m.name) {
                self.error(format!("method `{}` is declared more than once", m.name), m.name_span);
            }
            if m.is_abstract && m.is_static {
                self.error("an abstract method of a trait cannot be static", m.name_span);
            }
            for (n, s) in &m.type_params {
                if decl.type_params.iter().any(|(t, _)| t == n) {
                    self.error(format!("type parameter `{n}` is already a type parameter of `{tname}`"), *s);
                }
            }
        }
        let mut props: HashSet<&str> = HashSet::new();
        for p in &decl.props {
            if !props.insert(&p.name) {
                self.error(format!("property `${}` is declared more than once", p.name), p.span);
            }
        }
        let mut consts: HashSet<&str> = HashSet::new();
        for c in &decl.consts {
            if !consts.insert(&c.name) {
                self.error(format!("constant `{}` is declared more than once", c.name), c.name_span);
            }
        }
        // The traits it uses: unknown names, a trait that uses itself and
        // conflicts are reported also when no class uses it.
        if !decl.uses.is_empty() {
            self.type_params = self.classes[ci].type_params.clone();
            let mut path = vec![cid];
            // Reported once, whether found here or in a class that uses it first.
            let mark = self.diags.len();
            self.trait_members(&decl.uses, Some(cid), &mut path);
            self.note_trait(mark, cid, cid);
            self.type_params.clear();
        }
    }

    /// The members a class or a trait takes from the traits named by its
    /// `uses`, with the rules of their `{ ... }` applied. Names are looked up in
    /// the current module, and type arguments resolved with the type
    /// parameters (and trait substitution) in scope. `path` holds the traits
    /// being flattened (one that uses itself is an error).
    pub(super) fn trait_members(
        &mut self,
        uses: &'a [ast::TraitUse],
        self_class: Option<ClassId>,
        path: &mut Vec<ClassId>,
    ) -> TraitMembers<'a> {
        // Each trait used directly, where it is named, and what it gives.
        let mut used: Vec<(ClassId, Span, TraitMembers<'a>)> = Vec::new();
        for u in uses {
            for r in &u.traits {
                let Some(t) = self.class_ref(&r.name, r.span) else {
                    self.error(format!("unknown trait `{}`", r.name), r.span);
                    continue;
                };
                if !self.is_trait(t) {
                    let iface = self.classes[t as usize].kind == ClassKind::Interface;
                    let e = self.error(format!("`{}` is not a trait", r.name), r.span);
                    if iface {
                        e.add_note(format!("an interface is implemented: `implements {}`", r.name));
                    }
                    continue;
                }
                if used.iter().any(|(u, ..)| *u == t) {
                    self.error(format!("trait `{}` is used more than once", r.name), r.span);
                    continue;
                }
                let reason = self.deprecated_classes.get(&t).cloned();
                self.warn_deprecated(format!("trait `{}`", r.name), reason, None, r.span);
                let args = self.class_args(t, &r.args, self_class, r.span);
                let subst: Vec<(Name, Type)> = self.classes[t as usize].type_params.iter().cloned().zip(args).collect();
                let members = self.flatten_trait(t, subst, self_class, r.span, path);
                used.push((t, r.span, members));
            }
        }
        let mut excluded: HashSet<(ClassId, String)> = HashSet::new();
        let mut vis_change: HashMap<(ClassId, String), Visibility> = HashMap::new();
        let mut aliases: Vec<(TraitMethod<'a>, bool, ClassId, Span)> = Vec::new();
        for u in uses {
            for rule in &u.rules {
                match rule {
                    ast::TraitRule::Insteadof { from, method, excluded: others, .. } => {
                        let Some(keep) = self.rule_trait(&used, from) else { continue };
                        if !used[keep].2.has_method(&method.0) {
                            self.error(format!("trait `{}` has no method `{}`", from.0, method.0), method.1);
                            continue;
                        }
                        for o in others {
                            let Some(drop) = self.rule_trait(&used, o) else { continue };
                            if drop == keep {
                                self.error(
                                    format!(
                                        "`{}::{} insteadof {}` leaves out the method it keeps",
                                        from.0, method.0, o.0
                                    ),
                                    o.1,
                                );
                                continue;
                            }
                            if !used[drop].2.has_method(&method.0) {
                                self.error(format!("trait `{}` has no method `{}`", o.0, method.0), o.1);
                                continue;
                            }
                            excluded.insert((used[drop].0, method.0.clone()));
                        }
                    }
                    ast::TraitRule::As { from, method, vis, alias, span } => {
                        let at = match from {
                            Some(f) => {
                                let Some(i) = self.rule_trait(&used, f) else { continue };
                                if !used[i].2.has_method(&method.0) {
                                    self.error(format!("trait `{}` has no method `{}`", f.0, method.0), method.1);
                                    continue;
                                }
                                i
                            }
                            None => {
                                let hits: Vec<usize> =
                                    (0..used.len()).filter(|i| used[*i].2.has_method(&method.0)).collect();
                                match hits.as_slice() {
                                    [] => {
                                        self.error(format!("no trait used here has a method `{}`", method.0), method.1);
                                        continue;
                                    }
                                    [i] => *i,
                                    [a, b, ..] => {
                                        let (a, b) = (
                                            self.class_names[used[*a].0 as usize].clone(),
                                            self.class_names[used[*b].0 as usize].clone(),
                                        );
                                        self.error(
                                            format!("`{}` is a method of both `{a}` and `{b}`", method.0),
                                            method.1,
                                        )
                                        .add_note(format!("name the trait: `{a}::{} as ...;`", method.0));
                                        continue;
                                    }
                                }
                            }
                        };
                        let t = used[at].0;
                        match alias {
                            Some((a, _)) => {
                                let found: Vec<(TraitMethod<'a>, bool)> = used[at]
                                    .2
                                    .methods
                                    .iter()
                                    .map(|m| (m, false))
                                    .chain(used[at].2.required.iter().map(|m| (m, true)))
                                    .filter(|(m, _)| m.name == method.0)
                                    .map(|(m, abs)| (m.clone(), abs))
                                    .collect();
                                for (mut m, abs) in found {
                                    m.name = a.clone();
                                    m.vis = vis.unwrap_or(m.vis);
                                    aliases.push((m, abs, t, *span));
                                }
                            }
                            None => {
                                // The parser asks for a visibility or a name.
                                if let Some(v) = vis {
                                    vis_change.insert((t, method.0.clone()), *v);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Every method with the trait it came through and where that is named.
        let mut methods: Vec<(TraitMethod<'a>, ClassId, Span)> = Vec::new();
        let mut out = TraitMembers::default();
        for (t, span, m) in used {
            for mut tm in m.methods {
                if excluded.contains(&(t, tm.name.clone())) {
                    continue;
                }
                if let Some(v) = vis_change.get(&(t, tm.name.clone())) {
                    tm.vis = *v;
                }
                methods.push((tm, t, span));
            }
            for mut tm in m.required {
                if excluded.contains(&(t, tm.name.clone())) {
                    continue;
                }
                if let Some(v) = vis_change.get(&(t, tm.name.clone())) {
                    tm.vis = *v;
                }
                out.required.push(tm);
            }
            out.props.extend(m.props);
            out.consts.extend(m.consts);
        }
        for (m, abs, t, span) in aliases {
            if abs {
                out.required.push(m);
            } else {
                methods.push((m, t, span));
            }
        }
        for (m, t, span) in methods {
            let earlier = out.methods.iter().position(|x| x.name == m.name);
            match earlier {
                None => out.methods.push(m),
                // The same method reached along two paths (two traits using a third).
                Some(i) if std::ptr::eq(out.methods[i].decl, m.decl) && out.methods[i].vis == m.vis => {}
                Some(i) => {
                    let a = self.class_names[out.methods[i].origin as usize].clone();
                    let b = self.class_names[t as usize].clone();
                    let name = m.name.clone();
                    let a = if a == b { self.class_names[m.origin as usize].clone() } else { a };
                    self.error(format!("method `{name}` comes from both trait `{a}` and trait `{b}`"), span).add_note(
                        format!(
                            "pick one in the `use`: `{{ {a}::{name} insteadof {b}; }}`, and keep the other under \
                             another name if needed: `{b}::{name} as {name}{b};`"
                        ),
                    );
                }
            }
        }
        out
    }

    /// The index in `used` of the trait a rule names (`A` in `A::m`).
    fn rule_trait(&mut self, used: &[(ClassId, Span, TraitMembers<'a>)], name: &(String, Span)) -> Option<usize> {
        let t = self.class_ref(&name.0, name.1);
        let found = t.and_then(|t| used.iter().position(|(u, ..)| *u == t));
        if found.is_none() {
            self.error(format!("trait `{}` is not used here", name.0), name.1)
                .add_note("a rule can name only the traits of this class's `use`");
        }
        found
    }

    /// The members trait `t` gives (its own and those of the traits it
    /// uses), its type parameters standing for `subst`.
    fn flatten_trait(
        &mut self,
        t: ClassId,
        subst: Vec<(Name, Type)>,
        self_class: Option<ClassId>,
        span: Span,
        path: &mut Vec<ClassId>,
    ) -> TraitMembers<'a> {
        let name = self.class_names[t as usize].clone();
        if path.contains(&t) {
            self.error(format!("trait `{name}` uses itself"), span);
            return TraitMembers::default();
        }
        if path.len() >= MAX_CLASS_DEPTH {
            self.error(format!("traits are used inside each other more than {MAX_CLASS_DEPTH} levels deep"), span);
            return TraitMembers::default();
        }
        let Some(decl) = self.classes[t as usize].decl else { return TraitMembers::default() };
        // Each class holds a copy of the members of the traits it uses.
        if self.over_inherit_limit(decl.methods.len() + decl.props.len() + decl.consts.len(), decl) {
            return TraitMembers::default();
        }
        // And each copy of a method is checked and compiled: their source is
        // limited as a program's is.
        let code: usize =
            decl.methods.iter().filter(|m| !m.is_abstract).map(|m| (m.span.end - m.span.start) as usize).sum();
        self.trait_code = self.trait_code.saturating_add(code);
        if self.trait_code > MAX_TRAIT_CODE {
            if !self.reported_trait_code {
                self.reported_trait_code = true;
                self.error(
                    format!(
                        "the traits of this program copy more than {} MB of methods into the classes that use them",
                        MAX_TRAIT_CODE >> 20
                    ),
                    span,
                )
                .add_note(
                    "a trait's methods are checked and compiled in each class that uses it; put large shared code \
                     in a class or a function that the trait's methods call",
                );
            }
            return TraitMembers::default();
        }
        path.push(t);
        let module = std::mem::replace(&mut self.module, self.classes[t as usize].module);
        let outer_subst = std::mem::replace(&mut self.trait_subst, subst.clone());
        let outer_tps = std::mem::take(&mut self.type_params);
        let mark = self.diags.len();
        let mut out = self.trait_members(&decl.uses, self_class, path);
        if let Some(user) = self_class {
            self.note_trait(mark, t, user);
        }
        self.type_params = outer_tps;
        self.trait_subst = outer_subst;
        self.module = module;
        path.pop();
        // A trait's own methods win over those of the traits it uses.
        for m in decl.methods.iter().filter(|m| m.name != "__construct") {
            out.methods.retain(|x| x.name != m.name);
            let tm = TraitMethod { decl: m, name: m.name.clone(), vis: m.visibility, origin: t, subst: subst.clone() };
            if m.is_abstract {
                out.required.push(tm);
            } else {
                out.methods.push(tm);
            }
        }
        out.props.extend(decl.props.iter().map(|p| TraitProp { decl: p, origin: t, subst: subst.clone() }));
        out.consts.extend(decl.consts.iter().map(|c| TraitConst { decl: c, origin: t }));
        out
    }

    /// Runs `f` as in trait `t`'s module, with its type parameters standing
    /// for `subst` (unless `is_static`: a static method does not see them),
    /// and notes the diagnostics it gives as found in `t` used by `user`.
    pub(super) fn in_trait<R>(
        &mut self,
        t: ClassId,
        subst: &[(Name, Type)],
        is_static: bool,
        user: ClassId,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mark = self.diags.len();
        let module = std::mem::replace(&mut self.module, self.classes[t as usize].module);
        let substituted = if is_static { Vec::new() } else { subst.to_vec() };
        let outer = std::mem::replace(&mut self.trait_subst, substituted);
        let r = f(self);
        self.trait_subst = outer;
        self.module = module;
        self.note_trait(mark, t, user);
        r
    }

    /// Adds the constants class `cid` takes from traits (after its own):
    /// one of the same name as another must have the same value and type.
    pub(super) fn add_trait_consts(&mut self, cid: ClassId, consts: &[TraitConst<'a>]) {
        let ci = cid as usize;
        let mut from_trait: HashMap<&str, ClassId> = HashMap::new();
        for tc in consts {
            let c = tc.decl;
            let value = self.in_trait(tc.origin, &[], true, cid, |this| {
                this.annotations(&c.attrs, AnnTarget::Const);
                this.literal_const(&c.value, c.ty.as_ref(), Some(cid))
            });
            let Some(value) = value else { continue };
            if self.classes[ci].const_spans.contains_key(&c.name) {
                if self.classes[ci].consts.get(&c.name) != Some(&value) {
                    let other = match from_trait.get(c.name.as_str()) {
                        Some(o) => format!("trait `{}`", self.class_names[*o as usize]),
                        None => format!("`{}`", self.class_names[ci]),
                    };
                    let mark = self.diags.len();
                    self.error(
                        format!("constant `{}` of this trait is not the same as the one of {other}", c.name),
                        c.name_span,
                    )
                    .add_note("a class and the traits it uses may declare a constant of the same name only with the same value");
                    self.note_trait(mark, tc.origin, cid);
                }
                continue;
            }
            from_trait.insert(&c.name, tc.origin);
            self.classes[ci].const_spans.insert(c.name.clone(), c.name_span);
            self.classes[ci].consts.insert(c.name.clone(), value);
        }
    }

    /// Adds the properties class `cid` takes from traits (after its own): one
    /// of the same name as another of the class must be the same (type,
    /// visibility, `readonly` and default), as in PHP; one an ancestor
    /// declares is an error, as for any property.
    pub(super) fn add_trait_props(&mut self, cid: ClassId, props: &[TraitProp<'a>]) {
        let ci = cid as usize;
        let mut from_trait: HashMap<&str, ClassId> = HashMap::new();
        for tp in props {
            let p = tp.decl;
            let (ty, default) = self.in_trait(tp.origin, &tp.subst, false, cid, |this| {
                let ann = this.annotations(&p.attrs, AnnTarget::Property);
                if let Some(r) = ann.deprecated {
                    this.deprecated_fields.insert((cid, p.name.clone()), r);
                }
                let ty = this.resolve_type(&p.ty, Some(cid));
                let default = p.default.as_ref().and_then(|d| this.const_default(d, &ty, Some(cid)));
                (ty, default)
            });
            let existing = self.classes[ci].field_slots.get(&p.name).map(|s| &self.classes[ci].fields[*s as usize]);
            if let Some(f) = existing.filter(|f| f.owner == cid) {
                let same = f.ty == ty && f.vis == p.visibility && f.readonly == p.readonly && f.default == default;
                if !same {
                    let other = match from_trait.get(p.name.as_str()) {
                        Some(o) => format!("trait `{}`", self.class_names[*o as usize]),
                        None => format!("`{}`", self.class_names[ci]),
                    };
                    let mark = self.diags.len();
                    self.error(
                        format!("property `${}` of this trait is not the same as the one of {other}", p.name),
                        p.span,
                    )
                    .add_note(
                        "a class and the traits it uses may declare a property of the same name only with the same \
                         type, visibility, `readonly` and default value",
                    );
                    self.note_trait(mark, tp.origin, cid);
                }
                continue;
            }
            from_trait.insert(&p.name, tp.origin);
            let required = !ty.is_nullable() && default.is_none() && ty != Type::Error;
            let mark = self.diags.len();
            self.add_field(
                cid,
                FieldInfo {
                    name: p.name.clone(),
                    ty,
                    vis: p.visibility,
                    readonly: p.readonly,
                    owner: cid,
                    default,
                    required,
                    span: p.span,
                },
            );
            self.note_trait(mark, tp.origin, cid);
        }
    }

    /// The abstract methods of the traits class `cid` uses: each is met by the
    /// class's method of that name (its own, a trait's or an inherited one),
    /// which must be compatible; one that is not there is added as an
    /// abstract method of the class (so a concrete class reports it missing).
    pub(super) fn add_trait_requirements(&mut self, cid: ClassId, required: &[TraitMethod<'a>]) {
        let ci = cid as usize;
        let mut done: Vec<(&FunctionDecl, &str)> = Vec::new();
        for r in required {
            if done.iter().any(|(d, n)| std::ptr::eq(*d, r.decl) && *n == r.name) {
                continue;
            }
            done.push((r.decl, &r.name));
            let m = r.decl;
            let tname = self.class_names[r.origin as usize].clone();
            let fid = self.in_trait(r.origin, &r.subst, m.is_static, cid, |this| {
                let type_params: Vec<Name> = m.type_params.iter().map(|(n, _)| Name::from(n.as_str())).collect();
                this.type_params = type_params.clone();
                this.set_bounds(None, &[], &[]);
                let bounds = this.resolve_bounds(&m.type_bounds, Some(cid));
                this.set_bounds(None, &type_params, &bounds);
                let params = this.resolve_params(&m.params, Some(cid));
                let ret = match &m.ret {
                    Some(t) => this.resolve_type(t, Some(cid)),
                    None => Type::Void,
                };
                this.type_params.clear();
                this.bounds.clear();
                let fid = this.add_func(FuncInfo {
                    name: format!("{tname}::{}", r.name),
                    type_params,
                    is_abstract: true,
                    params,
                    ret,
                    body: Body::Function(m),
                    module: this.module,
                    class: Some(cid),
                    is_static: m.is_static,
                    kind: FuncKind::Method,
                    span: m.name_span,
                });
                if bounds.iter().any(Option::is_some) {
                    this.func_bounds.insert(fid, bounds);
                }
                fid
            });
            self.func_trait.insert(fid, (r.origin, r.subst.clone()));
            let cname = self.class_names[ci].clone();
            match self.classes[ci].methods.get(&r.name).cloned() {
                Some(have) => {
                    let own = self.funcs[have.func as usize].class == Some(cid);
                    let span = if own { self.funcs[have.func as usize].span } else { self.classes[ci].span };
                    let mark = self.diags.len();
                    if have.is_static != m.is_static {
                        self.error(
                            format!(
                                "`{cname}::{}` changes between static and non-static the abstract method `{tname}::{}`",
                                r.name, r.name
                            ),
                            span,
                        );
                    } else if !self.compatible_override(have.func, fid, cid) {
                        let sig = self.signature(fid);
                        self.error(
                            format!("`{cname}::{}` does not match the signature of abstract method `{tname}::{}`", r.name, r.name),
                            span,
                        )
                        .add_note(format!(
                            "trait `{tname}` declares it `{}`; the method must keep its parameter types and a compatible return type",
                            sig
                        ));
                    }
                    // Reported once, also when several classes use the trait.
                    let mut i = mark;
                    while i < self.diags.len() {
                        let d = &self.diags[i];
                        if self.diags[..mark].iter().any(|e| e.message == d.message && e.span == d.span) {
                            self.diags.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                }
                None => {
                    let c = &mut self.classes[ci];
                    let slot = if m.is_static {
                        None
                    } else {
                        c.vtable.push(fid);
                        Some(c.vtable.len() as u32 - 1)
                    };
                    c.methods.insert(
                        r.name.clone(),
                        MethodInfo { func: fid, slot, vis: r.vis, is_static: m.is_static, owner: cid },
                    );
                }
            }
        }
    }
}
