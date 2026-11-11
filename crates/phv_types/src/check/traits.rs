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
#[derive(Default, Clone)]
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
    /// How many members it holds (what a copy of it costs).
    fn size(&self) -> usize {
        self.methods.len() + self.required.len() + self.props.len() + self.consts.len()
    }
}

/// The methods of the traits a `use` names, by name, for its rules: a rule
/// finds what it names at once, however many rules and methods there are.
/// (A scan of the trait's methods per rule made `N` aliases take O(N²):
/// 40,000 took 13 s to check, and 17 levels of traits that each alias every
/// method of the one below took 43 s.)
struct UsedIndex<'u> {
    /// Where each trait is in `used`.
    at: HashMap<ClassId, usize>,
    /// The methods (false) and abstract methods (true) of `used[i]` named
    /// `n`, by `(i, n)`, as positions in its `methods` and `required`.
    members: HashMap<(usize, &'u str), Vec<(usize, bool)>>,
    /// The first two traits of `used` (in order) with a method of the name.
    owners: HashMap<&'u str, (usize, Option<usize>)>,
}

impl<'u> UsedIndex<'u> {
    fn new(used: &'u [(ClassId, Span, TraitMembers<'_>)]) -> UsedIndex<'u> {
        let mut index = UsedIndex { at: HashMap::new(), members: HashMap::new(), owners: HashMap::new() };
        for (i, (t, _, m)) in used.iter().enumerate() {
            index.at.insert(*t, i);
            let methods = m.methods.iter().enumerate().map(|(j, x)| (j, false, x));
            let required = m.required.iter().enumerate().map(|(j, x)| (j, true, x));
            for (j, abs, x) in methods.chain(required) {
                index.members.entry((i, x.name.as_str())).or_default().push((j, abs));
                let owners = index.owners.entry(x.name.as_str()).or_insert((i, None));
                if owners.0 != i && owners.1.is_none() {
                    owners.1 = Some(i);
                }
            }
        }
        index
    }

    /// Whether `used[i]` has a method (or an abstract one) named `name`.
    fn has_method(&self, i: usize, name: &str) -> bool {
        self.members.contains_key(&(i, name))
    }
}

