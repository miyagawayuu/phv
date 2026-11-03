//! `phv install` with local git repositories and a `file://` registry:
//! packages of packages, versions from tags, conflicts, `--locked`, and which
//! packages a module may import.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.email=test@example.com", "-c", "user.name=test", "-c", "init.defaultBranch=main"])
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A repository whose files are replaced and tagged for each version.
fn repo(dir: &Path, versions: &[(&str, &[(&str, &str)])]) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet"]);
    for (tag, files) in versions {
        for (rel, text) in *files {
            write(dir, rel, text);
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "-m", tag]);
        git(dir, &["tag", tag]);
    }
}

fn url(dir: &Path) -> String {
    format!("file:///{}", dir.to_string_lossy().replace('\\', "/").trim_start_matches('/'))
}

fn phv(args: &[&str], registry: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_phv"))
        .args(args)
        .env("PHV_REGISTRY", registry)
        .env("LOCALAPPDATA", std::env::temp_dir().join(format!("phv-install-cache-{}", std::process::id())))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn installs_packages_of_packages() {
    let base: PathBuf = std::env::temp_dir().join(format!("phv-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let shout = |suffix: &str| {
        format!("export function shout(string $s): string {{ return strtoupper($s) . \"{suffix}\"; }}\n")
    };
    let (v1, v11, v2) = (shout("!"), shout("!!"), shout("?"));
    repo(
        &base.join("strutil"),
        &[
            ("v1.0.0", &[("text.phv", v1.as_str())]),
            ("v1.1.0", &[("text.phv", v11.as_str())]),
            ("v2.0.0", &[("text.phv", v2.as_str())]),
        ],
    );
    let greet_toml =
        format!("[dependencies]\nstrutil = {{ git = \"{}\", version = \"^1.0\" }}\n", url(&base.join("strutil")));
    let hello =
        "import strutil.text.shout;\nexport function hello(string $n): string { return shout(\"hello \" . $n); }\n";
    repo(&base.join("greet"), &[("v0.1.0", &[("phv.toml", greet_toml.as_str()), ("hello.phv", hello)])]);
    let entry = format!("git = \"{}\"\ndescription = \"Greetings\"\n", url(&base.join("greet")));
    repo(&base.join("registry"), &[("v1", &[("index/greet.toml", entry.as_str())])]);
    let registry = url(&base.join("registry"));

    let app = base.join("app");
    write(&app, "phv.toml", "[dependencies]\ngreet = \"^0.1\"\n");
    write(&app, "main.phv", "<?phv\nimport greet.hello.hello;\necho hello(\"ann\");\n");
    let app_s = app.to_string_lossy().to_string();

    // The registry package and the package it needs, at the newest fitting version.
    let (ok, out) = phv(&["install", &app_s], &registry);
    assert!(ok, "{out}");
    assert!(out.contains("greet: v0.1.0") && out.contains("strutil: v1.1.0"), "{out}");
    let lock = std::fs::read_to_string(app.join("phv.lock")).unwrap();
    assert!(lock.contains("name = \"strutil\"") && lock.contains("version = \"1.1.0\""), "{lock}");
    let (ok, out) = phv(&["run", &app.join("main.phv").to_string_lossy()], &registry);
    assert!(ok && out.contains("HELLO ANN!!"), "{out}");

    // `--locked` installs what phv.lock says.
    let (ok, out) = phv(&["install", "--locked", &app_s], &registry);
    assert!(ok, "{out}");

    // The project may not use a package it does not depend on itself.
    write(&app, "direct.phv", "<?phv\nimport strutil.text.shout;\necho shout(\"x\");\n");
    let (ok, out) = phv(&["check", &app.join("direct.phv").to_string_lossy()], &registry);
    assert!(!ok && out.contains("package `strutil` is not a dependency of this project"), "{out}");

    // Two packages that want versions no tag satisfies together.
    let conflict = format!(
        "[dependencies]\ngreet = \"^0.1\"\nstrutil = {{ git = \"{}\", version = \"^2.0\" }}\n",
        url(&base.join("strutil"))
    );
    write(&app, "phv.toml", &conflict);
    let (ok, out) = phv(&["install", &app_s], &registry);
    assert!(!ok && out.contains("no version fits every requirement") && out.contains("^2.0 (phv.toml)"), "{out}");
    assert!(out.contains("^1.0 (greet)"), "{out}");
    let (ok, out) = phv(&["install", "--locked", &app_s], &registry);
    assert!(!ok, "{out}");

    // Unknown registry packages are reported.
    write(&app, "phv.toml", "[dependencies]\nnothere = \"^1\"\n");
    let (ok, out) = phv(&["install", &app_s], &registry);
    assert!(!ok && out.contains("no package \"nothere\" in the registry"), "{out}");
    let _ = std::fs::remove_dir_all(&base);
}
