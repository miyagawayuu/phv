//! Package rules the loader enforces: a package may not share its name with a
//! directory of the project, and a package's module must be inside it.

use phv_driver::compile;
use std::path::Path;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn errors(root: &Path, entry: &str) -> String {
    match compile(root, &root.join(entry)) {
        Ok(_) => String::new(),
        Err(e) => e.render(false),
    }
}

#[test]
fn package_names_cannot_shadow_project_directories() {
    let base = std::env::temp_dir().join(format!("phv-pkg-clash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    write(&base, "shared/posts.phv", "export function title(): string { return \"package\"; }\n");
    write(&app, "lib/posts.phv", "export function title(): string { return \"project\"; }\n");
    write(&app, "phv.toml", "[dependencies]\nlib = { path = \"../shared\" }\n");
    write(&app, "main.phv", "<?phv\nimport lib.posts.title;\necho title();\n");
    let out = errors(&app, "main.phv");
    assert!(out.contains("the package `lib` has the name of a directory or module of this project"), "{out}");
    // A path package that is that very directory is fine.
    write(&app, "phv.toml", "[dependencies]\nlib = { path = \"lib\" }\n");
    assert_eq!(errors(&app, "main.phv"), "");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn modules_of_a_package_stay_inside_it() {
    let base = std::env::temp_dir().join(format!("phv-pkg-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    write(&app, "internal/keys.phv", "export const KEY = \"s3cr3t\";\n");
    write(&app, "phv.toml", "[dependencies]\nevil = { path = \"../evil\" }\n");
    write(&app, "main.phv", "<?phv\nimport evil.stolen.KEY;\necho KEY;\n");
    std::fs::create_dir_all(base.join("evil")).unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(app.join("internal/keys.phv"), base.join("evil/stolen.phv")).is_ok();
    #[cfg(windows)]
    let linked =
        std::os::windows::fs::symlink_file(app.join("internal/keys.phv"), base.join("evil/stolen.phv")).is_ok();
    if !linked {
        eprintln!("skipped: cannot create symbolic links here");
        return;
    }
    let out = errors(&app, "main.phv");
    assert!(out.contains("module `evil.stolen` is outside the package `evil`"), "{out}");
    let _ = std::fs::remove_dir_all(&base);
}
