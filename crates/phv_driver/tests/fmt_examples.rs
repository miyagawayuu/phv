//! `phv fmt` on the example projects: every `examples/**/*.phv` is formatted
//! (at the default width and at a narrow one, which wraps most lines), and
//! each file that compiles as written must still compile from the formatted
//! sources, its imports formatted too.

use phv_driver::{compile, compile_with_overlay, find_root};
use phv_syntax::format::{format_source_width, DEFAULT_WIDTH};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn phv_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "vendor" {
                phv_files(&p, out);
            }
        } else if name.ends_with(".phv") {
            out.push(p);
        }
    }
}

#[test]
fn width_from_phv_toml() {
    let dir = std::env::temp_dir().join(format!("phv-fmt-width-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let width = |toml: &str| {
        std::fs::write(dir.join("phv.toml"), toml).unwrap();
        phv_driver::load_config(&dir).map(|c| c.fmt.width)
    };
    assert_eq!(width("[fmt]\nwidth = 80\n"), Ok(80));
    assert_eq!(width("[project]\nname = \"x\"\n"), Ok(DEFAULT_WIDTH));
    assert!(width("[fmt]\nwidht = 80\n").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn formatted_examples_still_compile() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").canonicalize().unwrap();
    let mut files = Vec::new();
    phv_files(&examples, &mut files);
    assert!(files.len() > 10, "examples not found in {}", examples.display());
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let failures = worker
        .spawn(move || {
            let mut failures = Vec::new();
            for width in [DEFAULT_WIDTH, 40] {
                let mut overlay = HashMap::new();
                for f in &files {
                    let src = std::fs::read_to_string(f).unwrap();
                    let formatted = format_source_width(&src, width).unwrap();
                    if format_source_width(&formatted, width).unwrap() != formatted {
                        failures.push(format!("{} (width {width}): not idempotent", f.display()));
                    }
                    overlay.insert(f.clone(), formatted);
                }
                for f in &files {
                    let root = find_root(f);
                    if compile(&root, f).is_err() {
                        continue; // not an entry point on its own
                    }
                    if let Err(e) = compile_with_overlay(&root, f, overlay.clone()) {
                        failures.push(format!("{} (width {width}):\n{}", f.display(), e.render(false)));
                    }
                }
            }
            failures
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
