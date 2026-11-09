//! Deterministic fuzzing of the front end: mutated versions of the golden
//! programs go through lexing, parsing, type checking (also the way an editor
//! does, past syntax errors), compilation and the JIT. Any panic is a bug
//! (bad input must become a diagnostic). Generated programs with objects,
//! methods and closures also run with and without the JIT, which must agree.

use phv_driver::{analyze_for_editor, compile_with_overlay};
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
    "<?phv ",
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
    "$this->",
    "parent::__construct(",
    "public function ",
    "function (int $x) use ($x): int { return $x; }",
    "fn(int $x): int => $x",
    "new A(1)",
    "?A ",
    "echo ",
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
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
        .collect();
    cases.sort();
    let iterations: usize = std::env::var("PHV_FUZZ_ITERATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
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
                        let _ = phv_syntax::format::format_source(&text);
                        // Editors check code even when it has syntax errors.
                        let _ = analyze_for_editor(&root, case, overlay.clone());
                        if let Ok(mut c) = compile_with_overlay(&root, case, overlay) {
                            c.enable_jit_all();
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

/// A random program with classes, properties of several kinds, methods
/// (overridden in a subclass and called through the method table), `new`,
/// output, closures (capturing by value and changing their copy, and passed
/// to builtins), and the errors these can raise: an index or key that is
/// missing, an integer overflow, a modulo by zero. Every loop is bounded.
fn generate(rng: &mut Rng) -> String {
    let c = |rng: &mut Rng, n: usize| 1 + rng.below(n);
    let mut stmts = Vec::new();
    for _ in 0..2 + rng.below(7) {
        let k = c(rng, 9);
        let s = match rng.below(24) {
            0 | 1 => "$t += $o->m0($i);".to_string(),
            2 => format!("$t = ($t * {k} + $i) % 1000003;"),
            3 => format!("$t = $t * {} + $i;", 1 + rng.below(1 << 20)),
            4 => format!("$o->n = $o->n + $i % {k};"),
            5 => format!("$o->s .= \"{}\";", ["a", "<b>", "&", "\\\"", "é"][rng.below(5)]),
            6 => format!("$o->xs[] = $i % {k};"),
            7 => format!("if (count($o->xs) > {k}) {{ $t += $o->xs[$i % {}]; }}", c(rng, 12)),
            8 => format!("$o->m[\"k\" . ($i % {k})] = $i;"),
            9 => format!("$t += $o->m[\"k\" . ($i % {k})] ?? {k};"),
            10 => format!("if ($i > {k}) {{ $t += $o->m[\"k\" . ($i % {})]; }}", c(rng, 12)),
            11 => format!("$o = $i % {k} == 0 ? new B($i) : new A($i);"),
            12 => "$o->next = $o;".to_string(),
            13 => "if ($o->next != null) { $t += $o->next->n % 7; }".to_string(),
            14 => "echo $i, \":\", $o->n, \" \";".to_string(),
            15 => "echo $o->f * $i, \";\";".to_string(),
            16 => "$o->m1($i);".to_string(),
            17 => format!("if ($i % {k} == 0) {{ continue; }}"),
            18 => format!("if ($t > {}) {{ break; }}", c(rng, 100000)),
            19 => "$o->f = $o->f + 0.25;".to_string(),
            20 => "$t += strlen($o->s) % 5;".to_string(),
            21 => format!("$t += $i % ({k} - $o->n % {k});"),
            22 => "$o->last = $o->next?->n;".to_string(),
            _ => "if ($o->last != null) { $t += $o->last; }".to_string(),
        };
        stmts.push(s);
    }
    let m0 = ["$this->n + $x", "$this->n * 3 + $x % 5", "$x - $this->n", "count($this->xs) + $x"];
    let m1 = ["$this->xs[] = $x;", "$this->n += $x % 3;", "echo \"m1 \";", "$this->s .= \"y\";"];
    let (a0, b0, m) = (m0[rng.below(4)], m0[rng.below(4)], m1[rng.below(4)]);
    let (n1, n2, n3) = (c(rng, 60), c(rng, 60), c(rng, 9));
    let body = stmts.join("\n        ");
    format!(
        r#"<?phv
class A {{
    public string $s = "";
    public list<int> $xs = [];
    public map<string, int> $m = [];
    public ?A $next = null;
    public ?int $last = null;
    public float $f = 0.5;
    public function __construct(public int $n) {{}}
    public function m0(int $x): int {{
        return {a0};
    }}
    public function m1(int $x): void {{
        {m}
    }}
}}
class B extends A {{
    public function m0(int $x): int {{
        return {b0} + 1;
    }}
}}
function work(A $o, int $n): int {{
    $t = 0;
    for ($i = 0; $i < $n; $i++) {{
        {body}
    }}
    return $t;
}}
function clo(int $n): int {{
    $k = {n3};
    $f = function (int $x) use ($k): int {{
        $k++;
        return $x * $k;
    }};
    $t = 0;
    for ($i = 0; $i < $n; $i++) {{
        $t += $f($i);
        $k = $k + 2;
    }}
    $xs = range(1, $n)->map(fn(int $x): int => $x * $k)->filter(fn(int $x): bool => $x % 3 == 0);
    return $t + $k + count($xs);
}}
$o = new A({n3});
try {{
    echo work($o, {n1}), "\n";
}} catch (Exception $e) {{
    echo $e->getMessage(), "\n", $e->getTraceAsString(), "\n";
}}
echo $o->n, " ", $o->s, " ", count($o->xs), " ", clo({n2}), "\n";
echo work(new B({n2}), {n2}), "\n";
"#
    )
}

/// Generated programs with objects, methods and closures give the same
/// output and the same errors, traces included, in the interpreter, with the
/// JIT as `phv run` has it, and with every eligible function compiled.
/// `PHV_FUZZ_PROGRAMS` sets how many (40 by default).
#[test]
fn jit_matches_the_interpreter_on_generated_programs() {
    let programs: usize = std::env::var("PHV_FUZZ_PROGRAMS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    let seed: u64 = std::env::var("PHV_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x5DEE_CE66_D1CE_4E5B);
    let dir = std::env::temp_dir().join(format!("phv-fuzz-jit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let (ran, compiled, raised, failures) = worker
        .spawn(move || {
            let mut rng = Rng(seed);
            let entry = dir.join("main.phv");
            let limits = phv_vm::Limits {
                max_time: Some(std::time::Duration::from_secs(20)),
                max_memory: Some(256 << 20),
                ..Default::default()
            };
            let (mut ran, mut compiled, mut raised, mut failures) = (0usize, 0usize, 0usize, Vec::new());
            for _ in 0..programs {
                let src = generate(&mut rng);
                std::fs::write(&entry, &src).unwrap();
                let mut results = Vec::new();
                for mode in 0..3 {
                    let mut c = match phv_driver::compile(&dir, &entry) {
                        Ok(c) => c,
                        Err(e) => {
                            failures.push(format!("--- does not compile:\n{src}\n{}", e.render(false)));
                            break;
                        }
                    };
                    match mode {
                        1 => c.enable_jit(),
                        2 => c.enable_jit_all(),
                        _ => {}
                    }
                    if mode == 2 {
                        let work = c.program.funcs.iter().position(|f| f.name == "work").unwrap();
                        compiled += c.jit.as_ref().is_some_and(|j| j.is_compiled(work as u32)) as usize;
                    }
                    let opts = phv_driver::RunOptions { html: false, limits, http: None, host: None };
                    let outcome = phv_driver::run(&c, opts);
                    results.push(format!("{}{}", outcome.output, outcome.error.map(|e| e.render()).unwrap_or_default()));
                }
                ran += 1;
                raised += results.first().is_some_and(|r| r.contains("\nat ") || r.contains("Uncaught")) as usize;
                if results.len() == 3 && (results[0] != results[1] || results[0] != results[2]) {
                    failures.push(format!(
                        "--- the JIT changed the result of:\n{src}\n--- interpreter:\n{}\n--- as shipped:\n{}\n--- all compiled:\n{}",
                        results[0], results[1], results[2]
                    ));
                }
                if failures.len() > 3 {
                    break;
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            (ran, compiled, raised, failures)
        })
        .unwrap()
        .join()
        .unwrap();
    eprintln!("jit fuzz: {ran} programs, `work` compiled in {compiled}, an exception in {raised}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    if !std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
        assert!(compiled * 2 >= ran, "the generated functions should mostly be compiled ({compiled} of {ran})");
    }
}
