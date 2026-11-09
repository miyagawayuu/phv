//! `std.i18n`: the golden cases of the project `tests/i18n` (each `*.phv`
//! there, compared with its `.out`; `PHV_BLESS=1` rewrites them), the locale
//! of a request, and the catalogs checked when a program is compiled.

use phv_driver::{compile, run, RunOptions};
use phv_vm::HttpCtx;
use std::path::{Path, PathBuf};

fn project_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/i18n")
}

fn render(root: &Path, entry: &Path, http: Option<HttpCtx>) -> String {
    match compile(root, entry) {
        Err(e) => e.render(false),
        Ok(c) => {
            let outcome = run(&c, RunOptions { html: false, limits: Default::default(), http, host: None });
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
    let root = project_dir();
    let bless = std::env::var_os("PHV_BLESS").is_some();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
        .collect();
    cases.sort();
    assert!(cases.len() >= 5, "the cases of {}", root.display());
    let mut failures = Vec::new();
    for case in &cases {
        let actual = render(&root, case, None).replace("\r\n", "\n");
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
    assert!(failures.is_empty(), "{} case(s) failed:\n\n{}", failures.len(), failures.join("\n"));
}

/// The locale of a request: from `Accept-Language` (only a listed one), or
/// the one the page chose; kept in the request, never between requests.
#[test]
fn locale_of_a_request() {
    let root = project_dir();
    let c = compile(&root, &root.join("locale.phv")).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let ask = |headers: &[(&str, &str)], query: &[(&str, &str)]| {
        let http = HttpCtx {
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let outcome = run(&c, RunOptions { html: false, limits: Default::default(), http: Some(http), host: None });
        assert!(outcome.error.is_none(), "{:?}", outcome.error.map(|e| e.render()));
        (outcome.output, outcome.http.locale)
    };
    assert_eq!(ask(&[], &[]).0, "en Shop");
    assert_eq!(ask(&[("accept-language", "ja-JP,ja;q=0.9,en;q=0.8")], &[]).0, "ja ショップ");
    assert_eq!(ask(&[("accept-language", "pt-BR")], &[]).0, "pt-BR Lojinha");
    assert_eq!(ask(&[("accept-language", "pt-PT, en;q=0.5")], &[]).0, "pt Loja");
    assert_eq!(ask(&[("accept-language", "de-DE, ru;q=0.3")], &[]).0, "ru Магазин");
    // Never a locale that is not listed.
    assert_eq!(ask(&[("accept-language", "de-DE, it;q=0.3")], &[]).0, "en Shop");
    assert_eq!(ask(&[("accept-language", "<script>alert(1)</script>")], &[]).0, "en Shop");
    // Two fields are read as one.
    assert_eq!(ask(&[("accept-language", "xx"), ("accept-language", "fr")], &[]).0, "fr Boutique");
    // The page's choice wins, if it is listed.
    assert_eq!(ask(&[("accept-language", "ja")], &[("lang", "AR")]).0, "ar متجر");
    assert_eq!(ask(&[("accept-language", "ja")], &[("lang", "../../etc")]).0, "ja ショップ");
    // The request keeps it (a page after its middleware gets the same one).
    assert_eq!(ask(&[("accept-language", "fr")], &[]).1.as_deref(), Some("fr"));
    // A huge header is read up to its first KB, without an error.
    let long = format!("ja,{}", "xx-YY;q=0.5,".repeat(10_000));
    assert_eq!(ask(&[("accept-language", &long)], &[]).0, "ja ショップ");
}

/// A temporary project with these files.
fn project(files: &[(&str, &str)]) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("phv-i18n-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

const TOML: &str = "[i18n]\ndefault = \"en\"\nlocales = [\"en\", \"ja\"]\n";
const MAIN: &str = "import std.i18n.t;\necho t(\"hi\");\n";

/// The errors of compiling `main.phv` of a project with these files.
fn errors(files: &[(&str, &str)]) -> String {
    let dir = project(files);
    let out = match compile(&dir, &dir.join("main.phv")) {
        Ok(_) => String::from("compiled"),
        Err(e) => e.render(false),
    };
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn catalogs_are_checked_when_compiled() {
    let ok = errors(&[
        ("phv.toml", TOML),
        ("main.phv", MAIN),
        ("lang/en.toml", "hi = \"Hi\""),
        ("lang/ja.toml", "hi = \"やあ\""),
    ]);
    assert_eq!(ok, "compiled");
    // No [i18n].
    let e = errors(&[("main.phv", MAIN)]);
    assert!(e.contains("std.i18n needs an [i18n] section in phv.toml"), "{e}");
    // A catalog that is missing.
    let e = errors(&[("phv.toml", TOML), ("main.phv", MAIN), ("lang/en.toml", "hi = \"Hi\"")]);
    assert!(e.contains("the catalog of `ja` not found (looked for lang/ja.toml)"), "{e}");
    // A malformed catalog: the error is shown in it.
    let e = errors(&[
        ("phv.toml", TOML),
        ("main.phv", MAIN),
        ("lang/en.toml", "hi = \"Hi\""),
        ("lang/ja.toml", "# ok\nhi = \"やあ\nbye = 1\n"),
    ]);
    assert!(e.contains("lang/ja.toml:2"), "{e}");
    assert!(e.contains("not valid TOML"), "{e}");
    let e = errors(&[
        ("phv.toml", TOML),
        ("main.phv", MAIN),
        ("lang/en.toml", "hi = \"Hi\""),
        ("lang/ja.toml", "hi = \"やあ\"\nbye = \"じゃあ\"\n"),
    ]);
    assert!(e.contains("lang/ja.toml:2") && e.contains("`bye` is not in the catalog of the default locale"), "{e}");
    // The key of the page is checked against the catalog.
    let e = errors(&[
        ("phv.toml", TOML),
        ("main.phv", "import std.i18n.t;\necho t(\"bye\");\n"),
        ("lang/en.toml", "hi = \"Hi\""),
        ("lang/ja.toml", ""),
    ]);
    assert!(e.contains("there is no message `bye` in lang/en.toml"), "{e}");
    // Another directory.
    let toml = format!("{TOML}dir = \"i18n/messages\"\n");
    let ok = errors(&[
        ("phv.toml", &toml),
        ("main.phv", MAIN),
        ("i18n/messages/en.toml", "hi = \"Hi\""),
        ("i18n/messages/ja.toml", ""),
    ]);
    assert_eq!(ok, "compiled");
    // A program without std.i18n does not read the catalogs.
    let ok = errors(&[("phv.toml", TOML), ("main.phv", "echo 1;\n")]);
    assert_eq!(ok, "compiled");
}

#[test]
fn settings_are_checked() {
    let with = |toml: &str| errors(&[("phv.toml", toml), ("main.phv", "echo 1;")]);
    let e = with("[i18n]\ndefault = \"en\"\nlocales = []\n");
    assert!(e.contains("[i18n].locales: list 1 to 32 locales"), "{e}");
    let many: Vec<String> = (0..33).map(|i| format!("\"x{}\"", (b'a' + (i % 26) as u8) as char)).collect();
    let e = with(&format!("[i18n]\ndefault = \"xa\"\nlocales = [{}]\n", many.join(",")));
    assert!(e.contains("list 1 to 32 locales"), "{e}");
    let e = with("[i18n]\ndefault = \"en\"\nlocales = [\"en\", \"en_US\"]\n");
    assert!(e.contains("\"en_US\" is not a locale"), "{e}");
    let e = with("[i18n]\ndefault = \"en\"\nlocales = [\"en\", \"../x\"]\n");
    assert!(e.contains("is not a locale"), "{e}");
    // Its catalog's name would be a device on Windows.
    let e = with("[i18n]\ndefault = \"en\"\nlocales = [\"en\", \"con\"]\n");
    assert!(e.contains("\"con\" is not a locale"), "{e}");
    let e = with("[i18n]\ndefault = \"en\"\nlocales = [\"en\", \"ja\", \"en\"]\n");
    assert!(e.contains("listed twice"), "{e}");
    let e = with("[i18n]\ndefault = \"de\"\nlocales = [\"en\"]\n");
    assert!(e.contains("[i18n].default: \"de\" must be one of"), "{e}");
    for dir in ["../lang", "/etc", "", "a/../b", "LANG~1"] {
        let e = with(&format!("{TOML}dir = \"{dir}\"\n"));
        assert!(e.contains("[i18n].dir"), "{dir}: {e}");
    }
    let e = with("[i18n]\ndefault = \"en\"\nlocales = [\"en\"]\nfallback = \"x\"\n");
    assert!(e.contains("unknown field"), "{e}");
}

#[test]
fn catalog_size_limits() {
    let big = format!("hi = \"Hi\"\n# {}\n", "x".repeat(phv_std::i18n::MAX_CATALOG_BYTES));
    let e = errors(&[("phv.toml", TOML), ("main.phv", MAIN), ("lang/en.toml", &big), ("lang/ja.toml", "")]);
    assert!(e.contains("the catalog of `en` (lang/en.toml) is larger than 2 MB"), "{e}");
    // 16 MB in all: nine catalogs of almost 2 MB.
    let locales: Vec<String> = (0..9).map(|i| format!("x{}", (b'a' + i) as char)).collect();
    let quoted: Vec<String> = locales.iter().map(|l| format!("\"{l}\"")).collect();
    let toml = format!("[i18n]\ndefault = \"xa\"\nlocales = [{}]\n", quoted.join(", "));
    let fill = format!("hi = \"Hi\"\n# {}\n", "x".repeat(phv_std::i18n::MAX_CATALOG_BYTES - 100));
    let mut files: Vec<(String, String)> = vec![("phv.toml".into(), toml), ("main.phv".into(), MAIN.into())];
    for l in &locales {
        files.push((format!("lang/{l}.toml"), fill.clone()));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let e = errors(&refs);
    assert!(e.contains("the catalogs of [i18n] have more than 16 MB in all"), "{e}");
}

/// A change to a catalog makes the program stale (it is compiled again), and
/// the programs of a process share the catalogs they were built from.
#[test]
fn catalogs_are_dependencies_and_shared() {
    let dir = project(&[
        ("phv.toml", TOML),
        ("main.phv", MAIN),
        ("other.phv", "import std.i18n.t;\necho t(\"hi\"), \"!\";\n"),
        ("lang/en.toml", "hi = \"Hi\""),
        ("lang/ja.toml", ""),
    ]);
    let a = compile(&dir, &dir.join("main.phv")).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let b = compile(&dir, &dir.join("other.phv")).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let (ca, cb) = (a.program.i18n.as_ref().unwrap(), b.program.i18n.as_ref().unwrap());
    assert!(std::sync::Arc::ptr_eq(&ca.0, &cb.0));
    assert!(!a.is_stale());
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.join("lang/ja.toml"), "hi = \"やあ\"\n").unwrap();
    assert!(a.deps.iter().any(|d| !d.is_fresh(true)));
    let c = compile(&dir, &dir.join("main.phv")).unwrap_or_else(|e| panic!("{}", e.render(false)));
    assert!(!std::sync::Arc::ptr_eq(&ca.0, &c.program.i18n.as_ref().unwrap().0));
    let _ = std::fs::remove_dir_all(&dir);
}
