use std::fmt;

pub type ClassId = u32;
pub type FuncId = u32;
pub type LocalId = u32;

/// The name of a type parameter: shared, so types that mention it are copied
/// without copying the name.
pub type Name = std::sync::Arc<str>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Type {
    Int,
    Float,
    Str,
    Bool,
    Null,
    Void,
    /// The type of `throw` expressions: never produces a value.
    Never,
    Html,
    /// `Html` that is also safe at the start of a URL attribute (from `safe_url()`).
    SafeUrl,
    /// `Html` that is also a safe JavaScript value inside `<script>` (from `json_html()`).
    SafeJs,
    /// Top type; values must be narrowed with `is` before use.
    Any,
    /// Produced after a reported error, to avoid cascading messages.
    Error,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// `set<T>`: distinct `int` or `string` values.
    Set(Box<Type>),
    /// A class or interface with its type arguments (empty unless it is generic).
    Class(ClassId, Vec<Type>),
    Func(Vec<Type>, Box<Type>),
    /// Normalized: flat, sorted, deduplicated, at least two members.
    Union(Vec<Type>),
    /// A type parameter in a built-in signature (`T`, `U`, `K`, `V`), or a
    /// user type parameter while a call is being inferred.
    Generic(u8),
    /// A user type parameter inside its generic function (opaque).
    Param(Name),
    /// A parameterized SQL query built with `sql"..."`.
    Sql,
    /// "Not known yet" inside an expected type, e.g. a closure's return type.
    Infer,
    /// A type argument that is not known: `is Box` and `catch` prove `Box<?>`
    /// only. A member read through it is `any`; nothing can be written
    /// through it (it accepts only `never`), and it is the same as no other
    /// type, itself included, so the value cannot be passed on as a `Box<T>`.
    Unknown,
}

impl Type {
    /// `Html` or one of its more specific forms.
    pub fn is_html(&self) -> bool {
        matches!(self, Type::Html | Type::SafeUrl | Type::SafeJs)
    }

    /// A class that takes no type arguments.
    pub fn class(c: ClassId) -> Type {
        Type::Class(c, Vec::new())
    }

    pub fn list(t: Type) -> Type {
        Type::List(Box::new(t))
    }

    pub fn map(k: Type, v: Type) -> Type {
        Type::Map(Box::new(k), Box::new(v))
    }

    pub fn set(t: Type) -> Type {
        Type::Set(Box::new(t))
    }

    pub fn union(members: Vec<Type>) -> Type {
        let mut flat = Vec::new();
        for m in members {
            match m {
                Type::Union(ms) => flat.extend(ms),
                Type::Never => {}
                other => flat.push(other),
            }
        }
        if flat.contains(&Type::Error) {
            return Type::Error;
        }
        if flat.contains(&Type::Any) {
            return Type::Any;
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Type::Never,
            1 => flat.pop().unwrap(),
            _ => Type::Union(flat),
        }
    }

    pub fn nullable(t: Type) -> Type {
        Type::union(vec![t, Type::Null])
    }

    pub fn members(&self) -> Vec<Type> {
        match self {
            Type::Union(ms) => ms.clone(),
            Type::Never => Vec::new(),
            other => vec![other.clone()],
        }
    }

    pub fn is_nullable(&self) -> bool {
        match self {
            Type::Null => true,
            Type::Union(ms) => ms.contains(&Type::Null),
            _ => false,
        }
    }

    pub fn without_null(&self) -> Type {
        match self {
            Type::Union(ms) => Type::union(ms.iter().filter(|m| **m != Type::Null).cloned().collect()),
            Type::Null => Type::Never,
            other => other.clone(),
        }
    }

