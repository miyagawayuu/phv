//! `phv test` and `phv fmt`.

use phv_driver::{compile_for_tests, find_root, load_config, run_test, RunOptions};
use phv_vm::Limits;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const SKIP_DIRS: &[&str] = &["target", "vendor", ".git", ".jj", "node_modules"];

/// Files under `paths` accepted by `want` (directories are walked).
pub fn collect(paths: &[PathBuf], want: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    collect_max(paths, want, usize::MAX)
}

/// `collect`, stopping once `max` files are found (a huge tree is not walked to the end).
pub fn collect_max(paths: &[PathBuf], want: &dyn Fn(&Path) -> bool, max: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, want: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>, max: usize) {
        if out.len() >= max {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        // Links (symlinks, junctions) are not followed: a loop would never end,
        // and `phv fmt` must not rewrite files outside the tree it was given.
        let mut entries: Vec<(PathBuf, std::fs::FileType)> =
            rd.filter_map(|e| e.ok().and_then(|e| Some((e.path(), e.file_type().ok()?)))).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (p, ty) in entries {
            if ty.is_symlink() {
                continue;
            }
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if out.len() >= max {
                return;
            }
            if ty.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    walk(&p, want, out, max);
                }
            } else if want(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if out.len() >= max {
            break;
        }
        if p.is_file() {
            out.push(p.clone());
        } else {
            walk(p, want, &mut out, max);
        }
    }
    out
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// What `phv test` was asked for besides the files.
pub struct TestOptions {
    /// Only tests whose name contains this.
    pub filter: Option<String>,
    /// One JSON document on stdout instead of the text report.
    pub json: bool,
    /// Also write a JUnit XML report here.
    pub junit: Option<PathBuf>,
    /// Report which statements of the project's files the tests ran.
    pub coverage: bool,
    pub color: bool,
}

/// One test's result.
struct TestResult {
    file: String,
    name: String,
    /// "pass", "fail" or "skip".
    status: &'static str,
    seconds: f64,
    /// The error, or the reason for a skip.
    message: String,
    output: String,
}

/// Statements of one source file that can run, and those that did.
#[derive(Default)]
struct FileCoverage {
    line_starts: Vec<u32>,
    all: std::collections::BTreeSet<u32>,
    ran: std::collections::BTreeSet<u32>,
}

impl FileCoverage {
    fn line_of(&self, off: u32) -> usize {
        self.line_starts.partition_point(|s| *s <= off)
    }

    /// The lines with a statement that did not run, as `3, 7-9`.
    fn missed_lines(&self) -> String {
        let mut lines: Vec<usize> = self.all.difference(&self.ran).map(|o| self.line_of(*o)).collect();
        lines.dedup();
        let mut parts: Vec<String> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let mut j = i;
            while j + 1 < lines.len() && lines[j + 1] == lines[j] + 1 {
                j += 1;
            }
            parts.push(if i == j { lines[i].to_string() } else { format!("{}-{}", lines[i], lines[j]) });
            i = j + 1;
        }
        parts.join(", ")
    }

    fn percent(&self) -> f64 {
        if self.all.is_empty() {
            100.0
        } else {
            self.ran.len() as f64 * 100.0 / self.all.len() as f64
        }
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Characters XML 1.0 cannot hold.
            c if ((c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t')) || matches!(c, '\u{FFFE}' | '\u{FFFF}') => {
                out.push('\u{FFFD}')
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[test]
fn junit_xml_holds_only_xml_characters() {
    assert_eq!(xml_escape("a\u{1}b\u{FFFF}c\u{FFFE}<&>\"\n"), "a\u{FFFD}b\u{FFFD}c\u{FFFD}&lt;&amp;&gt;&quot;\n");
}

fn junit(results: &[TestResult], seconds: f64) -> String {
    let mut files: Vec<&str> = Vec::new();
    for r in results {
        if !files.contains(&r.file.as_str()) {
            files.push(&r.file);
        }
    }
    let count = |rs: &[&TestResult], s: &str| rs.iter().filter(|r| r.status == s).count();
    let all: Vec<&TestResult> = results.iter().collect();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"phv test\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{seconds:.3}\">\n",
        all.len(),
        count(&all, "fail"),
        count(&all, "skip")
    );
    for file in files {
        let rs: Vec<&TestResult> = results.iter().filter(|r| r.file == file).collect();
        let time: f64 = rs.iter().map(|r| r.seconds).sum();
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{time:.3}\">\n",
            xml_escape(file),
            rs.len(),
            count(&rs, "fail"),
            count(&rs, "skip")
        ));
        for r in rs {
            out.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"",
                xml_escape(file),
                xml_escape(&r.name),
                r.seconds
            ));
            match r.status {
                "fail" => {
                    let first = r.message.lines().next().unwrap_or("");
                    out.push_str(&format!(
                        ">\n      <failure message=\"{}\">{}</failure>\n",
                        xml_escape(first),
                        xml_escape(&r.message)
                    ));
                    if !r.output.is_empty() {
                        out.push_str(&format!("      <system-out>{}</system-out>\n", xml_escape(&r.output)));
                    }
                    out.push_str("    </testcase>\n");
                }
                "skip" => out.push_str(&format!(
                    ">\n      <skipped message=\"{}\"/>\n    </testcase>\n",
                    xml_escape(&r.message)
                )),
                _ => out.push_str("/>\n"),
            }
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

pub fn test(paths: Vec<PathBuf>, opts: TestOptions) -> ExitCode {
    let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths };
    let files = collect(&paths, &|p| name_of(p).ends_with("_test.phv"));
    let text = !opts.json;
    let say = |s: String| {
        if text {
            println!("{s}");
        }
    };
    if files.is_empty() {
        say("no test files found (test files are named *_test.phv)".into());
        if opts.json {
            println!("{}", serde_json::json!({ "passed": 0, "failed": 0, "skipped": 0, "tests": [] }));
        }
        return ExitCode::SUCCESS;
    }
    let (green, red, yellow, reset) =
        if opts.color && text { ("\x1b[32m", "\x1b[31m", "\x1b[33m", "\x1b[0m") } else { ("", "", "", "") };
    let mut results: Vec<TestResult> = Vec::new();
    // Compile errors and files that could not run, as failures without a test.
    let mut file_errors = 0;
    let mut coverage: std::collections::BTreeMap<String, FileCoverage> = std::collections::BTreeMap::new();
    let start = Instant::now();
    let mut sites: std::collections::HashMap<PathBuf, Option<std::sync::Arc<dyn phv_vm::Site>>> =
        std::collections::HashMap::new();
    for file in files {
        let root = find_root(&file);
        let shown = file.display().to_string();
        let mut compiled = match compile_for_tests(&root, &file, opts.coverage) {
            Ok(c) => c,
            Err(e) => {
                say(shown.clone());
                if text {
                    print!("{}", e.render(opts.color));
                }
                file_errors += 1;
                results.push(TestResult {
                    file: shown,
                    name: "(compile)".into(),
                    status: "fail",
                    seconds: 0.0,
                    message: e.render(false),
                    output: String::new(),
                });
                continue;
            }
        };
        if !opts.coverage {
            compiled.enable_jit();
        }
        let tests: Vec<(u32, String, Option<String>)> = compiled
            .tests()
            .into_iter()
            .filter(|(_, name, _)| opts.filter.as_deref().is_none_or(|f| name.contains(f)))
            .collect();
        if opts.filter.is_some() && tests.is_empty() {
            continue;
        }
        say(shown.clone());
        if tests.is_empty() {
            say("  (no test_* or #[Test] functions)".into());
        }
        let config = load_config(&root).unwrap_or_else(|_| phv_driver::Config::default_for(&root));
        // A project with public/ gets its site, for `std.testing`.
        let mut host = (*config.host).clone();
        if root.join("public").is_dir() {
            match sites.entry(root.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => host.site = e.get().clone(),
                std::collections::hash_map::Entry::Vacant(e) => match phv_server::test_site(&root) {
                    Ok(site) => host.site = e.insert(Some(site)).clone(),
                    Err(err) => {
                        say(format!("  {yellow}warning{reset}: the site cannot be used in tests: {err}"));
                        e.insert(None);
                    }
                },
            }
        }
        let host = std::sync::Arc::new(host);
        let (before, after) = compiled.hooks();
        let mut ran: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        for (fid, name, skip) in tests {
            if let Some(reason) = skip {
                let shown_reason = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
                say(format!("  {yellow}skip{reset}  {name}{shown_reason}"));
                results.push(TestResult {
                    file: shown.clone(),
                    name,
                    status: "skip",
                    seconds: 0.0,
                    message: reason,
                    output: String::new(),
                });
                continue;
            }
            let limits = Limits { max_time: Some(Duration::from_secs(30)), ..Limits::default() };
            let run_opts = RunOptions { html: false, limits, http: None, host: Some(host.clone()) };
            let t = Instant::now();
            let cov = if opts.coverage { Some(&mut ran) } else { None };
            let outcome = run_test(&compiled, &before, fid, &after, run_opts, cov);
            let seconds = t.elapsed().as_secs_f64();
            match outcome.error {
                None => {
                    say(format!("  {green}ok{reset}    {name}"));
                    results.push(TestResult {
                        file: shown.clone(),
                        name,
                        status: "pass",
                        seconds,
                        message: String::new(),
                        output: outcome.output,
                    });
                }
                Some(err) => {
                    say(format!("  {red}FAIL{reset}  {name}"));
                    let message = err.render();
                    for line in message.lines() {
                        say(format!("        {line}"));
                    }
                    if !outcome.output.is_empty() {
                        say("        output:".into());
                        for line in outcome.output.lines() {
                            say(format!("          {line}"));
                        }
                    }
                    results.push(TestResult {
                        file: shown.clone(),
                        name,
                        status: "fail",
                        seconds,
                        message,
                        output: outcome.output,
                    });
                }
            }
        }
        if opts.coverage {
            // The project's own files: not the standard library, not test files.
            let prog = &compiled.program;
            for f in &prog.funcs {
                let Some(info) = prog.files.get(f.file as usize) else { continue };
                if info.name.starts_with('<') || info.name.ends_with("_test.phv") {
                    continue;
                }
                let entry = coverage.entry(info.name.clone()).or_default();
                entry.line_starts.clone_from(&info.line_starts);
                for ins in &f.code {
                    if let phv_vm::Instr::Cover(off) = ins {
                        entry.all.insert(*off);
                        if ran.contains(&(f.file, *off)) {
                            entry.ran.insert(*off);
                        }
                    }
                }
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let count = |s: &str| results.iter().filter(|r| r.status == s).count();
    let (passed, skipped) = (count("pass"), count("skip"));
    let failed = count("fail");
    coverage.retain(|_, c| !c.all.is_empty());
    if text {
        let colour = if failed == 0 { green } else { red };
        let skipped = if skipped > 0 { format!(", {skipped} skipped") } else { String::new() };
        let failed_tests = failed - file_errors;
        let errors = if file_errors > 0 { format!(", {file_errors} file(s) did not compile") } else { String::new() };
        println!("\n{colour}{passed} passed, {failed_tests} failed{skipped}{errors}{reset} ({secs:.2}s)");
        if opts.coverage && coverage.is_empty() {
            println!(
                "\ncoverage: the tests use no module of the project (pages run through TestClient are not measured)"
            );
        } else if opts.coverage {
            println!("\ncoverage (statements run by the tests):");
            let width = coverage.keys().map(|k| k.len()).max().unwrap_or(0);
            let (mut all, mut ran) = (0, 0);
            for (name, c) in &coverage {
                all += c.all.len();
                ran += c.ran.len();
                let missed = c.missed_lines();
                let missed = if missed.is_empty() { String::new() } else { format!("   not run: lines {missed}") };
                println!("  {name:width$}  {:5.1}%  ({}/{}){missed}", c.percent(), c.ran.len(), c.all.len());
            }
            let total = if all == 0 { 100.0 } else { ran as f64 * 100.0 / all as f64 };
            println!("  {:width$}  {total:5.1}%  ({ran}/{all})", "total");
        }
    } else {
        let tests: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "file": r.file, "name": r.name, "status": r.status, "seconds": r.seconds,
                    "message": r.message, "output": r.output,
                })
            })
            .collect();
        let mut doc = serde_json::json!({
            "passed": passed, "failed": failed, "skipped": skipped, "seconds": secs, "tests": tests,
        });
        if opts.coverage {
            doc["coverage"] = coverage
                .iter()
                .map(|(name, c)| {
                    let mut missed: Vec<usize> = c.all.difference(&c.ran).map(|o| c.line_of(*o)).collect();
                    missed.dedup();
                    serde_json::json!({
                        "file": name, "statements": c.all.len(), "run": c.ran.len(), "missed_lines": missed,
                    })
                })
                .collect();
        }
        println!("{doc}");
    }
    if let Some(path) = &opts.junit {
        if let Err(e) = std::fs::write(path, junit(&results, secs)) {
            eprintln!("error: cannot write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn fmt(paths: Vec<PathBuf>, check: bool, width: Option<usize>) -> ExitCode {
    let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths };
    let files = collect(&paths, &|p| {
        let n = name_of(p);
        n.ends_with(".phv") && !n.ends_with(".phv.html")
    });
    let mut changed = 0;
    let mut errors = 0;
    // `--width`, else `[fmt] width` of each file's project.
    let mut widths: std::collections::HashMap<PathBuf, Result<usize, String>> = std::collections::HashMap::new();
    for f in &files {
        let width = match width {
            Some(w) => Ok(w),
            None => widths.entry(find_root(f)).or_insert_with_key(|r| load_config(r).map(|c| c.fmt.width)).clone(),
        };
        let width = match width {
            Ok(w) => w,
            Err(e) => {
                eprintln!("error: {}: {e}", f.display());
                errors += 1;
                continue;
            }
        };
        let Ok(src) = std::fs::read_to_string(f) else {
            eprintln!("error: cannot read {}", f.display());
            errors += 1;
            continue;
        };
        match phv_syntax::format::format_source_width(&src, width) {
            Ok(out) if out != src => {
                changed += 1;
                if check {
                    println!("would reformat {}", f.display());
                } else if let Err(e) = std::fs::write(f, out) {
                    eprintln!("error: cannot write {}: {e}", f.display());
                    errors += 1;
                } else {
                    println!("formatted {}", f.display());
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {}: {e}", f.display());
                errors += 1;
            }
        }
    }
    let verb = if check { "need formatting" } else { "reformatted" };
    println!("{} file(s) checked, {changed} {verb}", files.len());
    if errors > 0 || (check && changed > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
