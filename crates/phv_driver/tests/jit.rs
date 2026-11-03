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
/// with a large frame that recurses deep fails with a depth error instead of
/// overflowing the thread's stack (a server worker's 16 MB).
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
    match out {
        (o, None) => assert_eq!(o, "ok"),
        (_, Some(e)) => assert!(e.contains("maximum call stack depth"), "{e}"),
    }
}
