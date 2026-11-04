//! `phv new <name> [--site]`: a new project in a new directory, with a test,
//! ready for `phv run`, `phv serve` and `phv test`.

use std::path::{Path, PathBuf};

const GITIGNORE: &str = ".phv-cache/\ndata/\nvendor/\n";

fn script_files(name: &str) -> Vec<(&'static str, String)> {
    vec![
        ("phv.toml", format!("[project]\nname = \"{name}\"\n")),
        (
            "main.phv",
            "<?phv\nimport lib.greet.greet;\n\n// phv run main.phv Ann\necho greet(argv()->first() ?? \"world\"), \"\\n\";\n"
                .into(),
        ),
        (
            "lib/greet.phv",
            "export function greet(string $name): string {\n    return \"Hello, {$name}!\";\n}\n".into(),
        ),
        (
            "tests/greet_test.phv",
            "<?phv\nimport lib.greet.greet;\n\nfunction test_greet(): void {\n    assert_eq(\"Hello, Ann!\", greet(\"Ann\"));\n}\n"
                .into(),
        ),
        (".gitignore", GITIGNORE.into()),
        ("README.md", format!("# {name}\n\n```bash\nphv run main.phv Ann\nphv test\n```\n")),
    ]
}

fn site_files(name: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "phv.toml",
            format!(
                "[project]\nname = \"{name}\"\n\n[security]\ncsrf = true\nsecure_cookies = false  # true once the site is served over HTTPS\n\n[permissions]\n# Scripts can only touch the files and variables listed here.\nwrite = [\"data/\"]\n"
            ),
        ),
        (
            "public/index.phv",
            "<?phv\nimport std.http.request;\n\nlayout(\"views/layout.phv.html\", title: \"Home\");\n$name = request()->query(\"name\") ?? \"world\";\n?>\n<h1>Hello, <?= $name ?>!</h1>\n<p>Edit <code>public/index.phv</code>; <code>phv serve --watch</code> reloads the page when you save.</p>\n"
                .into(),
        ),
        (
            "views/layout.phv.html",
            format!(
                "<?phv params(string $title, Html $body) ?>\n<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  <title><?= $title ?> · {name}</title>\n  <link rel=\"stylesheet\" href=\"/style.css\">\n</head>\n<body>\n<main>\n<?= $body ?>\n</main>\n</body>\n</html>\n"
            ),
        ),
        (
            "public/style.css",
            "body { font: 16px/1.5 system-ui, sans-serif; margin: 0; }\nmain { max-width: 40rem; margin: 3rem auto; padding: 0 1rem; }\n".into(),
        ),
        (
            "tests/home_test.phv",
            "<?phv\nimport std.testing.TestClient;\n\nfunction test_home_greets(): void {\n    $c = new TestClient();\n    $r = $c->get(\"/?name=Ann\");\n    assert_eq(200, $r->status);\n    assert($r->body->contains(\"Hello, Ann!\"), \"the page greets by name\");\n}\n"
                .into(),
        ),
        (".gitignore", GITIGNORE.into()),
        ("README.md", format!("# {name}\n\n```bash\nphv serve --watch   # http://127.0.0.1:8080/\nphv test\n```\n")),
    ]
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.starts_with('-')
}

/// Creates the project in `dir` (which must not exist, or be empty); returns the files written.
pub fn create(dir: &Path, site: bool) -> Result<Vec<PathBuf>, String> {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!("\"{name}\" is not a valid project name (use letters, digits, `-` and `_`)"));
    }
    if let Ok(meta) = std::fs::symlink_metadata(dir) {
        let empty = meta.is_dir() && std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !empty || meta.file_type().is_symlink() {
            return Err(format!("{} already exists and is not an empty directory", dir.display()));
        }
    }
    let files = if site { site_files(&name) } else { script_files(&name) };
    let mut written = Vec::new();
    for (rel, text) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        // `create_new`: never through a link or over a file that appeared meanwhile.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        std::io::Write::write_all(&mut f, text.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        written.push(path);
    }
    if site {
        std::fs::create_dir_all(dir.join("data")).map_err(|e| format!("cannot create data/: {e}"))?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_projects_check_and_pass_their_tests() {
        let base = std::env::temp_dir().join(format!("phv-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for site in [false, true] {
            let dir = base.join(if site { "my-site" } else { "my_tool" });
            create(&dir, site).unwrap();
            assert!(create(&dir, site).is_err(), "an existing project is not overwritten");
            let entry = dir.join(if site { "public/index.phv" } else { "main.phv" });
            let c = phv_driver::compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
            assert!(c.warnings.is_empty(), "{}", c.warnings);
            let test = dir.join(if site { "tests/home_test.phv" } else { "tests/greet_test.phv" });
            phv_driver::compile(&dir, &test).unwrap_or_else(|e| panic!("{}", e.render(false)));
        }
        assert!(create(&base.join("bad name"), false).is_err());
        assert!(create(&base.join(".."), false).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
