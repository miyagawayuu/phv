//! The time limit on the way back out of a recursion. The limit is looked at
//! on loop steps and calls; returning from thousands of nested calls runs
//! neither, and each return may be followed by an instruction that goes over
//! a value as large as memory allows. Found by the 21st assessment:
//! `return back($s, $n - 1) . $s;` 9,900 deep ran 27 seconds under a
//! 2-second limit (395 seconds under 20 with a 1 GB memory limit). Long
//! instructions now look at the clock: past the deadline the interpreter
//! stops the script there, and native code at once after a join and
//! otherwise when the call that ran it returns.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::Limits;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Runs `src` interpreted and with the JIT, each under a 1-second limit;
/// the output and first error line of each run, and how long it took.
fn run_both(name: &str, src: &str) -> Vec<(String, Duration)> {
    let dir = std::env::temp_dir().join(format!("phv-unwind-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit_all();
        }
        let limits =
            Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(512 << 20), ..Limits::default() };
        let start = Instant::now();
        let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
        let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
        results.push((format!("{}{}", outcome.output, err.unwrap_or_default()), start.elapsed()));
    }
    let _ = std::fs::remove_dir_all(&dir);
    results
}

#[test]
fn the_way_back_out_of_a_recursion_stops_at_the_time_limit() {
    let cases = [
        // Each return joins a string that grows, behind a piece that is
        // not its own (so it is built anew): 290 GB copied in all.
        (
            "concat",
            r#"<?phv
function back(string $s, int $n): string {
    if ($n == 0) { return $s; }
    return $s . back($s, $n - 1);
}
echo strlen(back(str_repeat("a", 6000), 9900));
"#,
        ),
        // Each return compares two equal strings of 16 MB.
        (
            "compare",
            r#"<?phv
function same(string $a, string $b, int $n): int {
    if ($n == 0) { return 0; }
    return same($a, $b, $n - 1) + ($a == $b ? 1 : 0);
}
$a = str_repeat("a", 16 << 20);
$b = str_repeat("a", (16 << 20) - 1) . "a";
echo same($a, $b, 9900);
"#,
        ),
        // Each return copies a shared list of a million items to add one.
        (
            "copy",
            r#"<?phv
function grown(list<int> $l, int $n): list<int> {
    if ($n == 0) { return $l; }
    $r = grown($l, $n - 1);
    $keep = $r;
    $r[] = count($keep);
    return $r;
}
echo count(grown(range(1, 1000000), 9900));
"#,
        ),
        // Each return appends to a string that another variable holds too.
        (
            "append",
            r#"<?phv
function more(string $s, int $n): string {
    if ($n == 0) { return $s; }
    $r = more($s, $n - 1);
    $keep = $r;
    $r .= "x";
    return $r . ($keep == "" ? "y" : "");
}
echo strlen(more(str_repeat("a", 16 << 20), 9900));
"#,
        ),
    ];
    for (name, src) in cases {
        for (jit, (out, took)) in run_both(name, src).into_iter().enumerate() {
            assert!(out.contains("maximum execution time of 1 seconds exceeded"), "{name} (jit: {jit}): {out}");
            // Unchecked, each of these ran for 20 seconds or more (tests run side by side).
            assert!(took < Duration::from_secs(8), "{name} (jit: {jit}) took {took:?}");
        }
    }
}

/// A recursion that ends within its limit is not stopped by the check.
#[test]
fn a_recursion_within_its_limit_returns_its_value() {
    let src = r#"<?phv
function back(string $s, int $n): string {
    if ($n == 0) { return $s; }
    return back($s, $n - 1) . $s;
}
echo strlen(back(str_repeat("a", 70000), 60));
"#;
    for (out, _) in run_both("within", src) {
        assert_eq!(out, "4270000");
    }
}

