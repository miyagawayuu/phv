//! `phv jobs`: run a site's background jobs without serving its pages, list
//! the jobs of its store, and put a failed job back in the queue.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn store(project: &Path) -> Result<phv_std::jobs::Store, String> {
    let root = project.canonicalize().map_err(|e| format!("cannot open {}: {e}", project.display()))?;
    let config = phv_driver::load_config(&root)?;
    if config.jobs.is_none() {
        return Err("phv.toml has no [jobs] section".into());
    }
    let store = phv_std::jobs::Store::of(&config.host)
        .ok_or_else(|| "[jobs] store: the database is not in [databases]".to_string())?;
    if matches!(store, phv_std::jobs::Store::Memory(_)) {
        return Err(
            "[jobs] store = \"memory\" keeps the jobs inside the server process; give a database (\"db:<name>\") to see them here"
                .into(),
        );
    }
    Ok(store)
}

/// `phv jobs run`: runs the jobs until Ctrl-C or SIGTERM.
pub fn run(project: &Path, time_limit: f64) -> ExitCode {
    let opts = phv_server::ServeOptions {
        root: project.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs_f64(time_limit),
        watch: false,
    };
    match phv_server::run_jobs(opts, None, None) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `phv jobs list`: the waiting, running and failed jobs (or only the failed).
pub fn list(project: &Path, failed: bool) -> ExitCode {
    let jobs = match store(project).and_then(|s| s.list(failed)) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if jobs.is_empty() {
        println!("no jobs");
        return ExitCode::SUCCESS;
    }
    for j in &jobs {
        let at = chrono::DateTime::from_timestamp_millis(j.at)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_default();
        // Rows come from the store, which others may write: every field is
        // escaped (found by the 1.1 audit: ids and names reached the terminal).
        use phv_syntax::diag::log_escape;
        let short = |s: &str| log_escape(&s.chars().take(200).collect::<String>());
        println!("{}  {:<8} {:<24} runs {:<3} {at}", short(&j.id), short(j.state), short(&j.name), j.attempts);
        if !j.error.is_empty() {
            // The first line of the error.
            println!("    {}", short(j.error.lines().next().unwrap_or("")));
        }
    }
    ExitCode::SUCCESS
}

/// `phv jobs retry`: puts a failed job back in the queue.
pub fn retry(project: &Path, id: &str) -> ExitCode {
    match store(project).and_then(|s| s.requeue(id)) {
        Ok(true) => {
            println!("job {id} is waiting again");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("error: there is no failed job {id}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
