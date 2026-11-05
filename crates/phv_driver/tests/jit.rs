//! The JIT under the time and memory limits (the golden tests run without
//! limits), and checks that the JIT golden cases really are compiled.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::Limits;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn cases() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases")
}

/// Runs `src` with and without the JIT; both must give the same output and
/// error, which is returned. `native` functions must be compiled.
fn run_both(name: &str, src: &str, limits: Limits, native: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("phv-jit-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit();
            assert_compiled(&c, native);
        }
        let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
        let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
        results.push((outcome.output, err));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(results[0], results[1], "the JIT changed the result of {name}");
    let (out, err) = results.pop().unwrap();
    format!("{out}{}", err.unwrap_or_default())
}

fn assert_compiled(c: &phv_driver::Compiled, names: &[&str]) {
    if std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
        return;
    }
    let jit = c.jit.as_ref().expect("nothing was compiled");
    for name in names {
        let i = c.program.funcs.iter().position(|f| f.name == *name).unwrap_or_else(|| panic!("no function {name}"));
        assert!(jit.entries[i].is_some(), "{name} was not compiled to native code");
    }
}

#[test]
fn memory_limit_inside_native_string_loops() {
    let limits = Limits { max_memory: Some(16 << 20), ..Limits::default() };
    // `.=` checks the limit as the string grows, `.` before it allocates.
    let src = r#"<?phv
function append(int $n): string {
    $s = "";
    for ($i = 0; $i < $n; $i++) {
        $s .= "0123456789abcdef" . $i;
    }
    return $s;
}
function double(string $s): string {
    while (true) {
        $s = $s . $s . "x";
    }
    return $s;
}
echo strlen(append(1000)), "\n";
echo strlen(append(10000000)), "\n";
"#;
    let out = run_both("append", src, limits, &["append", "double"]);
    assert_eq!(out, "18890\nFatal error: allowed memory size of 16 MB exhausted");
    let src = src.replace("append(10000000)", "double(\"ab\")");
    let out = run_both("double", &src, limits, &["append", "double"]);
    assert_eq!(out, "18890\nFatal error: allowed memory size of 16 MB exhausted");
}

#[test]
fn time_limit_inside_native_string_loops() {
    let limits = Limits { max_time: Some(Duration::from_millis(200)), ..Limits::default() };
    let src = r#"<?phv
function spin(string $s, float $f): int {
    $n = 0;
    while (true) {
        $t = $s . $n . $f;
        $n = $n + strlen($t) % 2 + 1;
        $f = $f * 1.000001;
    }
    return $n;
}
echo "start\n";
echo spin("x", 1.5);
"#;
    let out = run_both("spin", src, limits, &["spin"]);
    assert_eq!(out, "start\nFatal error: maximum execution time of 0.2 seconds exceeded");
}

/// Comparing or joining long strings takes long for one instruction: the
/// time limit is checked soon after, not thousands of instructions later.
#[test]
fn time_limit_after_long_string_operations() {
    let limits = Limits { max_time: Some(Duration::from_millis(300)), ..Limits::default() };
    let src = r#"<?phv
function same(string $a, string $b, int $n): int {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        if ($a == $b) {
            $c++;
        }
    }
    return $c;
}
function join(string $a, int $n): int {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        $t = $a . "";
        $c += strlen($t) % 7;
    }
    return $c;
}
$a = "x"->repeat(8000000);
$b = "x"->repeat(8000000);
echo same($a, $b, 1000000), join($a, 1000000);
"#;
    for f in ["same", "join"] {
        let src = if f == "join" { src.replace("echo same($a, $b, 1000000), ", "echo ") } else { src.to_string() };
        let start = std::time::Instant::now();
        let out = run_both(f, &src, limits, &[f]);
        assert_eq!(out, "Fatal error: maximum execution time of 0.3 seconds exceeded", "{f}");
        // Two runs (with and without the JIT), each stopping near the limit.
        assert!(start.elapsed() < Duration::from_secs(4), "{f} took {:?}", start.elapsed());
    }
}

#[test]
fn golden_jit_cases_are_compiled() {
    for (case, names) in [
        ("jit_float.phv", &["poly", "harmonic", "power_sum", "compare", "compare_mixed", "fact", "to_int", "nan"][..]),
        (
            "jit_string.phv",
            &["repeat", "numbered", "describe", "order", "lengths", "reverse", "longest", "fail_inside"],
        ),
    ] {
        let mut c = compile(&cases(), &cases().join(case)).unwrap_or_else(|e| panic!("{}", e.render(false)));
        c.enable_jit();
        assert_compiled(&c, names);
    }
}

