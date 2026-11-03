//! How `phv test` finds tests: `test_*` functions and `#[Test]` / `#[Skip]`.

use phv_driver::compile;

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
