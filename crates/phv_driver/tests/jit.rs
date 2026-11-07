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

/// Runs `src` with and without the JIT (every eligible function compiled);
/// both must give the same output and error, which is returned. `native`
/// functions must be compiled.
fn run_both(name: &str, src: &str, limits: Limits, native: &[&str]) -> String {
    let dir = std::env::temp_dir().join(format!("phv-jit-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit_all();
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
        assert!(jit.is_compiled(i as u32), "{name} was not compiled to native code");
    }
}

/// Runs `src` without the JIT and with it as `phv run` has it (functions
/// with a loop or recursion compiled at load, with the functions they call):
/// the output and the error, which must be the same apart from the stack
/// traces (an exception raised in native code has one that goes back only
/// as far as the caller, as the specification says), and the compiled
/// program.
fn run_as_shipped(name: &str, src: &str) -> (String, phv_driver::Compiled) {
    let dir = std::env::temp_dir().join(format!("phv-jit-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    let mut compiled = None;
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit();
        }
        let outcome = run(&c, RunOptions { html: false, limits: Limits::default(), http: None, host: None });
        results.push(format!("{}{}", outcome.output, outcome.error.map(|e| e.render()).unwrap_or_default()));
        compiled = Some(c);
    }
    let _ = std::fs::remove_dir_all(&dir);
    let strip = |s: &str| s.lines().filter(|l| !l.trim_start().starts_with("at ")).collect::<Vec<_>>().join("\n");
    assert_eq!(strip(&results[0]), strip(&results[1]), "the JIT changed the result of {name}");
    (results.pop().unwrap(), compiled.unwrap())
}

/// What is compiled is fixed when the program is loaded: a function with a
/// loop or recursion, with the functions it calls; a straight-line function
/// on its own is never compiled, however often it is called (entering native
/// code costs more than its body). The output and the error positions are
/// the interpreter's in both cases.
#[test]
fn straight_line_functions_are_compiled_only_within_a_unit() {
    let pick = "function pick(list<int> $xs, int $i): int {\n    return $xs[$i] * 2;\n}\n";
    // Alone, called 100,000 times (with an exception every fourth call).
    let src = format!(
        r#"<?phv
{pick}$xs = [1, 2, 3];
$t = 0;
$caught = 0;
$trace = "";
for ($k = 0; $k < 100000; $k++) {{
    try {{
        $t += pick($xs, $k % 4);
    }} catch (IndexError $e) {{
        $caught++;
        $trace = $e->getTraceAsString();
    }}
}}
echo $t, " ", $caught, "\n", $trace, "\n";
echo pick($xs, 7);
"#
    );
    let (out, c) = run_as_shipped("alone", &src);
    assert!(out.starts_with("300000 25000\nat pick (main.phv:3)\nat {main} (main.phv:11)\n"), "{out}");
    assert!(
        out.contains("Uncaught IndexError: list index 7 is out of range (length 3)\n  at pick (main.phv:3)\n  at {main} (main.phv:18)"),
        "{out}"
    );
    if !std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
        assert!(c.jit.is_none(), "nothing to compile: `pick` has no loop and no recursion");
    }

    // Reached from a function with a loop: compiled in that function's unit,
    // and then native also when called directly.
    let src = format!(
        r#"<?phv
{pick}function total(list<int> $xs, int $n): int {{
    $t = 0;
    for ($k = 0; $k < $n; $k++) {{
        $t += pick($xs, $k % 3);
    }}
    return $t;
}}
function unused(int $n): int {{
    return $n + 1;
}}
$xs = [1, 2, 3];
echo total($xs, 100000), " ", pick($xs, 1), "\n";
try {{
    echo total($xs, 4) + pick($xs, 3);
}} catch (IndexError $e) {{
    echo "caught: ", $e->getMessage(), "\n";
}}
echo pick($xs, 7);
"#
    );
    let (out, c) = run_as_shipped("within_unit", &src);
    assert!(out.starts_with("399998 4\ncaught: list index 3 is out of range (length 3)\n"), "{out}");
    assert!(out.contains("Uncaught IndexError: list index 7 is out of range (length 3)\n"), "{out}");
    if std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
        return;
    }
    let jit = c.jit.as_ref().unwrap();
    let id = |name: &str| c.program.funcs.iter().position(|f| f.name == name).unwrap() as u32;
    assert!(jit.is_compiled(id("total")), "a function with a loop");
    assert!(jit.is_compiled(id("pick")), "called by a compiled function");
    assert!(!jit.is_compiled(id("unused")), "straight-line, called by no compiled function");
    assert_eq!(jit.compiled(), 2);
    // Compiled, `pick` raises from the caller's line (the specification's rule).
    assert!(out.ends_with("  at {main} (main.phv:22)\n"), "{out}");
}

