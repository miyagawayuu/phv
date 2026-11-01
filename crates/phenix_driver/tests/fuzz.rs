//! Deterministic fuzzing of the front end: mutated versions of the golden
//! programs go through lexing, parsing, type checking, compilation and the
//! JIT. Any panic is a bug (bad input must become a diagnostic).

use phenix_driver::compile_with_overlay;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const PIECES: &[&str] = &[
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    "$x",
    "$this",
    "\"",
    "'",
    "?>",
    "<?phx ",
    "<?= ",
    "fn",
    "=>",
    "::",
    "?->",
    "->",
    "..",
    "..=",
    "/*",
    "*/",
    "//",
    "\\u{",
    "{$",
    "??",
    "??=",
    ";",
    ",",
    ":",
    "match",
    "class",
    "interface",
    "enum",
    "extends",
    "implements",
    "abstract",
    "function",
    "return",
    "throw",
    "try",
    "finally",
    "yield",
    "new",
    "static",
    "list<",
    "map<",
    ">>",
    "?",
    "|",
    "int",
    "string",
    "null",
    "0x",
    "1e",
    "9223372036854775807",
    "-",
    "**",
    "%",
    "sql\"",
    "render(\"",
    "import ",
    "export ",
    "const ",
    "case ",
    "endif;",
    "endforeach;",
    ":",
    "\n",
    "é",
    "😀",
];

fn mutate(src: &str, rng: &mut Rng) -> String {
    let mut s: Vec<char> = src.chars().collect();
    for _ in 0..1 + rng.below(4) {
        let len = s.len();
        match rng.below(6) {
            0 if len > 0 => {
                let a = rng.below(len);
                let b = (a + 1 + rng.below(20)).min(len);
                s.drain(a..b);
            }
            1 if len > 0 => {
                let a = rng.below(len);
                let b = (a + 1 + rng.below(40)).min(len);
                let chunk: Vec<char> = s[a..b].to_vec();
                let at = rng.below(len);
                s.splice(at..at, chunk);
            }
            2 | 3 => {
                let piece = PIECES[rng.below(PIECES.len())];
                let at = rng.below(len + 1);
                s.splice(at..at, piece.chars());
            }
            4 => {
                let at = rng.below(len + 1);
                let c = (32 + rng.below(95)) as u8 as char;
                s.insert(at, c);
            }
            _ => {
                let at = rng.below(len + 1);
                s.truncate(at);
            }
        }
    }
    s.into_iter().collect()
}

#[test]
fn front_end_never_panics() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phx"))
        .collect();
    cases.sort();
    let iterations: usize = std::env::var("PHENIX_FUZZ_ITERATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let worker = std::thread::Builder::new().stack_size(256 * 1024 * 1024);
    let failures = worker
        .spawn(move || {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut failures = Vec::new();
            for case in &cases {
                let src = std::fs::read_to_string(case).unwrap();
                let key = case.canonicalize().unwrap();
                for _ in 0..iterations {
                    let text = mutate(&src, &mut rng);
                    let overlay = HashMap::from([(key.clone(), text.clone())]);
                    let r = std::panic::catch_unwind(|| {
                        let _ = phenix_syntax::format::format_source(&text);
                        if let Ok(mut c) = compile_with_overlay(&root, case, overlay) {
                            c.enable_jit();
                        }
                    });
                    if r.is_err() {
                        failures.push(format!("--- panic on mutation of {}:\n{text}", case.display()));
                        if failures.len() > 5 {
                            return failures;
                        }
                    }
                }
            }
            failures
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
