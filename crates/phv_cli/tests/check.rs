//! `phv check`: one file, or several in one process, each checked on its own.

use std::path::Path;
use std::process::Command;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn phv(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_phv")).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn several_files_are_checked_each_on_its_own() {
    let dir = std::env::temp_dir().join(format!("phv-check-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Two projects: each file is checked against its own root.
    write(&dir, "a/phv.toml", "");
    write(&dir, "a/lib/m.phv", "export function f(): int {\n    return 1;\n}\n");
    write(&dir, "a/page.phv", "<?phv\nimport lib.m.f;\necho f();\n");
    write(&dir, "b/phv.toml", "");
    write(&dir, "b/lib/m.phv", "export function g(): string {\n    return \"x\";\n}\n");
    write(&dir, "b/page.phv", "<?phv\nimport lib.m.g;\necho g();\n");
    write(&dir, "b/bad.phv", "<?phv\n$n = 1 + \"x\";\n");
    let a = dir.join("a/page.phv").to_string_lossy().into_owned();
    let b = dir.join("b/page.phv").to_string_lossy().into_owned();
    let bad = dir.join("b/bad.phv").to_string_lossy().into_owned();

    let (ok, out, _) = phv(&["check", &a]);
    assert!(ok, "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");

    let (ok, out, err) = phv(&["check", &a, &b]);
    assert!(ok, "{out}{err}");
    assert_eq!(out, format!("ok: no errors in {a}\nok: no errors in {b}\n"));

    // An error in one: the others are still checked, and the exit status is an error.
    let (ok, out, err) = phv(&["check", &a, &bad, &b]);
    assert!(!ok);
    assert!(out.contains(&format!("ok: no errors in {a}")) && out.contains(&format!("ok: no errors in {b}")), "{out}");
    assert!(!out.contains("bad.phv"), "{out}");
    assert!(err.contains("bad.phv") && err.contains("1 error found"), "{err}");

    // No file at all is a usage error.
    let (ok, _, err) = phv(&["check"]);
    assert!(!ok);
    assert!(err.contains("<FILES>"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