/// Native recursion is bounded by the stack, not only by the depth: a function
/// with a large frame that recurses deep does not overflow the thread's stack
/// (a server worker's 16 MB). When native code runs out of stack, the
/// interpreter runs the call, so the result is the same as without the JIT.
#[test]
fn deep_recursion_of_large_frames_does_not_overflow() {
    let mut src = String::from("<?php\nfunction f(int $n): int {\n    $v0 = $n;\n");
    for i in 1..300 {
        src.push_str(&format!("    $v{i} = ($v{} * 31 + $n) % 1000003;\n", i - 1));
    }
    src.push_str("    $r = 0;\n    if ($n > 0) {\n        $r = f($n - 1);\n    }\n    return $r");
    for i in 0..300 {
        src.push_str(&format!(" + $v{i}"));
    }
    src.push_str(";\n}\necho f(9900) > 0 ? \"ok\" : \"no\";\n");
    let src = src.replace("<?php", "<?phv");
    let dir = std::env::temp_dir().join(format!("phv-jit-test-{}-deep", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let out = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || {
            let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
            c.enable_jit();
            assert_compiled(&c, &["f"]);
            let outcome = run(&c, RunOptions { html: false, limits: Limits::default(), http: None, host: None });
            (outcome.output, outcome.error.map(|e| e.render()))
        })
        .unwrap()
        .join()
        .expect("the run crashed");
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("phv-jit-test-{}-deep", std::process::id())));
    assert_eq!(out, ("ok".to_string(), None));
}

/// Functions on lists of ints, floats, strings and bools are compiled, and
/// give what the interpreter gives: values, copies, errors and limits.
#[test]
fn lists_are_compiled_natively() {
    let src = r#"<?phv
function sieve(int $n): int {
    list<bool> $prime = [];
    for ($i = 0; $i <= $n; $i++) {
        $prime[] = true;
    }
    $count = 0;
    for ($i = 2; $i <= $n; $i++) {
        if ($prime[$i]) {
            $count++;
            for ($j = $i * $i; $j <= $n; $j += $i) {
                $prime[$j] = false;
            }
        }
    }
    return $count;
}

function bubble(list<int> $xs): list<int> {
    $n = count($xs);
    for ($i = 0; $i < $n; $i++) {
        for ($j = 0; $j < $n - 1 - $i; $j++) {
            if ($xs[$j] > $xs[$j + 1]) {
                $t = $xs[$j];
                $xs[$j] = $xs[$j + 1];
                $xs[$j + 1] = $t;
            }
        }
    }
    return $xs;
}

function total(list<float> $xs): float {
    $t = 0.0;
    foreach ($xs as $x) {
        $t += $x;
    }
    return $t;
}

function words(list<string> $ws): string {
    $out = "";
    foreach ($ws as $i => $w) {
        $out .= $i . "=" . $w . " ";
    }
    return $out . count($ws);
}

function squares(int $n): list<int> {
    list<int> $out = [];
    foreach (range(1, $n) as $x) {
        $out[] = $x ** 2;
    }
    return $out;
}

function at(list<int> $xs, int $i): int {
    return $xs[$i];
}

function pow_of(int $a, int $b): int {
    return $a ** $b;
}

$orig = [5, 3, 9, 1, 7];
$sorted = bubble($orig);
echo sieve(1000), " ", $sorted->join(","), " ", $orig->join(","), "\n";
echo total([1.5, 2.25, 3.0]), " ", words(["a", "b", "c"]), "\n";
echo squares(5)->join(","), " ", count(squares(0)), " ", pow_of(3, 4), "\n";
try {
    echo at([1, 2, 3], 5);
} catch (IndexError $e) {
    echo "IndexError: ", $e->getMessage(), "\n";
}
try {
    echo pow_of(2, -1);
} catch (ArithmeticError $e) {
    echo $e->getMessage(), "\n";
}
try {
    echo pow_of(10, 30);
} catch (ArithmeticError $e) {
    echo $e->getMessage(), "\n";
}
"#;
    let out =
        run_both("lists", src, Limits::default(), &["sieve", "bubble", "total", "words", "squares", "at", "pow_of"]);
    assert_eq!(
        out,
        "168 1,3,5,7,9 5,3,9,1,7\n6.75 0=a 1=b 2=c 3\n1,4,9,16,25 2 81\n\
         IndexError: list index 5 is out of range (length 3)\n\
         negative exponent in int ** int; use a float base, e.g. 2.0 ** -1\ninteger overflow\n"
    );
}

/// A list built in a native loop stops at the memory limit, and a long one
/// at the time limit, as in the interpreter.
#[test]
fn native_list_loops_stop_at_the_limits() {
    let src = r#"<?phv
function grow(int $n): int {
    list<int> $xs = [];
    for ($i = 0; $i < $n; $i++) {
        $xs[] = $i;
    }
    return count($xs);
}
echo grow(100000000);
"#;
    let limits = Limits { max_memory: Some(16 << 20), ..Limits::default() };
    let out = run_both("list_memory", src, limits, &["grow"]);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");

    let src = r#"<?phv
function spin(list<int> $xs): int {
    $t = 0;
    while (true) {
        foreach ($xs as $x) {
            $t = ($t + $x) % 1000;
        }
    }
    return $t;
}
echo spin(range(1, 1000));
"#;
    let limits = Limits { max_time: Some(Duration::from_millis(500)), ..Limits::default() };
    let out = run_both("list_time", src, limits, &["spin"]);
    assert!(out.contains("maximum execution time"), "{out}");
}

