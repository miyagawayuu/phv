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

/// Makes `link` a link to the directory `target`: a symbolic link, or on
/// Windows a junction when symbolic links need privileges.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
            || Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
    }
}

/// What a repository or a package can plant is refused: links at vendor/ and
/// phv.lock (which would make the install write elsewhere), packages of
/// packages that are not https://, uppercase names, and huge manifests.
#[test]
fn refuses_what_a_repository_or_package_plants() {
    let base: PathBuf = std::env::temp_dir().join(format!("phv-install-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    repo(&base.join("pkg"), &[("v1.0.0", &[("m.phv", "export function f(): int { return 1; }\n")])]);
    let pkg = url(&base.join("pkg"));
    // Not a local registry: packages of packages must be https://.
    let registry = "https://registry.invalid/index.git";
    let app = base.join("app");
    let app_s = app.to_string_lossy().to_string();
    write(&app, "phv.toml", &format!("[dependencies]\npkg = {{ git = \"{pkg}\", version = \"^1\" }}\n"));

    // vendor/ is a link to a directory outside the project.
    let outside = base.join("outside");
    write(&outside, "pkg/precious.txt", "keep me");
    if link_dir(&outside, &app.join("vendor")) {
        let (ok, out) = phv(&["install", &app_s], registry);
        assert!(!ok && out.contains("is a link"), "{out}");
        assert_eq!(std::fs::read_to_string(outside.join("pkg/precious.txt")).unwrap(), "keep me");
        std::fs::remove_dir(app.join("vendor")).or_else(|_| std::fs::remove_file(app.join("vendor"))).unwrap();
    } else {
        eprintln!("skipped the vendor link: cannot create links here");
    }

    // phv.lock is a link to a file outside the project.
    write(&outside, "dotfile", "keep me too");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(outside.join("dotfile"), app.join("phv.lock")).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(outside.join("dotfile"), app.join("phv.lock")).is_ok();
    if linked {
        let (ok, out) = phv(&["install", &app_s], registry);
        assert!(!ok && out.contains("phv.lock is a link"), "{out}");
        assert_eq!(std::fs::read_to_string(outside.join("dotfile")).unwrap(), "keep me too");
        std::fs::remove_file(app.join("phv.lock")).unwrap();
    } else {
        eprintln!("skipped the phv.lock link: cannot create symbolic links here");
    }

    // A package asking for a package over file:// (or ssh://), an uppercase
    // name, a huge phv.toml.
    let local = format!("[dependencies]\nother = {{ git = \"{pkg}\", version = \"^1\" }}\n");
    let upper = "[dependencies]\nOther = { git = \"https://example.invalid/o.git\" }\n".to_string();
    let huge = format!("# {}\n", "x".repeat(2 << 20));
    for (i, (manifest, expect)) in
        [(local.as_str(), "not https://"), (upper.as_str(), "invalid package name"), (huge.as_str(), "at most 1 MB")]
            .into_iter()
            .enumerate()
    {
        let name = format!("bad{i}");
        repo(&base.join(&name), &[("v1.0.0", &[("phv.toml", manifest), ("m.phv", "export const X = 1;\n")])]);
        let toml = format!("[dependencies]\n{name} = {{ git = \"{}\", version = \"^1\" }}\n", url(&base.join(&name)));
        write(&app, "phv.toml", &toml);
        let (ok, out) = phv(&["install", &app_s], registry);
        assert!(!ok && out.contains(expect), "{name}: {out}");
        assert!(!out.contains("xxxxxxxx"), "the manifest is echoed: {name}");
    }
    let _ = std::fs::remove_dir_all(&base);
}
