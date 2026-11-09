//! `phv jobs`: run a site's background jobs without serving its pages, list
//! the jobs of its store, put a failed job back in the queue, and adopt the
//! jobs the site left in the shared store before it had a namespace.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The shared store of the project's jobs, with the project's root and its
/// configuration.
fn opened(project: &Path) -> Result<(phv_std::jobs::Store, PathBuf, phv_driver::config::Config), String> {
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
    Ok((store, root, config))
}

fn store(project: &Path) -> Result<phv_std::jobs::Store, String> {
    opened(project).map(|(store, _, _)| store)
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
    let store = match store(project) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let jobs = match store.list(failed) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if jobs.is_empty() {
        println!("no jobs");
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
    // A site that got a namespace (1.1) may have left jobs in the store every
    // site shared before: they are not run, and nothing else shows them. A
    // store that cannot be counted (or has no such table) adds nothing.
    match store.left_in_shared() {
        Ok(n) if n > 0 => {
            let (jobs, are) = if n == 1 { ("job", "is") } else { ("jobs", "are") };
            println!(
                "note: {n} {jobs} added before this site had a namespace {are} still in the shared store and \
                 not run; `phv jobs adopt` moves those of this site's jobs"
            );
        }
        _ => {}
    }
    ExitCode::SUCCESS
}

/// `phv jobs adopt`: moves the jobs the site left in the shared store before
/// it had a namespace into its own store; a dry run unless `yes`. With
/// `only` (`--name`), only the jobs of those of the site's job scripts.
pub fn adopt(project: &Path, yes: bool, only: &[String]) -> ExitCode {
    use phv_std::jobs::Outcome;
    use phv_syntax::diag::log_escape;
    let (store, root, config) = match opened(project) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The shared store records no site: only jobs named as this site's job
    // scripts are taken.
    let dir = config.jobs.as_ref().map_or("jobs", |j| j.dir.as_str());
    let mut names = phv_driver::config::scan_jobs(&root, dir).unwrap_or_default();
    if names.is_empty() {
        println!("nothing to adopt: {dir}/ has no job scripts, and only jobs of this site's scripts are adopted");
        return ExitCode::SUCCESS;
    }
    if !only.is_empty() {
        // A name that is not a job script of the site would adopt nothing:
        // said, so a typo is not taken for an empty store.
        if let Some(n) = only.iter().find(|n| !names.contains(n)) {
            eprintln!(
                "error: --name {}: {dir}/ has no job script {}.phv, and only jobs of this site's scripts are adopted",
                log_escape(n),
                log_escape(n)
            );
            return ExitCode::FAILURE;
        }
        names.retain(|n| only.contains(n));
    }
    let jobs = match store.adopt(&names, yes) {
        Ok(Some(jobs)) => jobs,
        Ok(None) => {
            println!("nothing to adopt: the site has no namespace, so the shared store is its own");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if jobs.is_empty() {
        println!("nothing to adopt: the shared store holds no job of this site's scripts");
        return ExitCode::SUCCESS;
    }
    // Rows come from the store, which others may write: every field is escaped.
    let short = |s: &str| log_escape(&s.chars().take(200).collect::<String>());
    let moved: Vec<_> = jobs.iter().filter(|j| j.outcome == Outcome::Moved).collect();
    let count = |state: &str| moved.iter().filter(|j| j.state == state).count();
    let plural = |n: usize| if n == 1 { "job" } else { "jobs" };
    if !moved.is_empty() {
        let verb = if yes { "moved" } else { "would move" };
        println!(
            "{verb} {} {} ({} waiting, {} failed):",
            moved.len(),
            plural(moved.len()),
            count("waiting"),
            count("failed")
        );
        // By name first: the check that another site on the same database
        // did not leave jobs of the same names is made on these counts, not
        // on the lines of every job (found by the 18th assessment).
        let mut by_name: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
        for j in &moved {
            let e = by_name.entry(j.name.as_str()).or_default();
            if j.state == "waiting" {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        for (name, (waiting, failed)) in &by_name {
            println!("  {}: {waiting} waiting, {failed} failed", short(name));
        }
        for j in &moved {
            println!("  {}  {:<8} {}", short(&j.id), short(j.state), short(&j.name));
        }
    }
    let mut no_room = 0;
    for j in jobs.iter().filter(|j| j.outcome != Outcome::Moved) {
        let why = match j.outcome {
            Outcome::Moved => continue,
            Outcome::Running => "running: a job is moved between its runs",
            Outcome::Locked => "held by another connection; run the command again",
            Outcome::KeyTaken => "this site's store has a job with its key",
            Outcome::NoRoom => {
                no_room += 1;
                "this site's store is full"
            }
        };
        println!("skipped  {}  {:<8} {}: {why}", short(&j.id), short(j.state), short(&j.name));
    }
    if no_room > 0 {
        println!(
            "{no_room} waiting {} {} in the shared store: {}",
            plural(no_room),
            if no_room == 1 { "stays" } else { "stay" },
            phv_std::jobs::DB_CAP.full()
        );
    }
    if yes {
        // What is left of this site's jobs, by name: those skipped above, and
        // any the store got meanwhile (another site still on 1.0 adds them)
        // or that the move did not reach, so none is left unsaid (found by
        // the 19th assessment: two jobs were left with only the count below).
        if let Ok(Some(left)) = store.adopt(&names, false) {
            let mut by_name: std::collections::BTreeMap<&str, usize> = Default::default();
            for j in &left {
                *by_name.entry(j.name.as_str()).or_default() += 1;
            }
            if !by_name.is_empty() {
                println!("left in the shared store, of this site's scripts:");
                for (name, n) in &by_name {
                    println!("  {}: {n} {}", short(name), plural(*n));
                }
            }
        }
        if let Ok(n) = store.left_in_shared() {
            if n > 0 {
                println!(
                    "note: {n} {} still in the shared store (skipped above, or not jobs of this site's scripts)",
                    if n == 1 { "job is" } else { "jobs are" }
                );
            }
        }
    } else if !moved.is_empty() {
        println!("a dry run: nothing was moved; run `phv jobs adopt --yes` to move them");
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
