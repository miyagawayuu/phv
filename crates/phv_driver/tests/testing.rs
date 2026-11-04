//! How `phv test` finds tests (`test_*` functions and `#[Test]` / `#[Skip]`),
//! runs `#[Before]` / `#[After]` around them, and measures coverage.

use phv_driver::{compile, compile_for_tests, run_test, RunOptions};

#[test]
fn finds_named_and_marked_tests() {
    let dir = std::env::temp_dir().join(format!("phv-tests-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = r#"<?phv
function helper(): int { return 1; }

function test_named(): void {}

#[Test]
function checks_marked(): void {}

#[Skip("not ready")]
function test_skipped(): void {}

#[Skip]
function marked_skip(): void {}

function test_takes_args(int $x): void {}
"#;
    std::fs::write(dir.join("math_test.phv"), src).unwrap();
    let root = dir.canonicalize().unwrap();
    let compiled = compile(&root, &root.join("math_test.phv")).map_err(|e| e.render(false)).unwrap();
    let tests: Vec<(String, Option<String>)> =
        compiled.tests().into_iter().map(|(_, name, skip)| (name, skip)).collect();
    assert_eq!(
        tests,
        [
            ("test_named".to_string(), None),
            ("checks_marked".to_string(), None),
            ("test_skipped".to_string(), Some("not ready".to_string())),
            ("marked_skip".to_string(), Some(String::new())),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runs_hooks_around_each_test_and_records_coverage() {
    let dir = std::env::temp_dir().join(format!("phv-hooks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = r#"<?phv
#[Before]
function first(): void { echo "before "; }

#[After]
function last(): void { echo " after"; }

function test_fails(): void {
    echo "test";
    assert(false, "no");
    echo "never";
}

function covered(int $n): int {
    if ($n > 0) {
        return 1;
    }
    return 0;
}

function test_covers(): void {
    assert_eq(1, covered(5));
}
"#;
    std::fs::write(dir.join("hooks_test.phv"), src).unwrap();
    let root = dir.canonicalize().unwrap();
    let entry = root.join("hooks_test.phv");
    let c = compile_for_tests(&root, &entry, true).map_err(|e| e.render(false)).unwrap();
    let (before, after) = c.hooks();
    assert_eq!((before.len(), after.len()), (1, 1));
    let tests = c.tests();
    assert_eq!(tests.len(), 2, "hooks are not tests");
    let opts = || RunOptions { html: false, limits: Default::default(), http: None, host: None };
    let mut ran = std::collections::HashSet::new();
    let failing = run_test(&c, &before, tests[0].0, &after, opts(), Some(&mut ran));
    assert_eq!(failing.output, "before test after", "the After hook runs after a failure");
    assert!(failing.error.is_some());
    let passing = run_test(&c, &before, tests[1].0, &after, opts(), Some(&mut ran));
    assert!(passing.error.is_none());
    // `return 0;` (line 18) never ran; `return 1;` (line 16) did.
    let line = |off: u32| src[..off as usize].matches('\n').count() + 1;
    let ran_lines: Vec<usize> = ran.iter().map(|(_, off)| line(*off)).collect();
    assert!(ran_lines.contains(&16) && !ran_lines.contains(&18), "{ran_lines:?}");
    assert!(!ran_lines.contains(&11), "nothing after the failed assert ran");
    // Without coverage, nothing is recorded and the program has no coverage instructions.
    let plain = compile_for_tests(&root, &entry, false).map_err(|e| e.render(false)).unwrap();
    assert!(plain.program.funcs.iter().all(|f| !f.code.iter().any(|i| matches!(i, phv_vm::Instr::Cover(_)))));
    let _ = std::fs::remove_dir_all(&dir);
}