/// Straight-line code without a return close by: a script's own statements,
/// a generator's body, and a way back that runs many joins just under the
/// size that counts as long (found by the re-attack of the 21st assessment:
/// 8,000 statements joining 8 to 32 MB each ran 15 seconds under a 1-second
/// limit, and 3,000 joins of 62 KB on each of 9,900 returns ran 50 seconds
/// under 2 without any error). The run now ends at the long instruction
/// itself; no `catch` and no `finally` of the script runs.
#[test]
fn straight_line_code_stops_at_the_time_limit() {
    // 8 MB joined from 16 pieces, then 32 MB from 4 of those, 3,000 times
    // over: 120 GB copied.
    let joins = format!("$t = $s{};\n$w = $t . $t . $t . $t;\n", " . $s".repeat(15)).repeat(3000);
    let guarded = |body: &str| {
        format!(
            "try {{\n{body}}} catch (Exception $e) {{\n    echo \"CAUGHT\";\n}} finally {{\n    echo \"FINALLY\";\n}}\n"
        )
    };
    let start = "$s = str_repeat(\"a\", 1 << 19);\n$t = \"\";\n$w = \"\";\n";
    let top = format!("<?phv\n{start}{}echo \"AFTER\", strlen($w);\n", guarded(&joins));
    let generator = format!(
        "<?phv\nfunction g(): Generator<int> {{\n{start}yield 1;\n{}yield strlen($w);\n}}\n\
         foreach (g() as $v) {{\n    echo $v, \";\";\n}}\necho \"AFTER\";\n",
        guarded(&joins)
    );
    // 62 KB each, 300 on every return: 180 GB copied in all.
    let medium = format!(
        "<?phv\nfunction back(string $v, int $n): int {{\n    if ($n == 0) {{ return 0; }}\n    $r = back($v, $n - 1);\n{}    \
         return $r + strlen($a);\n}}\n{}echo \"AFTER\";\n",
        "    $a = $v . $v;\n".repeat(300),
        guarded("    echo back(str_repeat(\"a\", 31000), 9900);\n")
    );
    for (name, src) in [("top", top), ("generator", generator), ("medium", medium)] {
        for (jit, (out, took)) in run_both(name, &src).into_iter().enumerate() {
            assert!(out.contains("maximum execution time of 1 seconds exceeded"), "{name} (jit: {jit}): {out}");
            for word in ["AFTER", "CAUGHT", "FINALLY"] {
                assert!(!out.contains(word), "{name} (jit: {jit}) went on to {word}: {out}");
            }
            // Unchecked, each of these ran for 10 seconds or more (tests run side by side).
            assert!(took < Duration::from_secs(6), "{name} (jit: {jit}) took {took:?}");
        }
    }
}

/// Found by the second re-attack of the 21st assessment's fixes: native code
/// added only operations of 4 KB or more to its sum, so 3,000 comparisons
/// of 4,000-byte strings on each of 9,900 returns ran 1.9 seconds under a
/// 0.3-second limit (here 3,000 returns under 0.1 seconds) and gave their result. Every operation adds up now.
#[test]
fn short_comparisons_on_the_way_back_add_up_in_native_code() {
    let src = format!(
        "<?phv\nfunction back(string $v, string $w, int $n): int {{\n    if ($n == 0) {{ return 0; }}\n    \
         $t = back($v, $w, $n - 1);\n{}    return $t;\n}}\n\
         echo back(str_repeat(\"a\", 4000), str_repeat(\"a\", 4000), 3000);\n",
        "    $t = $t + ($v == $w ? 1 : 0);\n".repeat(3000)
    );
    let dir = std::env::temp_dir().join(format!("phv-unwind-test-{}-short", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, &src).unwrap();
    let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    c.enable_jit_all();
    assert!(c.jit_count() > 0, "nothing was compiled");
    let limits =
        Limits { max_time: Some(Duration::from_millis(100)), max_memory: Some(512 << 20), ..Limits::default() };
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
    let _ = std::fs::remove_dir_all(&dir);
    let err = outcome.error.map(|e| e.render()).unwrap_or_default();
    // Unchecked, the run ends with its sum (9000000) after half a second or more.
    assert!(err.contains("maximum execution time"), "output {:?}, error {err:?}", outcome.output);
    assert!(!outcome.output.contains("9000000"), "{}", outcome.output);
}