/// What a trait gives, by the trait, what its type parameters stand for and
/// the class (or trait) whose `use` is flattened (`self` in a type argument
/// is that class): `None` once it was flattened, the members from the second
/// time on (see `Checker::flatten_trait`). Emptied after each class.
pub(super) type TraitMemo<'a> = HashMap<(ClassId, Vec<(Name, Type)>, Option<ClassId>), Option<TraitMembers<'a>>>;

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
        if mark >= self.diags.len() {
            return;
        }
        let note = (t != user).then(|| used_by(&self.class_names[t as usize], &self.class_names[user as usize]));
        self.drop_repeated(mark, note);
    }

    /// Drops the diagnostics from `mark` on that say what one before `mark`
    /// says at the same place, and adds `note` to the others (unless they
    /// name a trait already: the innermost one is named). Linear in the
    /// diagnostics looked at: a scan of the earlier ones for each made a
    /// trait whose 40,000 rules each gave an error take 10 s to check
    /// (audit 20). The earlier ones are indexed as calls reach them, and
    /// again only when a call looks at a shorter prefix (a call drops only
    /// diagnostics from its `mark` on, so the index of a prefix no longer
    /// than that stays right).
    fn drop_repeated(&mut self, mark: usize, note: Option<String>) {
        if self.seen_len > mark {
            self.seen_diags.clear();
            self.seen_len = 0;
        }
        for d in &self.diags[self.seen_len..mark] {
            self.seen_diags.insert((d.is_error(), d.message.clone(), d.span));
        }
        self.seen_len = mark;
        for mut d in self.diags.split_off(mark) {
            if self.seen_diags.contains(&(d.is_error(), d.message.clone(), d.span)) {
                continue;
            }
            if let Some(note) = &note {
                if !d.notes.iter().any(|n| n.starts_with("in trait `")) {
                    d.notes.push(note.clone());
                }
            }
            self.diags.push(d);
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
            self.trait_memo.clear();
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
        // Each rule counts as a member copied (an alias makes one), so that
        // rules repeated along nested traits stop at the limit.
        let rules: usize = uses.iter().map(|u| u.rules.len()).sum();
        if rules > 0 && self.over_inherit_limit_at(rules, uses[0].span) {
            return TraitMembers::default();
        }
        let index = UsedIndex::new(&used);
        for u in uses {
            for rule in &u.rules {
                match rule {
                    ast::TraitRule::Insteadof { from, method, excluded: others, .. } => {
                        let Some(keep) = self.rule_trait(&index, from) else { continue };
                        if !index.has_method(keep, &method.0) {
                            self.error(format!("trait `{}` has no method `{}`", from.0, method.0), method.1);
                            continue;
                        }
                        for o in others {
                            let Some(drop) = self.rule_trait(&index, o) else { continue };
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
                            if !index.has_method(drop, &method.0) {
                                self.error(format!("trait `{}` has no method `{}`", o.0, method.0), o.1);
                                continue;
                            }
                            excluded.insert((used[drop].0, method.0.clone()));
                        }
                    }
                    ast::TraitRule::As { from, method, vis, alias, span } => {
                        let at = match from {
                            Some(f) => {
                                let Some(i) = self.rule_trait(&index, f) else { continue };
                                if !index.has_method(i, &method.0) {
                                    self.error(format!("trait `{}` has no method `{}`", f.0, method.0), method.1);
                                    continue;
                                }
                                i
                            }
                            None => match index.owners.get(method.0.as_str()).copied() {
                                None => {
                                    self.error(format!("no trait used here has a method `{}`", method.0), method.1);
                                    continue;
                                }
                                Some((i, None)) => i,
                                Some((a, Some(b))) => {
                                    let (a, b) = (
                                        self.class_names[used[a].0 as usize].clone(),
                                        self.class_names[used[b].0 as usize].clone(),
                                    );
                                    self.error(format!("`{}` is a method of both `{a}` and `{b}`", method.0), method.1)
                                        .add_note(format!("name the trait: `{a}::{} as ...;`", method.0));
                                    continue;
                                }
                            },
                        };
                        let t = used[at].0;
                        match alias {
                            Some((a, _)) => {
                                let given = &used[at].2;
                                let found = index.members.get(&(at, method.0.as_str())).map_or(&[][..], |v| &v[..]);
                                for &(j, abs) in found {
                                    let mut m = if abs { given.required[j].clone() } else { given.methods[j].clone() };
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
        drop(index);
        // Every method with the trait it came through and where that is named.
        let mut methods: Vec<(TraitMethod<'a>, ClassId, Span)> = Vec::new();
        let mut out = TraitMembers::default();
        // A member reached along two paths (two traits using a third) is kept
        // once: a trait at the bottom of `n` levels of such "diamonds" is
        // reached along 2^n paths.
        let mut required: HashSet<(*const FunctionDecl, String, u8)> = HashSet::new();
        let mut props: HashSet<(*const ast::PropDecl, Vec<(Name, Type)>)> = HashSet::new();
        let mut consts: HashSet<*const ast::ConstDecl> = HashSet::new();
        let mut add_required = |out: &mut TraitMembers<'a>, tm: TraitMethod<'a>| {
            if required.insert((tm.decl as *const FunctionDecl, tm.name.clone(), vis_rank(tm.vis))) {
                out.required.push(tm);
            }
        };
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
                add_required(&mut out, tm);
            }
            for p in m.props {
                if props.insert((p.decl as *const ast::PropDecl, p.subst.clone())) {
                    out.props.push(p);
                }
            }
            for c in m.consts {
                if consts.insert(c.decl as *const ast::ConstDecl) {
                    out.consts.push(c);
                }
            }
        }
        for (m, abs, t, span) in aliases {
            if abs {
                add_required(&mut out, m);
            } else {
                methods.push((m, t, span));
            }
        }
        let mut at: HashMap<String, usize> = HashMap::new();
        for (m, t, span) in methods {
            let earlier = at.get(&m.name).copied();
            match earlier {
                None => {
                    at.insert(m.name.clone(), out.methods.len());
                    out.methods.push(m);
                }
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
    fn rule_trait(&mut self, index: &UsedIndex<'_>, name: &(String, Span)) -> Option<usize> {
        let t = self.class_ref(&name.0, name.1);
        let found = t.and_then(|t| index.at.get(&t).copied());
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
        // A trait reached again along another path: what it gave the second
        // time is kept, and given from the third time on. (What it gives the
        // first time is not kept, so that a chain of traits does not hold a
        // copy per level.) Flattened at most twice per class, the traits cost
        // in proportion to their `use` lists, whatever the number of paths.
        let key = (t, subst, self_class);
        let seen = match self.trait_memo.get(&key) {
            Some(Some(kept)) => {
                let kept = kept.clone();
                // Each flatten counts, the copy as the members it holds.
                if self.over_inherit_limit(1 + kept.size(), decl) {
                    return TraitMembers::default();
                }
                return kept;
            }
            Some(None) => true,
            None => false,
        };
        let subst = key.1.clone();
        // Each class holds a copy of the members of the traits it uses, and
        // each flatten counts (one of a trait without members too).
        if self.over_inherit_limit(1 + decl.methods.len() + decl.props.len() + decl.consts.len(), decl) {
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
        let own: HashSet<&str> =
            decl.methods.iter().filter(|m| m.name != "__construct").map(|m| m.name.as_str()).collect();
        out.methods.retain(|x| !own.contains(x.name.as_str()));
        for m in decl.methods.iter().filter(|m| m.name != "__construct") {
            let tm = TraitMethod { decl: m, name: m.name.clone(), vis: m.visibility, origin: t, subst: subst.clone() };
            if m.is_abstract {
                out.required.push(tm);
            } else {
                out.methods.push(tm);
            }
        }
        out.props.extend(decl.props.iter().map(|p| TraitProp { decl: p, origin: t, subst: subst.clone() }));
        out.consts.extend(decl.consts.iter().map(|c| TraitConst { decl: c, origin: t }));
        let kept = seen.then(|| out.clone());
        self.trait_memo.insert(key, kept);
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
    /// visibility, `readonly`, default, and the column and key std.orm reads
    /// from `#[Column]` and `#[Id]`), as in PHP; one an ancestor declares is
    /// an error, as for any property. Its annotations are the class's.
    pub(super) fn add_trait_props(&mut self, cid: ClassId, props: &[TraitProp<'a>]) {
        let ci = cid as usize;
        let mut from_trait: HashMap<&str, ClassId> = HashMap::new();
        for tp in props {
            let p = tp.decl;
            let (ty, default, ann) = self.in_trait(tp.origin, &tp.subst, false, cid, |this| {
                let ann = this.annotations(&p.attrs, AnnTarget::Property);
                let ty = this.resolve_type(&p.ty, Some(cid));
                let default = p.default.as_ref().and_then(|d| this.const_default(d, &ty, Some(cid)));
                (ty, default, ann)
            });
            let key = (cid, p.name.clone());
            if let Some(r) = ann.deprecated.clone() {
                self.deprecated_fields.insert(key.clone(), r);
            }
            let existing = self.classes[ci].field_slots.get(&p.name).map(|s| &self.classes[ci].fields[*s as usize]);
            if let Some(f) = existing.filter(|f| f.owner == cid) {
                let same = f.ty == ty
                    && f.vis == p.visibility
                    && f.readonly == p.readonly
                    && f.default == default
                    && self.orm_columns.get(&key) == ann.column.as_ref()
                    && self.orm_ids.contains(&key) == ann.id;
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
                         type, visibility, `readonly`, default value, `#[Column]` and `#[Id]`",
                    );
                    self.note_trait(mark, tp.origin, cid);
                }
                continue;
            }
            from_trait.insert(&p.name, tp.origin);
            // `#[Id]` and `#[Column]` describe the class's property, as the
            // class's own do (std.orm reads them for its key and columns).
            if let Some(c) = ann.column {
                self.orm_columns.insert(key.clone(), c);
            }
            if ann.id {
                self.orm_ids.insert(key);
            }
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
        // Each (declaration, name) once, found at once whatever their number.
        let mut done: HashSet<(*const FunctionDecl, &str)> = HashSet::new();
        for r in required {
            if !done.insert((r.decl as *const FunctionDecl, &r.name)) {
                continue;
            }
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
                    if mark < self.diags.len() {
                        self.drop_repeated(mark, None);
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
