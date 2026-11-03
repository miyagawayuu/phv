//! `phv build`: a site and a script compiled into executables that run
//! without their sources, and a damaged bundle that is refused.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn phv(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_phv")).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

#[test]
fn built_sites_and_scripts_run_without_their_sources() {
    let base = std::env::temp_dir().join(format!("phv-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let site = base.join("site");
    write(&site, "phv.toml", "[permissions]\nwrite = [\"data/\"]\n");
    write(&site, "lib/greet.phv", "export function greet(string $n): string { return \"Hello, \" . $n; }\n");
    write(&site, "views/page.phv.html", "<?phv params(string $text); ?><h1><?= $text ?></h1>\n");
    write(
        &site,
        "public/index.phv",
        "<?phv\nimport lib.greet.greet;\necho render(\"views/page.phv.html\", text: greet(\"site\"));\n",
    );
    write(&site, "public/about/index.phv", "<?phv echo \"about\";");
    write(&site, "public/style.css", "body { color: red; }");
    write(&site, "public/.hidden", "secret");
    write(&site, "public/_partial.phv", "<?phv echo \"partial\";");
    let exe = base.join(if cfg!(windows) { "site-server.exe" } else { "site-server" });
    let (ok, out) = phv(&["build", &site.to_string_lossy(), "-o", &exe.to_string_lossy()]);
    assert!(ok && out.contains("2 pages, 1 files"), "{out}");

    // The sources are gone; the executable runs from another directory.
    std::fs::remove_dir_all(&site).unwrap();
    let run_dir = base.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = Command::new(&exe)
        .args(["--port", &port.to_string(), "--prod"])
        .current_dir(&run_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let home = loop {
        if let Some(r) = get(port, "/") {
            break r;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the built site did not start");
        std::thread::sleep(Duration::from_millis(100));
    };
    let pages = [get(port, "/about"), get(port, "/style.css"), get(port, "/.hidden"), get(port, "/_partial")];
    let _ = child.kill();
    let _ = child.wait();
    assert!(home.starts_with("HTTP/1.1 200") && home.contains("<h1>Hello, site</h1>"), "{home}");
    let [about, css, hidden, partial] = pages.map(Option::unwrap);
    assert!(about.ends_with("about"), "{about}");
    assert!(css.contains("text/css") && css.ends_with("body { color: red; }"), "{css}");
    assert!(hidden.starts_with("HTTP/1.1 404") && partial.starts_with("HTTP/1.1 404"), "{hidden}\n{partial}");

    // A script.
    write(&base, "tool/main.phv", "<?phv\necho \"tool \", 6 * 7, \"\\n\";\n");
    let tool = base.join(if cfg!(windows) { "tool-bin.exe" } else { "tool-bin" });
    let (ok, out) = phv(&["build", &base.join("tool/main.phv").to_string_lossy(), "-o", &tool.to_string_lossy()]);
    assert!(ok, "{out}");
    let out = Command::new(&tool).current_dir(&run_dir).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "tool 42\n");

    // A damaged bundle is refused.
    let mut bytes = std::fs::read(&tool).unwrap();
    let n = bytes.len();
    bytes[n - 60] ^= 0xff;
    let broken = base.join(if cfg!(windows) { "broken.exe" } else { "broken" });
    std::fs::write(&broken, &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = Command::new(&broken).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("damaged"));
    let _ = std::fs::remove_dir_all(&base);
}