    pub fn contains(&self, t: &Type) -> bool {
        match self {
            Type::Union(ms) => ms.contains(t),
            other => other == t,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    /// Whether a type argument in `self` is `Unknown`.
    pub fn is_open(&self) -> bool {
        match self {
            Type::Unknown => true,
            Type::List(t) | Type::Set(t) => t.is_open(),
            Type::Map(k, v) => k.is_open() || v.is_open(),
            Type::Func(ps, r) => ps.iter().any(|p| p.is_open()) || r.is_open(),
            Type::Union(ms) => ms.iter().any(|m| m.is_open()),
            Type::Class(_, args) => args.iter().any(|a| a.is_open()),
            _ => false,
        }
    }

    /// A member's type read through a receiver with unknown type arguments:
    /// where a value comes out (`read`, e.g. a property read or a return
    /// value) an unknown type is `any`; where one goes in (a property write,
    /// a parameter) it is `never`. Type arguments of classes stay unknown.
    pub fn resolve_open(&self, read: bool) -> Type {
        if !self.is_open() {
            return self.clone();
        }
        match self {
            Type::Unknown if read => Type::Any,
            Type::Unknown => Type::Never,
            Type::List(t) => Type::list(t.resolve_open(read)),
            Type::Set(t) => Type::set(t.resolve_open(read)),
            Type::Map(k, v) => Type::map(k.resolve_open(read), v.resolve_open(read)),
            // A closure's parameters receive values: the other way round.
            Type::Func(ps, r) => {
                Type::Func(ps.iter().map(|p| p.resolve_open(!read)).collect(), Box::new(r.resolve_open(read)))
            }
            Type::Union(ms) => Type::union(ms.iter().map(|m| m.resolve_open(read)).collect()),
            other => other.clone(),
        }
    }

    pub fn has_unknown(&self) -> bool {
        match self {
            Type::Infer | Type::Generic(_) => true,
            Type::List(t) | Type::Set(t) => t.has_unknown(),
            Type::Map(k, v) => k.has_unknown() || v.has_unknown(),
            Type::Func(ps, r) => ps.iter().any(|p| p.has_unknown()) || r.has_unknown(),
            Type::Union(ms) => ms.iter().any(|m| m.has_unknown()),
            Type::Class(_, args) => args.iter().any(|a| a.has_unknown()),
            _ => false,
        }
    }

    pub fn display<'a>(&'a self, class_names: &'a [String]) -> TypeDisplay<'a> {
        TypeDisplay { ty: self, names: class_names, depth: 0 }
    }
}

/// Types nested deeper than this are shown as `...` (an inferred type may be
/// thousands of levels deep, and each level is a call here).
const MAX_DISPLAY_DEPTH: u32 = 64;

pub struct TypeDisplay<'a> {
    ty: &'a Type,
    names: &'a [String],
    depth: u32,
}

impl<'a> TypeDisplay<'a> {
    fn sub(&self, ty: &'a Type) -> TypeDisplay<'a> {
        TypeDisplay { ty, names: self.names, depth: self.depth + 1 }
    }
}

impl fmt::Display for TypeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.depth >= MAX_DISPLAY_DEPTH {
            return write!(f, "...");
        }
        match self.ty {
            Type::Int => write!(f, "int"),
            Type::Float => write!(f, "float"),
            Type::Str => write!(f, "string"),
            Type::Bool => write!(f, "bool"),
            Type::Null => write!(f, "null"),
            Type::Void => write!(f, "void"),
            Type::Never => write!(f, "never"),
            Type::Html => write!(f, "Html"),
            Type::SafeUrl => write!(f, "SafeUrl"),
            Type::SafeJs => write!(f, "SafeJs"),
            Type::Any => write!(f, "any"),
            Type::Unknown => write!(f, "?"),
            Type::Error => write!(f, "{{unknown}}"),
            Type::List(t) => write!(f, "list<{}>", self.sub(t)),
            Type::Set(t) => write!(f, "set<{}>", self.sub(t)),
            Type::Map(k, v) => write!(f, "map<{}, {}>", self.sub(k), self.sub(v)),
            Type::Class(c, args) => {
                write!(f, "{}", self.names.get(*c as usize).map(|s| s.as_str()).unwrap_or("?"))?;
                if !args.is_empty() {
                    write!(f, "<")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", self.sub(a))?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            Type::Func(ps, r) => {
                write!(f, "fn(")?;
                for (i, p) in ps.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", self.sub(p))?;
                }
                write!(f, "): {}", self.sub(r))
            }
            Type::Union(ms) => {
                let non_null: Vec<&Type> = ms.iter().filter(|m| **m != Type::Null).collect();
                if non_null.len() == 1 && ms.len() == 2 {
                    let inner = non_null[0];
                    if matches!(inner, Type::Func(..)) {
                        return write!(f, "?({})", self.sub(inner));
                    }
                    return write!(f, "?{}", self.sub(inner));
                }
                for (i, m) in ms.iter().enumerate() {
                    if i > 0 {
                        write!(f, "|")?;
                    }
                    write!(f, "{}", self.sub(m))?;
                }
                Ok(())
            }
            Type::Generic(i) => write!(f, "{}", ["T", "U", "K", "V"].get(*i as usize).copied().unwrap_or("T")),
            Type::Param(n) => write!(f, "{n}"),
            Type::Sql => write!(f, "Sql"),
            Type::Infer => write!(f, "_"),
        }
    }
}
