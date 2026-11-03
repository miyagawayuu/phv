//! Golden tests: every `tests/cases/*.phv` is compiled and run, and the result
//! (output, runtime error or compile errors) is compared with the `.out` file
//! next to it. Set `PHV_BLESS=1` to rewrite the expected files.

use phv_driver::{compile_with_overlay, run, RunOptions};
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
    let bless = std::env::var_os("PHV_BLESS").is_some();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
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

/// `phv fmt` must not change what any program prints.
#[test]
fn formatting_preserves_behavior() {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
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
                let formatted = phv_syntax::format::format_source(&src).unwrap();
                let again = phv_syntax::format::format_source(&formatted).unwrap();
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
        .filter(|p| p.extension().is_some_and(|e| e == "phv"))
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
    let dir = std::env::temp_dir().join(format!("phv-cache-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, "<?phv\necho 1 + 2;\n").unwrap();
    let first = phv_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(!first.from_cache);
    let second = phv_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(second.from_cache);
    let out = run(&second, RunOptions { html: false, limits: Default::default(), http: None, host: None });
    assert_eq!(out.output, "3");
    // Changing the source invalidates the entry.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&entry, "<?phv\necho 40 + 2;\n").unwrap();
    let third = phv_driver::compile_cached(&dir, &entry).ok().unwrap();
    assert!(!third.from_cache);
    let out = run(&third, RunOptions { html: false, limits: Default::default(), http: None, host: None });
    assert_eq!(out.output, "42");
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_out(c: &phv_driver::Compiled) -> String {
    run(c, RunOptions { html: false, limits: Default::default(), http: None, host: None }).output
}

fn only_entry(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir.join(".phv-cache"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "bin"))
        .unwrap()
}

#[test]
fn cache_entries_cannot_be_forged_or_reused() {
    let dir = std::env::temp_dir().join(format!("phv-cache-forge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = dir.join("page.phv");
    std::fs::write(
        &page,
        "<?phv\nfunction is_admin(): bool { return false; }\necho is_admin() ? \"ADMIN\" : \"DENIED\";\n",
    )
    .unwrap();
    assert_eq!(run_out(&phv_driver::compile_cached(&dir, &page).ok().unwrap()), "DENIED");
    let bin = only_entry(&dir);

    // Same size, mtime put back: the content hash still notices.
    let mtime = std::fs::metadata(&page).unwrap().modified().unwrap();
    std::fs::write(
        &page,
        "<?phv\nfunction is_admin(): bool { return  true; }\necho is_admin() ? \"ADMIN\" : \"DENIED\";\n",
    )
    .unwrap();
    std::fs::File::options().write(true).open(&page).unwrap().set_modified(mtime).unwrap();
    assert_eq!(run_out(&phv_driver::compile_cached(&dir, &page).ok().unwrap()), "ADMIN");

    // A tampered entry (one flipped byte) is ignored.
    let mut bytes = std::fs::read(&bin).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 1;
    std::fs::write(&bin, &bytes).unwrap();
    let c = phv_driver::compile_cached(&dir, &page).ok().unwrap();
    assert!(!c.from_cache);

    // An entry built for one script is not used for another.
    let other = dir.join("other.phv");
    std::fs::write(&other, "<?phv\necho \"other\";\n").unwrap();
    let good = std::fs::read(&bin).unwrap();
    let other_bin = {
        phv_driver::compile_cached(&dir, &other).ok().unwrap();
        std::fs::read_dir(dir.join(".phv-cache"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "bin") && *p != bin)
            .unwrap()
    };
    std::fs::write(&other_bin, &good).unwrap();
    let c = phv_driver::compile_cached(&dir, &other).ok().unwrap();
    assert!(!c.from_cache);
    assert_eq!(run_out(&c), "other");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn cache_does_not_follow_links() {
    let dir = std::env::temp_dir().join(format!("phv-cache-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    std::fs::write(dir.join("outside/.gitignore"), "keep me").unwrap();
    std::os::unix::fs::symlink(dir.join("outside"), dir.join("proj/.phv-cache")).unwrap();
    let page = dir.join("proj/page.phv");
    std::fs::write(&page, "<?phv\necho 1;\n").unwrap();
    phv_driver::compile_cached(&dir.join("proj"), &page).ok().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("outside/.gitignore")).unwrap(), "keep me");
    assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn cache_does_not_follow_junctions() {
    let dir = std::env::temp_dir().join(format!("phv-cache-junction-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    std::fs::write(dir.join("outside/.gitignore"), "keep me").unwrap();
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(dir.join("proj/.phv-cache"))
        .arg(dir.join("outside"))
        .output()
        .is_ok_and(|o| o.status.success());
    if made {
        let page = dir.join("proj/page.phv");
        std::fs::write(&page, "<?phv\necho 1;\n").unwrap();
        phv_driver::compile_cached(&dir.join("proj"), &page).ok().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("outside/.gitignore")).unwrap(), "keep me");
        assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 1);
        let _ = std::fs::remove_dir(dir.join("proj/.phv-cache"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inline_code_warnings_follow_the_policy() {
    let dir = std::env::temp_dir().join(format!("phv-csp-warn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = dir.join("page.phv");
    std::fs::write(&page, "<?phv\n?>\n<button onclick=\"go()\">x</button>\n").unwrap();
    let warned = phv_driver::compile(&dir, &page).ok().unwrap();
    assert!(warned.warnings.contains("`onclick`"), "{}", warned.warnings);
    std::fs::write(dir.join("phv.toml"), "[security]\ncsp = \"off\"\n").unwrap();
    let quiet = phv_driver::compile(&dir, &page).ok().unwrap();
    assert!(quiet.warnings.is_empty(), "{}", quiet.warnings);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A function needing more registers than a frame has is a compile error, not a crash.
#[test]
fn oversized_functions_are_errors() {
    let dir = std::env::temp_dir().join(format!("phv-oversized-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let locals: String = (0..70_000).map(|i| format!("$v{i} = {i};\n")).collect();
    let pairs: Vec<String> = (0..40_000).map(|i| format!("\"k{i}\" => {i}")).collect();
    // A property's slot is a 16-bit operand: property 65,536 used to alias property 0.
    let props: String = (0..65_536).map(|i| format!("    public int $f{i} = {i};\n")).collect();
    let cases = [
        (
            "props.phv",
            format!("<?phv\nclass C {{\n{props}}}\n$o = new C();\n$o->f65535 = 7;\necho $o->f0;\n"),
            "class `C` has too many properties (at most 65535, inherited ones included)",
        ),
        ("locals.phv", format!("<?phv\n{locals}"), "this script is too large to compile"),
        (
            "literal.phv",
            format!("<?phv\nfunction f(): int {{\n    return count([{}]);\n}}\necho f();\n", pairs.join(", ")),
            "`f` is too large to compile",
        ),
    ];
    for (name, src, message) in cases {
        let file = dir.join(name);
        std::fs::write(&file, src).unwrap();
        let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
        let (d, f) = (dir.clone(), file.clone());
        let rendered = worker
            .spawn(move || match phv_driver::compile(&d, &f) {
                Err(e) => e.render(false),
                Ok(_) => String::from("compiled"),
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(rendered.contains(message), "{name}: {rendered}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Constant expressions, type aliases and spreads cannot make checking or
/// compiling take exponential time or memory, or nest the compiler deeply.
#[test]
fn constants_aliases_and_spreads_stay_small() {
    let dir = std::env::temp_dir().join(format!("phv-v06-limits-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let doubling: String = (1..80).map(|i| format!("const C{i} = C{} . C{};\n", i - 1, i - 1)).collect();
    let aliases: String = (1..80).map(|i| format!("type T{i} = fn(T{}): T{};\n", i - 1, i - 1)).collect();
    let generic: String = (1..80).map(|i| format!("type Q{i} = P<Q{}>;\n", i - 1)).collect();
    let cases = [
        ("strings.phv", format!("<?phv\nconst C0 = \"xxxxxxxx\";\n{doubling}"), "this constant string is too long"),
        (
            "aliases.phv",
            format!("<?phv\ntype T0 = int;\n{aliases}"),
            "type alias `T13` stands for a type that is too large",
        ),
        (
            "generic.phv",
            format!("<?phv\ntype P<A> = fn(A): A;\ntype Q0 = int;\n{generic}"),
            "`P` here stands for a type that is too large",
        ),
        (
            "chain.phv",
            format!(
                "<?phv\n{}type A2000 = int;\nfunction f(A0 $x): void {{}}\n",
                (0..2000).map(|i| format!("type A{i} = A{};\n", i + 1)).collect::<String>()
            ),
            "is nested too deeply (at most 64 aliases inside each other)",
        ),
        (
            "uses.phv",
            format!(
                "<?phv\ntype M0 = int;\n{}{}",
                (1..13).map(|i| format!("type M{i} = fn(M{}): M{};\n", i - 1, i - 1)).collect::<String>(),
                (0..20_000).map(|i| format!("function g{i}(M12 $x): void {{}}\n")).collect::<String>()
            ),
            "the type aliases of this program stand for too much in total",
        ),
        (
            "total.phv",
            format!(
                "<?phv\nconst S0 = \"xxxxxxxxxxxxxxxx\";\n{}{}",
                (1..16).map(|i| format!("const S{i} = S{} . S{};\n", i - 1, i - 1)).collect::<String>(),
                (0..1000).map(|i| format!("const B{i} = S15 . S15;\n")).collect::<String>()
            ),
            "the constant strings of this program are too long in total",
        ),
        ("unicode.phv", format!("<?phv\necho \"{}\";\n", "\\u{".repeat(200_000)), "unterminated unicode escape"),
        (
            "union.phv",
            format!(
                "<?phv\n{}function f({} $x): void {{}}\n",
                (0..300).map(|i| format!("class K{i} {{}}\n")).collect::<String>(),
                (0..300).map(|i| format!("K{i}")).collect::<Vec<_>>().join("|")
            ),
            "a union type can have at most 256 members",
        ),
    ];
    for (name, src, message) in cases {
        let file = dir.join(name);
        std::fs::write(&file, src).unwrap();
        let rendered = match phv_driver::compile(&dir, &file) {
            Err(e) => e.render(false),
            Ok(_) => String::from("compiled"),
        };
        assert!(rendered.contains(message), "{name}: {rendered}");
    }
    // A constant string is shared by its uses: 3,000 uses of 1 MiB used to take 3 GB.
    let file = dir.join("shared.phv");
    let uses: String = (0..3000).map(|i| format!("const B{i} = A;\n")).collect();
    std::fs::write(&file, format!("<?phv\nconst A = \"{}\";\n{uses}echo strlen(B2999);\n", "x".repeat(1 << 20)))
        .unwrap();
    let c = phv_driver::compile(&dir, &file).unwrap_or_else(|e| panic!("{}", e.render(false)));
    assert_eq!(
        run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: None }).output,
        "1048576"
    );
    // 100,000 spreads in one literal are joined in pairs, not nested 100,000 deep.
    let file = dir.join("spreads.phv");
    std::fs::write(
        &file,
        format!("<?phv\n$a = [1];\n$b = [{}];\necho $b->len();\n", vec!["...$a"; 100_000].join(", ")),
    )
    .unwrap();
    let c = phv_driver::compile(&dir, &file).unwrap_or_else(|e| panic!("{}", e.render(false)));
    assert_eq!(
        run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: None }).output,
        "100000"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `std.mail` sends through `[mail.<name>]` to a local SMTP server.
#[test]
fn mail_reaches_a_local_server() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        w.write_all(b"220 hi\r\n").unwrap();
        let (mut rcpt, mut data) = (Vec::new(), String::new());
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let l = line.trim_end().to_string();
            if l == "DATA" {
                w.write_all(b"354 go\r\n").unwrap();
                loop {
                    let mut d = String::new();
                    r.read_line(&mut d).unwrap();
                    if d == ".\r\n" {
                        break;
                    }
                    data.push_str(&d);
                }
            } else if let Some(a) = l.strip_prefix("RCPT TO:") {
                rcpt.push(a.to_string());
            } else if l == "QUIT" {
                w.write_all(b"221 bye\r\n").unwrap();
                break;
            }
            w.write_all(b"250 ok\r\n").unwrap();
        }
        (rcpt, data)
    });
    let dir = std::env::temp_dir().join(format!("phv-mail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("phv.toml"),
        format!("[mail.default]\nurl = \"smtp://127.0.0.1:{port}\"\nfrom = \"Shop <shop@example.com>\"\n"),
    )
    .unwrap();
    let file = dir.join("send.phv");
    std::fs::write(
        &file,
        "<?phv\nimport std.mail.Mail;\nMail::to(\"ann@example.com\")->also(\"bob@example.com\")->subject(\"Your order\")\n    \
         ->text(\"Thanks!\")->html(raw(\"<p>Thanks!</p>\"))->send();\ntry {\n    Mail::to(\"x@y.z\r\nBcc: evil@e.x\")->send();\n} \
         catch (ValueError $e) {\n    echo $e->getMessage();\n}\n",
    )
    .unwrap();
    let c = phv_driver::compile(&dir, &file).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let host = phv_driver::load_config(&dir).unwrap().host;
    let out = run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: Some(host) });
    assert!(out.error.is_none(), "{}", out.error.map(|e| e.render()).unwrap_or_default());
    assert_eq!(out.output, "mail: an address cannot contain line breaks");
    let (rcpt, data) = server.join().unwrap();
    assert_eq!(rcpt, ["<ann@example.com>", "<bob@example.com>"]);
    assert!(data.contains("Subject: Your order") && data.contains("multipart/alternative"), "{data}");
    let _ = std::fs::remove_dir_all(&dir);
}
