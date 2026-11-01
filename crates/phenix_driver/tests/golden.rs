//! Golden tests: every `tests/cases/*.phx` is compiled and run, and the result
//! (output, runtime error or compile errors) is compared with the `.out` file
//! next to it. Set `PHENIX_BLESS=1` to rewrite the expected files.

use phenix_driver::{compile, run, RunOptions};
use std::path::{Path, PathBuf};

fn render_case(root: &Path, path: &Path) -> String {
    match compile(root, path) {
        Err(e) => e.render(false),
        Ok(c) => {
            let outcome = run(&c, RunOptions { html: false, limits: Default::default(), http: None });
            let mut out = c.warnings.clone();
            out.push_str(&outcome.output);
            if let Some(err) = outcome.error {
                out.push_str("\n--- runtime error ---\n");
                out.push_str(&err.render());
            }
            out
        }
    }
}

#[test]
fn golden() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let bless = std::env::var_os("PHENIX_BLESS").is_some();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phx"))
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no test cases found in {}", root.display());

    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let failures = worker
        .spawn(move || {
            let mut failures = Vec::new();
            for case in &cases {
                let actual = render_case(&root, case).replace("\r\n", "\n");
                let expected_path = case.with_extension("out");
                if bless {
                    std::fs::write(&expected_path, &actual).unwrap();
                    continue;
                }
                let expected = std::fs::read_to_string(&expected_path).unwrap_or_default().replace("\r\n", "\n");
                if actual != expected {
                    failures.push(format!(
                        "=== {} ===\n--- expected ---\n{expected}\n--- actual ---\n{actual}",
                        case.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
            failures
        })
        .unwrap()
        .join()
        .unwrap();
    if !failures.is_empty() {
        panic!("{} golden test(s) failed:\n\n{}", failures.len(), failures.join("\n"));
    }
}
