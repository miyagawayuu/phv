//! `phenix test` and `phenix fmt`.

use phenix_driver::{compile, find_root, load_config, run_func, RunOptions};
use phenix_vm::Limits;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const SKIP_DIRS: &[&str] = &["target", "vendor", ".git", ".jj", "node_modules"];

/// Files under `paths` accepted by `want` (directories are walked).
pub fn collect(paths: &[PathBuf], want: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    fn walk(dir: &Path, want: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    walk(&p, want, out);
                }
            } else if want(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if p.is_file() {
            out.push(p.clone());
        } else {
            walk(p, want, &mut out);
        }
    }
    out
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

pub fn test(paths: Vec<PathBuf>, color: bool) -> ExitCode {
    let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths };
    let files = collect(&paths, &|p| name_of(p).ends_with("_test.phx"));
    if files.is_empty() {
        println!("no test files found (test files are named *_test.phx)");
        return ExitCode::SUCCESS;
    }
    let (green, red, reset) = if color { ("\x1b[32m", "\x1b[31m", "\x1b[0m") } else { ("", "", "") };
    let (mut passed, mut failed) = (0, 0);
    let start = Instant::now();
    for file in files {
        let root = find_root(&file);
        println!("{}", file.display());
        let compiled = match compile(&root, &file) {
            Ok(c) => c,
            Err(e) => {
                print!("{}", e.render(color));
                failed += 1;
                continue;
            }
        };
        let config = load_config(&root).unwrap_or_else(|_| phenix_driver::Config::default_for(&root));
        let tests = compiled.entry_functions("test_");
        if tests.is_empty() {
            println!("  (no test_* functions)");
        }
        for (fid, name) in tests {
            let limits = Limits { max_time: Some(Duration::from_secs(30)), ..Limits::default() };
            let opts = RunOptions { html: false, limits, http: None, host: Some(config.host.clone()) };
            let outcome = run_func(&compiled, fid, opts);
            match outcome.error {
                None => {
                    passed += 1;
                    println!("  {green}ok{reset}    {name}");
                }
                Some(err) => {
                    failed += 1;
                    println!("  {red}FAIL{reset}  {name}");
                    for line in err.render().lines() {
                        println!("        {line}");
                    }
                    if !outcome.output.is_empty() {
                        println!("        output:");
                        for line in outcome.output.lines() {
                            println!("          {line}");
                        }
                    }
                }
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let colour = if failed == 0 { green } else { red };
    println!("\n{colour}{passed} passed, {failed} failed{reset} ({secs:.2}s)");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn fmt(paths: Vec<PathBuf>, check: bool) -> ExitCode {
    let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths };
    let files = collect(&paths, &|p| {
        let n = name_of(p);
        n.ends_with(".phx") && !n.ends_with(".phx.html")
    });
    let mut changed = 0;
    let mut errors = 0;
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            eprintln!("error: cannot read {}", f.display());
            errors += 1;
            continue;
        };
        match phenix_syntax::format::format_source(&src) {
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
