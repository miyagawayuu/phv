//! The signatures of the builtins (`builtins::BUILTINS`), read into types.
//!
//! A signature is a PHV function type written in a small part of the type
//! syntax: names with type arguments (`list<T>`, `map<K, V>`), `?T`, unions
//! `A | B`, parentheses and `fn(A, B): R`. It is read here straight into a
//! `Type`, by the grammar of `Parser::ty`, instead of through the lexer and
//! parser of `phv_syntax`: that took about half a millisecond for all of them
//! at the start of every process (a `phv check` takes a few). A test reads
//! every builtin both ways and compares the results.

use crate::types::Type;

/// The type of the signature `sig`. Panics on anything else than the syntax
/// above or a name `named` knows: the table is part of the program.
pub(super) fn parse(sig: &str) -> Type {
    let mut r = Reader { s: sig.as_bytes(), at: 0, sig };
    let t = r.ty();
    r.space();
    if r.at != r.s.len() {
        r.bad();
    }
    t
}

/// The type a name with its type arguments stands for in a signature.
/// `T`, `U`, `K`, `V` and `W` are the type parameters.
fn named(name: &str, args: Vec<Type>) -> Type {
    let mut args = args.into_iter();
    let (a, b, c) = (args.next(), args.next(), args.next());
    match (name, a, b, c) {
        ("int", None, ..) => Type::Int,
        ("float", None, ..) => Type::Float,
        ("string", None, ..) => Type::Str,
        ("bool", None, ..) => Type::Bool,
        ("void", None, ..) => Type::Void,
        ("any", None, ..) => Type::Any,
        ("Html", None, ..) => Type::Html,
        ("SafeUrl", None, ..) => Type::SafeUrl,
        ("SafeJs", None, ..) => Type::SafeJs,
        ("Sql", None, ..) => Type::Sql,
        ("T", None, ..) => Type::Generic(0),
        ("U", None, ..) => Type::Generic(1),
        ("K", None, ..) => Type::Generic(2),
        ("V", None, ..) => Type::Generic(3),
        ("W", None, ..) => Type::Generic(4),
        ("list", Some(a), None, _) => Type::list(a),
        ("set", Some(a), None, _) => Type::set(a),
        ("map", Some(k), Some(v), None) => Type::map(k, v),
        _ => panic!("bad builtin signature type `{name}`"),
    }
}

struct Reader<'s> {
    s: &'s [u8],
    at: usize,
    sig: &'s str,
}

impl Reader<'_> {
    fn bad(&self) -> ! {
        panic!("bad builtin signature: {}", self.sig)
    }

    fn space(&mut self) {
        while self.s.get(self.at).is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    /// The next character that is not a space.
    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.s.get(self.at).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) {
        if !self.eat(c) {
            self.bad();
        }
    }

    /// `A`, or the union `A | B | ...`.
    fn ty(&mut self) -> Type {
        let first = self.atom();
        if self.peek() != Some(b'|') {
            return first;
        }
        let mut members = vec![first];
        while self.eat(b'|') {
            members.push(self.atom());
        }
        Type::union(members)
    }

    fn atom(&mut self) -> Type {
        match self.peek() {
            Some(b'?') => {
                self.at += 1;
                Type::nullable(self.atom())
            }
            Some(b'(') => {
                self.at += 1;
                let t = self.ty();
                self.expect(b')');
                t
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                    self.at += 1;
                }
                let name = &self.sig[start..self.at];
                if name == "fn" {
                    self.expect(b'(');
                    let mut params = Vec::new();
                    while self.peek() != Some(b')') {
                        params.push(self.ty());
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.expect(b')');
                    self.expect(b':');
                    let ret = self.atom();
                    return Type::Func(params, Box::new(ret));
                }
                let mut args = Vec::new();
                if self.eat(b'<') {
                    loop {
                        args.push(self.ty());
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.expect(b'>');
                }
                named(name, args)
            }
            _ => self.bad(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::BUILTINS;
    use phv_syntax::ast::{TypeExpr, TypeExprKind};
    use phv_syntax::parse_type_str;

    /// A signature read by the parser of the language, as the checker did
    /// before `parse`: the reference `parse` is compared with.
    fn by_parser(sig: &str) -> Type {
        fn sig_type(t: &TypeExpr) -> Type {
            match &t.kind {
                TypeExprKind::Named(n, args) => match (n.as_str(), args.as_slice()) {
                    ("int", []) => Type::Int,
                    ("float", []) => Type::Float,
                    ("string", []) => Type::Str,
                    ("bool", []) => Type::Bool,
                    ("void", []) => Type::Void,
                    ("any", []) => Type::Any,
                    ("Html", []) => Type::Html,
                    ("SafeUrl", []) => Type::SafeUrl,
                    ("SafeJs", []) => Type::SafeJs,
                    ("Sql", []) => Type::Sql,
                    ("T", []) => Type::Generic(0),
                    ("U", []) => Type::Generic(1),
                    ("K", []) => Type::Generic(2),
                    ("V", []) => Type::Generic(3),
                    ("W", []) => Type::Generic(4),
                    ("list", [a]) => Type::list(sig_type(a)),
                    ("set", [a]) => Type::set(sig_type(a)),
                    ("map", [k, v]) => Type::map(sig_type(k), sig_type(v)),
                    _ => panic!("bad builtin signature type `{n}`"),
                },
                TypeExprKind::Nullable(inner) => Type::nullable(sig_type(inner)),
                TypeExprKind::Union(ms) => Type::union(ms.iter().map(sig_type).collect()),
                TypeExprKind::Func(ps, r) => Type::Func(ps.iter().map(sig_type).collect(), Box::new(sig_type(r))),
            }
        }
        sig_type(&parse_type_str(sig).unwrap_or_else(|| panic!("bad builtin signature: {sig}")))
    }

    /// Every builtin's signature reads to the type the language's parser gives.
    #[test]
    fn every_builtin_reads_as_the_parser_reads_it() {
        for b in BUILTINS {
            assert_eq!(parse(b.sig), by_parser(b.sig), "{} `{}`", b.name, b.sig);
            assert!(matches!(parse(b.sig), Type::Func(..)), "{}", b.sig);
        }
    }

    /// The syntax the table does not use yet reads the same way too.
    #[test]
    fn the_rest_of_the_syntax_reads_as_the_parser_reads_it() {
        for sig in [
            "fn(int | string, ?list<T>): ?map<K, list<V>>",
            "fn(fn(T): bool, fn(T, U,): ?U): list<T>",
            "fn(int,): (int | float)",
            "fn(?(int | string)): set<?int>",
            "fn(): fn(int): fn(): string",
            "fn( map< string , list<list<int>> > ):void",
            "fn(any|int): any",
        ] {
            assert_eq!(parse(sig), by_parser(sig), "{sig}");
        }
    }

    #[test]
    fn other_text_is_refused() {
        for sig in ["fn(int): int)", "fn(int) int", "fn(null): int", "fn(list<int, int>): int", "fn(int): int;"] {
            assert!(std::panic::catch_unwind(|| parse(sig)).is_err(), "{sig}");
        }
    }
}
