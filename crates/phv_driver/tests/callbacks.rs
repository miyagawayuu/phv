//! The callback fast path (`Vm::callback`): `map`, `filter`, `sortBy` and
//! the other builtins that call a closure per element skip what `call_value`
//! checks before every call. Every program here runs through the fast path
//! and through the full path (`Compiled::slow_callbacks`), with the JIT on
//! and off, and its output, the traces of its exceptions and the position
//! of its error must be the same each way.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::Limits;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// The memory limit is enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Compiles and runs `entry` under `root`: its output, then the runtime error
/// as `phv run` prints it (the message and the trace).
fn render(root: &Path, entry: &Path, slow: bool, jit: bool, limits: Limits) -> String {
    let mut c = match compile(root, entry) {
        Ok(c) => c,
        Err(e) => return e.render(false),
    };
    c.slow_callbacks = slow;
    if jit {
        c.enable_jit_all();
    }
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
    let mut out = outcome.output;
    if let Some(err) = outcome.error {
        out.push_str("\n--- runtime error ---\n");
        out.push_str(&err.render());
    }
    out.replace("\r\n", "\n")
}

/// Runs the program the four ways (on a thread with room for deep
/// recursion) and checks that they agree; returns the result.
fn same_both_ways(name: &str, root: &Path, entry: &Path, limits: Limits) -> String {
    let (name, root, entry) = (name.to_string(), root.to_path_buf(), entry.to_path_buf());
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    worker
        .spawn(move || {
            let fast = render(&root, &entry, false, false, limits);
            let slow = render(&root, &entry, true, false, limits);
            assert_eq!(fast, slow, "{name}: the fast path and the full path differ (interpreter)");
            let fast_jit = render(&root, &entry, false, true, limits);
            let slow_jit = render(&root, &entry, true, true, limits);
            assert_eq!(fast_jit, slow_jit, "{name}: the fast path and the full path differ (JIT)");
            // Errors raised inside native code have a shorter trace (as the
            // golden tests allow); everything else is the same.
            let strip =
                |s: &str| s.lines().filter(|l| !l.trim_start().starts_with("at ")).collect::<Vec<_>>().join("\n");
            assert_eq!(strip(&fast), strip(&fast_jit), "{name}: the JIT changes the result");
            fast
        })
        .unwrap()
        .join()
        .unwrap()
}

