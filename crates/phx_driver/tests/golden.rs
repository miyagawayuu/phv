//! Golden tests: every `tests/cases/*.phx` is compiled and run, and the result
//! (output, runtime error or compile errors) is compared with the `.out` file
//! next to it. Set `PHX_BLESS=1` to rewrite the expected files.

use phx_driver::{compile_with_overlay, run, RunOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn render_case(root: &Path, path: &Path) -> String {
    render_with(root, path, HashMap::new())
}

fn render_with(root: &Path, path: &Path, overlay: HashMap<PathBuf, String>) -> String {
    render_opts(root, path, overlay, false)
}

fn render_opts(root: &Path, path: &Path, overlay: HashMap<PathBuf, String>, jit: bool) -> String {
    match compile_with_overlay(root, path, overlay) {
        Err(e) => e.render(false),
        Ok(mut c) => {
            if jit {
                c.enable_jit();
            }
            let outcome = run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: None });
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
    let bless = std::env::var_os("PHX_BLESS").is_some();
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

/// `phx fmt` must not change what any program prints.
#[test]
fn formatting_preserves_behavior() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phx"))
        .collect();
    cases.sort();
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let failures = worker
        .spawn(move || {
            let mut failures = Vec::new();
            for case in &cases {
                let expected =
                    std::fs::read_to_string(case.with_extension("out")).unwrap_or_default().replace("\r\n", "\n");
                if expected.starts_with("error") || expected.starts_with("warning") {
                    continue; // diagnostics mention columns, which formatting may move
                }
                let src = std::fs::read_to_string(case).unwrap();
                let formatted = phx_syntax::format::format_source(&src).unwrap();
                let again = phx_syntax::format::format_source(&formatted).unwrap();
                let name = case.file_name().unwrap().to_string_lossy().to_string();
                if again != formatted {
                    failures.push(format!("{name}: formatting is not idempotent"));
                }
                let overlay = HashMap::from([(case.canonicalize().unwrap(), formatted)]);
                let actual = render_with(&root, case, overlay).replace("\r\n", "\n");
                let strip =
                    |s: &str| s.lines().filter(|l| !l.trim_start().starts_with("at ")).collect::<Vec<_>>().join("\n");
                if strip(&actual) != strip(&expected) {
                    failures.push(format!("{name}: output changed after formatting:\n{actual}"));
                }
            }
            failures
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The JIT must produce exactly the same results as the interpreter.
#[test]
fn golden_with_jit() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phx"))
        .collect();
    cases.sort();
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let failures = worker
        .spawn(move || {
            let mut failures = Vec::new();
            for case in &cases {
                let expected =
                    std::fs::read_to_string(case.with_extension("out")).unwrap_or_default().replace("\r\n", "\n");
                let actual = render_opts(&root, case, HashMap::new(), true).replace("\r\n", "\n");
                // Errors raised inside native code have a shorter stack trace.
                let strip =
                    |s: &str| s.lines().filter(|l| !l.trim_start().starts_with("at ")).collect::<Vec<_>>().join("\n");
                if strip(&actual) != strip(&expected) {
                    let name = case.file_name().unwrap().to_string_lossy().to_string();
                    failures.push(format!("{name} differs with the JIT:\n{actual}"));
                }
            }
            failures
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn bytecode_cache_round_trip() {
    let dir = std::env::temp_dir().join(format!("phx-cache-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phx");
    std::fs::write(&entry, "<?phx\necho 1 + 2;\n").unwrap();
    let first = phx_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(!first.from_cache);
    let second = phx_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(second.from_cache);
    let out = run(&second, RunOptions { html: false, limits: Default::default(), http: None, host: None });
    assert_eq!(out.output, "3");
    // Changing the source invalidates the entry.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&entry, "<?phx\necho 40 + 2;\n").unwrap();
    let third = phx_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(!third.from_cache);
    let out = run(&third, RunOptions { html: false, limits: Default::default(), http: None, host: None });
    assert_eq!(out.output, "42");
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_out(c: &phx_driver::Compiled) -> String {
    run(c, RunOptions { html: false, limits: Default::default(), http: None, host: None }).output
}

fn only_entry(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir.join(".phx-cache"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "bin"))
        .unwrap()
}

#[test]
fn cache_entries_cannot_be_forged_or_reused() {
    let dir = std::env::temp_dir().join(format!("phx-cache-forge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = dir.join("page.phx");
    std::fs::write(
        &page,
        "<?phx\nfunction is_admin(): bool { return false; }\necho is_admin() ? \"ADMIN\" : \"DENIED\";\n",
    )
    .unwrap();
    assert_eq!(run_out(&phx_driver::compile_cached(&dir, &page).ok().unwrap()), "DENIED");
    let bin = only_entry(&dir);

    // Same size, mtime put back: the content hash still notices.
    let mtime = std::fs::metadata(&page).unwrap().modified().unwrap();
    std::fs::write(
        &page,
        "<?phx\nfunction is_admin(): bool { return  true; }\necho is_admin() ? \"ADMIN\" : \"DENIED\";\n",
    )
    .unwrap();
    std::fs::File::options().write(true).open(&page).unwrap().set_modified(mtime).unwrap();
    assert_eq!(run_out(&phx_driver::compile_cached(&dir, &page).ok().unwrap()), "ADMIN");

    // A tampered entry (one flipped byte) is ignored.
    let mut bytes = std::fs::read(&bin).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 1;
    std::fs::write(&bin, &bytes).unwrap();
    let c = phx_driver::compile_cached(&dir, &page).ok().unwrap();
    assert!(!c.from_cache);

    // An entry built for one script is not used for another.
    let other = dir.join("other.phx");
    std::fs::write(&other, "<?phx\necho \"other\";\n").unwrap();
    let good = std::fs::read(&bin).unwrap();
    let other_bin = {
        phx_driver::compile_cached(&dir, &other).ok().unwrap();
        std::fs::read_dir(dir.join(".phx-cache"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "bin") && *p != bin)
            .unwrap()
    };
    std::fs::write(&other_bin, &good).unwrap();
    let c = phx_driver::compile_cached(&dir, &other).ok().unwrap();
    assert!(!c.from_cache);
    assert_eq!(run_out(&c), "other");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn cache_does_not_follow_links() {
    let dir = std::env::temp_dir().join(format!("phx-cache-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    std::fs::write(dir.join("outside/.gitignore"), "keep me").unwrap();
    std::os::unix::fs::symlink(dir.join("outside"), dir.join("proj/.phx-cache")).unwrap();
    let page = dir.join("proj/page.phx");
    std::fs::write(&page, "<?phx\necho 1;\n").unwrap();
    phx_driver::compile_cached(&dir.join("proj"), &page).ok().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("outside/.gitignore")).unwrap(), "keep me");
    assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn cache_does_not_follow_junctions() {
    let dir = std::env::temp_dir().join(format!("phx-cache-junction-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    std::fs::write(dir.join("outside/.gitignore"), "keep me").unwrap();
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(dir.join("proj/.phx-cache"))
        .arg(dir.join("outside"))
        .output()
        .is_ok_and(|o| o.status.success());
    if made {
        let page = dir.join("proj/page.phx");
        std::fs::write(&page, "<?phx\necho 1;\n").unwrap();
        phx_driver::compile_cached(&dir.join("proj"), &page).ok().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("outside/.gitignore")).unwrap(), "keep me");
        assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 1);
        let _ = std::fs::remove_dir(dir.join("proj/.phx-cache"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inline_code_warnings_follow_the_policy() {
    let dir = std::env::temp_dir().join(format!("phx-csp-warn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = dir.join("page.phx");
    std::fs::write(&page, "<?phx\n?>\n<button onclick=\"go()\">x</button>\n").unwrap();
    let warned = phx_driver::compile(&dir, &page).ok().unwrap();
    assert!(warned.warnings.contains("`onclick`"), "{}", warned.warnings);
    std::fs::write(dir.join("phx.toml"), "[security]\ncsp = \"off\"\n").unwrap();
    let quiet = phx_driver::compile(&dir, &page).ok().unwrap();
    assert!(quiet.warnings.is_empty(), "{}", quiet.warnings);
    let _ = std::fs::remove_dir_all(&dir);
}
