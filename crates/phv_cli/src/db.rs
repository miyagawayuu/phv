//! `phv db migrate` and `phv db status`.

use phv_std::migrate::{self, Conn, State, Target};
use std::path::Path;
use std::process::ExitCode;

/// Applies the pending migrations (`apply`), or lists every migration and its state.
pub fn run(project: &Path, db: Option<&str>, sqlite: Option<&str>, dir: &str, apply: bool) -> ExitCode {
    match go(project, db, sqlite, dir, apply) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn go(project: &Path, db: Option<&str>, sqlite: Option<&str>, dir: &str, apply: bool) -> Result<(), String> {
    let config = phv_driver::load_config(project)?;
    let target = match (db, sqlite) {
        (Some(name), None) => match config.host.databases.get(name) {
            Some(spec) => Target::Server(spec),
            None => return Err(format!("phv.toml has no [databases.{name}]")),
        },
        (None, Some(file)) => {
            // Inside the project, like every path PHV opens: only names (not
            // `..`, `\\x`, `C:x` or an absolute path; found by the 1.0 audit).
            use std::path::Component;
            let p = Path::new(file);
            let names = p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
            let rel: std::path::PathBuf = p.components().filter(|c| matches!(c, Component::Normal(_))).collect();
            if !names || rel.as_os_str().is_empty() {
                return Err(format!("--sqlite {file}: give a path inside the project"));
            }
            Target::Sqlite(project.to_path_buf(), rel)
        }
        _ => return Err("name the database: --db <name> (from [databases]) or --sqlite <file>".into()),
    };
    let files = migrate::files(&project.join(dir))?;
    let mut conn = Conn::open(&target)?;
    let (states, gone) = migrate::plan(&mut conn, &files)?;
    for name in &gone {
        eprintln!("warning: {name} was applied, but its file is gone");
    }
    if let Some((name, _)) = states.iter().find(|(_, s)| *s == State::Changed) {
        return Err(format!(
            "{name} has changed since it was applied; undo the change, and put new changes in a new migration"
        ));
    }
    if !apply {
        for (name, state) in &states {
            let word = if *state == State::Applied { "applied" } else { "pending" };
            println!("{word:8} {name}");
        }
        return Ok(());
    }
    let pending: Vec<&migrate::Migration> =
        files.iter().filter(|f| states.iter().any(|(n, s)| *n == f.name && *s == State::Pending)).collect();
    if pending.is_empty() {
        println!("Nothing to apply: the database is up to date");
        return Ok(());
    }
    for f in pending {
        let sql = std::fs::read_to_string(&f.path).map_err(|e| format!("cannot read {}: {e}", f.path.display()))?;
        conn.apply(&f.name, &sql).map_err(|e| format!("{} failed: {e}", f.name))?;
        println!("applied  {}", f.name);
    }
    Ok(())
}