/// Writes `src` as `main.phv` of its own directory and runs it the four ways.
fn same_both_ways_src(name: &str, src: &str, limits: Limits) -> String {
    let dir = std::env::temp_dir().join(format!("phv-callbacks-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let out = same_both_ways(name, &dir, &entry, limits);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

const PRELUDE: &str = r#"<?phv
import std.regex.Regex;

class Counter {
    public int $n = 0;
}

function inc(int $x, int $by = 1): int {
    return $x + $by;
}

function boom(int $x): int {
    if ($x == 1000) {
        throw new RuntimeException("at element $x");
    }
    return $x;
}

// A recursive callback: each level maps a list of one element.
function depth(int $n): int {
    return $n == 0 ? 0 : [$n - 1]->map(fn(int $m): int => depth($m) + 1)[0];
}

// Recursion through callbacks, deeper than a trace keeps whole, then a throw.
function deep(int $n): int {
    return $n == 0 ? boom(1000) : [$n - 1]->map(fn(int $m): int => deep($m) + 1)[0];
}
"#;

/// Caught exceptions (their traces read while caught and later), nested and
/// recursive callbacks, captures, default parameters, a `finally`, and every
/// builtin that calls a closure per element.
#[test]
fn callbacks_behave_the_same_through_the_fast_path() {
    let src = format!(
        r#"{PRELUDE}
// Throws at the 1,000th element: the trace, read while caught and after.
list<RuntimeException> $kept = [];
try {{
    range(1, 5000)->map(fn(int $x): int => boom($x));
}} catch (RuntimeException $e) {{
    echo $e->getMessage(), "\n", $e->getTraceAsString(), "\n";
    $kept[] = $e;
}}
try {{
    range(1, 5000)->filter(fn(int $x): bool => boom($x) % 2 == 0);
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
try {{
    range(1, 5000)->reduce(0, fn(int $acc, int $x): int => $acc + boom($x));
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
try {{
    range(1, 5000)->find(fn(int $x): bool => boom($x) < 0);
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
try {{
    range(1, 5000)->sortBy(fn(int $a, int $b): int => boom($a) <=> $b);
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
try {{
    set(range(1, 5000))->any(fn(int $x): bool => boom($x) < 0);
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
// Thrown from a nested map inside a callback.
try {{
    range(1, 3)->map(fn(int $x): list<int> => range(998, 1001)->map(fn(int $y): int => boom($y) * $x));
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
// Thrown at the bottom of a recursion through callbacks (240 frames).
try {{
    deep(120);
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
// Thrown from a callback with a `finally` (the full path either way).
try {{
    range(1, 5000)->map(function (int $x): int {{
        try {{
            return boom($x);
        }} finally {{
            if ($x == 1000) {{
                echo "[finally]\n";
            }}
        }}
    }});
}} catch (RuntimeException $e) {{
    $kept[] = $e;
}}
foreach ($kept as $i => $e) {{
    echo "-- $i\n", $e->getTraceAsString(), "\n";
}}

// Nested map inside a callback.
echo range(1, 3)
    ->map(fn(int $x): list<int> => range(1, $x)->map(fn(int $y): int => $x * $y))
    ->map(fn(list<int> $l): string => $l->join("+"))
    ->join(" | "), "\n";
// A recursive callback.
echo depth(50), "\n";
// Captures: a copied value (every call starts from the captured copy) and
// an object changed through its handle.
$count = 0;
$c = new Counter();
$seen = range(1, 5)->map(function (int $x) use ($count, $c): int {{
    $count++;
    $c->n += $x;
    return $count;
}});
echo $seen->join(","), " ", $count, " ", $c->n, "\n";
// Default parameters: a function with one, called by the callback with and without it.
echo range(1, 3)->map(fn(int $x): int => inc($x))->join(","), " ",
    range(1, 3)->map(fn(int $x): int => inc($x, 10))->join(","), "\n";

// Every builtin that calls a closure per element.
$xs = range(1, 10);
echo $xs->map(fn(int $x): int => $x * 2)->join(","), "\n";
echo $xs->filter(fn(int $x): bool => $x % 3 == 0)->join(","), "\n";
echo $xs->find(fn(int $x): bool => $x > 4) ?? -1, " ", $xs->any(fn(int $x): bool => $x > 9) ? "any" : "none", " ",
    $xs->all(fn(int $x): bool => $x > 0) ? "all" : "not all", "\n";
echo $xs->sortBy(fn(int $a, int $b): int => $b <=> $a)->join(","), "\n";
echo $xs->reduce(0, fn(int $acc, int $x): int => $acc + $x), "\n";
echo $xs->flatMap(fn(int $x): list<int> => [$x, -$x])->join(","), "\n";
echo json_encode($xs->groupBy(fn(int $x): string => $x % 2 == 0 ? "even" : "odd")), "\n";
echo json_encode($xs->groupBy(fn(int $x): int => $x % 3)), "\n";
echo $xs->zip(["a", "b", "c"], fn(int $n, string $s): string => $s . $n)->join(","), "\n";
[$big, $small] = $xs->partition(fn(int $x): bool => $x > 5);
echo $big->join(","), " / ", $small->join(","), "\n";
echo $xs->count(fn(int $x): bool => $x % 2 == 1), "\n";
map<string, int> $m = ["a" => 1, "b" => 2, "c" => 3];
echo json_encode($m->filter(fn(string $k, int $v): bool => $v != 2)), " ",
    json_encode($m->map(fn(int $v): int => $v * 10)), "\n";
$s = set(["b", "a", "c"]);
echo $s->filter(fn(string $x): bool => $x != "b")->toList()->join(","), " ",
    $s->any(fn(string $x): bool => $x == "c") ? "any" : "none", " ",
    $s->all(fn(string $x): bool => $x->len() == 1) ? "all" : "not all", "\n";
echo (new Regex('(\w)(\w*)'))->replaceWith("hello big world", fn(list<string> $g): string => $g[1]->upper() . $g[2]), "\n";
// Many short callbacks in a row (the time limit is ticked at each).
echo range(1, 200000)->map(fn(int $x): int => $x % 7)->filter(fn(int $x): bool => $x == 3)->len(), "\n";
"#
    );
    let out = same_both_ways_src("caught", &src, Limits::default());
    assert!(out.contains("at element 1000\nat boom (main.phv:14)\nat {closure} (main.phv:"), "{out}");
    assert!(out.contains(" more calls ..."), "the deep recursion's trace is cut: {out}");
    assert!(out.contains("[finally]\n"), "{out}");
    // `$count++` works on the captured copy, so every call returns 1 and the
    // variable outside stays 0; the object is changed through its handle.
    assert!(out.contains("\n1,1,1,1,1 0 15\n"), "captures: {out}");
    assert!(out.contains("\n2,3,4 11,12,13\n"), "default parameters: {out}");
    assert!(out.contains("\n28572\n"), "{out}");
    assert!(!out.contains("runtime error"), "{out}");
}

/// An exception a callback throws at the 1,000th element and nobody catches:
/// the error and its trace as `phv run` prints them.
#[test]
fn an_uncaught_exception_in_a_callback_has_the_same_trace() {
    let src = format!(
        r#"{PRELUDE}
echo "start\n";
$total = range(1, 5000)->map(fn(int $x): int => boom($x))->sum();
echo "never $total\n";
"#
    );
    let out = same_both_ways_src("uncaught", &src, Limits::default());
    assert!(
        out.contains("start\n\n--- runtime error ---\nUncaught RuntimeException: at element 1000\n  at boom (main.phv:14)\n  at {closure} (main.phv:"),
        "{out}"
    );
    assert!(out.contains("  at {main} (main.phv:"), "{out}");
}

/// A callback that runs past the time limit, and one that allocates past the
/// memory limit, are stopped the same way either path.
#[test]
fn the_limits_stop_a_callback_the_same_way() {
    let src = format!(
        r#"{PRELUDE}
echo "start\n";
echo range(1, 10000000)->map(fn(int $x): int => str_repeat("a", 1000)->len() + $x)->sum(), "\n";
"#
    );
    let limits = Limits { max_time: Some(Duration::from_secs(1)), ..Limits::default() };
    let start = Instant::now();
    let out = same_both_ways_src("time", &src, limits);
    assert!(start.elapsed() < Duration::from_secs(30), "took {:?}", start.elapsed());
    assert_eq!(out, "start\n\n--- runtime error ---\nFatal error: maximum execution time of 1 seconds exceeded\n");

    let src = format!(
        r#"{PRELUDE}
echo "start\n";
echo range(1, 100000)->map(fn(int $x): string => str_repeat("a", 1000))->len(), "\n";
"#
    );
    let limits = Limits { max_memory: Some(16 << 20), ..Limits::default() };
    let out = same_both_ways_src("memory", &src, limits);
    assert_eq!(out, "start\n\n--- runtime error ---\nFatal error: allowed memory size of 16 MB exhausted\n");
}

/// Every golden case that passes a closure to a builtin gives the same result
/// through the full path.
#[test]
fn the_golden_cases_with_callbacks_agree() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
        .filter(|p| {
            let src = std::fs::read_to_string(p).unwrap_or_default();
            ["->map(", "->filter(", "->sortBy(", "->reduce(", "->any(", "->all(", "->find(", "->groupBy("]
                .iter()
                .any(|m| src.contains(m))
        })
        .collect();
    cases.sort();
    assert!(cases.len() >= 10, "{} cases", cases.len());
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    worker
        .spawn(move || {
            for case in &cases {
                let name = case.file_name().unwrap().to_string_lossy().into_owned();
                let fast = render(&root, case, false, false, Limits::default());
                let slow = render(&root, case, true, false, Limits::default());
                assert_eq!(fast, slow, "{name}: the fast path and the full path differ");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