/// The compiled code of a function is shared between the programs of a
/// process that have the same text for it and what it calls: a second
/// program with the same function gets the same entry, whatever else it
/// holds and wherever its constants and functions are numbered.
#[test]
fn compiled_code_is_shared_between_programs() {
    let f = "function greet(string $who, int $n): string {\n    $s = \"\";\n    for ($i = 0; $i < $n; $i++) {\n        $s .= \"hi \" . $who . \"! \";\n    }\n    return $s;\n}\n";
    // The second program names other constants and functions before `greet`.
    let a = format!("<?phv\n{f}echo greet(\"a\", 2);\n");
    let b = format!("<?phv\nfunction other(int $x): int {{ return $x; }}\n$u = \"unrelated\" . other(1);\n{f}echo greet(\"b\", 3), $u;\n");
    let (out_a, ca) = run_as_shipped("share_a", &a);
    let (out_b, cb) = run_as_shipped("share_b", &b);
    assert_eq!(out_a, "hi a! hi a! ");
    assert_eq!(out_b, "hi b! hi b! hi b! unrelated1");
    if std::env::var("PHV_JIT").is_ok_and(|v| v == "0") {
        return;
    }
    let id = |c: &phv_driver::Compiled| c.program.funcs.iter().position(|f| f.name == "greet").unwrap() as u32;
    let ea = ca.jit.as_ref().unwrap().entry(id(&ca)).unwrap();
    let eb = cb.jit.as_ref().unwrap().entry(id(&cb)).unwrap();
    assert_eq!(ea.entry as usize, eb.entry as usize, "the same native code");
    assert_ne!(id(&ca), id(&cb), "numbered differently");
    assert_ne!(ea.consts, eb.consts, "with each program's own constants");
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
    let out = run_both("append", src, limits, &["append"]);
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

/// Loops test their condition at the end, jumping back while it holds: a loop
/// whose condition never fails stops at the time limit, natively and in the
/// interpreter, whatever the form of its test.
#[test]
fn loops_tested_at_their_end_stop_at_the_time_limit() {
    let limits = Limits { max_time: Some(Duration::from_millis(200)), ..Limits::default() };
    for (name, cond) in
        [("lt", "$i < 10"), ("imm", "$i <= $n"), ("eq", "$i == 0"), ("bool", "!$done"), ("or", "$i < 0 || $n > 0")]
    {
        let src = format!(
            "<?phv
function spin(int $n): int {{
    $i = 0;
    $done = false;
    while ({cond}) {{
        $i = $i * 1;
        $done = $i > 5;
    }}
    return $i;
}}
echo \"start\n\";
echo spin(5);
"
        );
        let out = run_both(name, &src, limits, &["spin"]);
        assert_eq!(
            out,
            "start
Fatal error: maximum execution time of 0.2 seconds exceeded",
            "{name}"
        );
    }
    // A `for` over a range, and one that ends: the same results.
    let src = "<?phv
function sum(int $n): int {
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $t = $t + $i;
    }
    foreach (1..$n as $j) {
        $t = $t + $j;
    }
    for ($k = 0; $k <= 3; $k++) {
        if ($k == 2) {
            continue;
        }
        $t = $t + 100;
    }
    return $t;
}
echo sum(0), \" \", sum(10);
";
    assert_eq!(run_both("sum", src, Limits::default(), &["sum"]), "300 390");
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

/// `.=` on an element of a string list or map, and `==` / `!=` in conditions
/// (one instruction that compares and branches) are compiled natively.
#[test]
fn element_appends_and_equality_branches_are_compiled() {
    let src = r#"<?phv
function tally(list<string> $words): string {
    map<string, string> $seen = [];
    list<string> $marks = ["", ""];
    foreach ($words as $w) {
        if ($w == "skip") {
            continue;
        }
        if (!$seen->has($w)) {
            $seen[$w] = "";
        }
        $seen[$w] .= "*";
        $marks[$w == "a" ? 0 : 1] .= $w;
    }
    $out = "";
    foreach ($seen as $k => $v) {
        $out .= $k . $v . " ";
    }
    return $out . $marks[0] . "|" . $marks[1];
}
function count_eq(?int $x, int $n): int {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        if ($x != null && $x == $i % 3) {
            $c++;
        }
        if ($i % 2 != 0) {
            $c += 10;
        }
    }
    return $c;
}
echo tally(["a", "b", "skip", "a", "c"]), "\n", count_eq(1, 9), " ", count_eq(null, 4);
"#;
    let out = run_both("element_appends", src, Limits::default(), &["tally", "count_eq"]);
    assert_eq!(out, "a** b* c* aa|bc\n43 20");
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
        c.enable_jit_all();
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
            // Recursive: compiled at load.
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

/// Native code cannot run the cycle collector: when it runs out of memory
/// that cycles made before the call hold, they are freed and the call runs
/// again in the interpreter, as it would have without the JIT.
#[test]
fn native_code_out_of_memory_frees_cycles_first() {
    let k = "k".repeat(1000);
    let src = format!(
        r#"<?phv
class Node {{
    public ?Node $next = null;
    public string $data = "";
}}
function fill(int $n): int {{
    list<string> $parts = [];
    for ($i = 0; $i < $n; $i++) {{
        $parts[] = "{k}" . $i;
    }}
    return count($parts);
}}
$big = str_repeat("x", 1 << 20);
for ($i = 0; $i < 20; $i++) {{
    $n = new Node();
    $n->data = $big . $i;
    $n->next = $n;
}}
echo fill(20000), " ";
echo fill(40000);
"#
    );
    let limits = Limits { max_memory: Some(32 << 20), ..Limits::default() };
    let out = run_both("cycles_memory", &src, limits, &["fill"]);
    // The second call needs more than the limit with nothing to free.
    assert_eq!(out, "20000 Fatal error: allowed memory size of 32 MB exhausted");
}

/// Found by the 1.0.0 audit: native code took `JIT_STACK` (8 MB) below where
/// it was first entered, whatever the thread had left, so entered deep inside
/// callbacks of builtins, or on a smaller thread, it could run past the end
/// of the stack. Now it stops before the end (the interpreter runs the call),
/// and callbacks stop with an error when the stack runs short.
#[test]
fn deep_callbacks_into_the_jit_stop_cleanly() {
    let mut src = String::from("<?phv\nfunction f(int $n): int {\n    $v0 = $n;\n");
    for i in 1..300 {
        src.push_str(&format!("    $v{i} = ($v{} * 31 + $n) % 1000003;\n", i - 1));
    }
    src.push_str("    $r = 0;\n    if ($n > 0) {\n        $r = f($n - 1);\n    }\n    return $r");
    for i in 0..300 {
        src.push_str(&format!(" + $v{i}"));
    }
    src.push_str(
        ";\n}\nfunction nest(int $d, int $n): int {\n    if ($d == 0) { return $n > 0 ? f($n) : 1; }\n    \
         return [1]->map(fn(int $x): int => nest($d - 1, $n))[0];\n}\n",
    );
    let dir = std::env::temp_dir().join(format!("phv-jit-test-{}-callbacks", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // On a thread of 4 MB (less than `JIT_STACK`), native code entered inside
    // callbacks runs out of its stack and the interpreter finishes the call.
    // Then 198 levels of callbacks on a thread of 1 MB: they fit in a release
    // build, and stop with an error in a debug one (larger frames).
    let cases = [(4 << 20, "echo nest(20, 9500) > 0 ? \"ok\" : \"no\";\n"), (1 << 20, "echo nest(198, 0);\n")];
    for (i, (stack, main)) in cases.into_iter().enumerate() {
        let entry = dir.join(format!("main{i}.phv"));
        std::fs::write(&entry, format!("{src}{main}")).unwrap();
        let root = dir.clone();
        // Compiled on a thread of the usual size: only the run is measured.
        let c = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let mut c = compile(&root, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
                c.enable_jit_all();
                c
            })
            .unwrap()
            .join()
            .unwrap();
        assert_compiled(&c, &["f"]);
        let out = std::thread::Builder::new()
            .stack_size(stack)
            .spawn(move || {
                let outcome = run(&c, RunOptions { html: false, limits: Limits::default(), http: None, host: None });
                (outcome.output, outcome.error.map(|e| e.render()))
            })
            .unwrap()
            .join()
            .expect("the run crashed");
        match i {
            0 => assert_eq!(out, ("ok".to_string(), None)),
            _ => assert!(
                out == ("1".to_string(), None)
                    || out.1.as_deref().is_some_and(|e| e.contains("maximum call stack depth exceeded")),
                "{out:?}"
            ),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
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