/// Maps of scalars and nullable scalars are compiled too, with the
/// interpreter's results and errors.
#[test]
fn maps_and_nullable_values_are_compiled_natively() {
    let src = r#"<?phv
function count_keys(int $n): int {
    map<string, int> $counts = [];
    foreach (0..$n as $i) {
        $key = "k" . ($i % 7);
        $counts[$key] = ($counts[$key] ?? 0) + 1;
    }
    $t = 0;
    foreach ($counts as $k => $v) {
        $t += $v * strlen($k);
    }
    return $t + count($counts);
}

function first_pos(list<int> $xs, int $v): ?int {
    ?int $found = null;
    foreach ($xs as $i => $x) {
        if ($x == $v && $found == null) {
            $found = $i;
        }
    }
    return $found;
}

function use_pos(list<int> $xs, int $v): int {
    $p = first_pos($xs, $v);
    if ($p != null) {
        return $p + 100;
    }
    return $p ?? -1;
}

function lookup(map<int, string> $names, int $id): string {
    $n = $names[$id] ?? null;
    if ($n == null) {
        return "?";
    }
    return $n . ($names->has($id + 1) ? "+" : "");
}

function strict(map<string, float> $m, string $k): float {
    return $m[$k] * 2.0;
}

function half(?float $x): ?float {
    if ($x == null) {
        return null;
    }
    return $x / 2.0;
}

function build(int $n): map<int, bool> {
    map<int, bool> $m = [];
    for ($i = 0; $i < $n; $i++) {
        $m[$i * 2] = $i % 2 == 0;
    }
    return $m;
}

echo count_keys(100), " ", use_pos([4, 5, 6, 5], 5), " ", use_pos([1], 9), "\n";
echo lookup([1 => "a", 2 => "b"], 1), lookup([1 => "a", 2 => "b"], 2), lookup([1 => "a"], 3), "\n";
echo half(3.0) ?? -1.0, " ", half(null) ?? -1.0, " ", count(build(5)), "\n";
try {
    echo strict(["x" => 1.5], "y");
} catch (KeyError $e) {
    echo "KeyError: ", $e->getMessage(), "\n";
}
"#;
    let out = run_both(
        "maps",
        src,
        Limits::default(),
        &["count_keys", "first_pos", "use_pos", "lookup", "strict", "half", "build"],
    );
    assert_eq!(out, "207 101 -1\na+b?\n1.5 -1 5\nKeyError: undefined map key \"y\"\n");
}

/// Found by the 1.0 audit: a function with only int locals calling one with a
/// `?int` parameter (passed in the window) had no window of its own, and the
/// call wrote into its caller's (an abort, or a caller's string emptied).
#[test]
fn calls_with_nullable_arguments_keep_their_windows() {
    let src = r#"<?phv
function orZero(?int $x): int {
    return $x ?? 0;
}
function twice(int $a): int {
    return orZero($a) * 2;
}
function maybe(int $a): ?int {
    return $a > 0 ? $a : null;
}
function outer(string $s): string {
    $t1 = $s . "1";
    $t2 = $s . "2";
    $n = twice(21) + (maybe(1) ?? 0);
    return $t1 . "|" . $t2 . "|" . $n;
}
echo twice(21), " ", outer("s"), "\n";
"#;
    let out = run_both("nullable_calls", src, Limits::default(), &["orZero", "twice", "maybe", "outer"]);
    assert_eq!(out, "42 s1|s2|43\n");
}

/// Found by the 1.0 audit: arguments kept for a rerun made a function that
/// changes its list parameter copy the whole list (128 MB were not enough).
#[test]
fn a_list_parameter_changed_in_place_is_not_copied() {
    let src = r#"<?phv
function touch(list<int> $xs): int {
    $xs[0] = 5;
    return $xs[0] + count($xs);
}
function fill(int $n): int {
    list<int> $xs = [];
    while (count($xs) < $n) {
        $xs[] = 1;
    }
    return count($xs);
}
echo touch(range(1, 5000000)), " ", fill(200000), "\n";
"#;
    let limits = Limits { max_memory: Some(128 << 20), max_time: Some(Duration::from_secs(10)), ..Limits::default() };
    let out = run_both("param_in_place", src, limits, &["touch", "fill"]);
    assert_eq!(out, "5000005 200000\n");
}
