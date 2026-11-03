//! `phv build`, and running what it built: an executable whose bundle
//! (`phv_driver::bundle`) is a site to serve or a script to run.

use clap::Parser;
use phv_driver::bundle::{self, Bundle};
use phv_driver::{find_root, run, RunOptions};
use phv_vm::Limits;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

pub fn build(path: &Path, output: &Path) -> ExitCode {
    let built = if path.is_dir() {
        if !path.join("public").is_dir() {
            eprintln!("error: {} has no `public/` directory (give a site's directory, or a script)", path.display());
            return ExitCode::FAILURE;
        }
        if let Err(e) = phv_server::check_routes(&path.join("public")) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        bundle::build_site(path)
    } else {
        bundle::build_script(&find_root(path), path)
    };
    let mut b = match built {
        Ok(b) => b,
        Err(e) => {
            eprint!("{e}");
            if !e.ends_with('\n') {
                eprintln!();
            }
            return ExitCode::FAILURE;
        }
    };
    // Only the files the server would serve from public/ (not, say, a stray .toml or .env.bak).
    b.statics.retain(|(rel, _)| phv_server::serves_static(Path::new(rel)));
    if let Err(e) = bundle::write(&b, output) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    if b.site {
        println!("built {} ({} pages, {} files)", output.display(), b.pages.len(), b.statics.len());
    } else {
        println!("built {}", output.display());
    }
    ExitCode::SUCCESS
}

/// Options of an executable built from a site.
#[derive(Parser)]
#[command(about = "A PHV site built with `phv build`")]
struct SiteArgs {
    /// Where the directories the site reads and writes (and its sessions) are.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Show error details in the browser.
    #[arg(long, conflicts_with = "prod")]
    dev: bool,
    #[arg(long)]
    prod: bool,
    /// Maximum execution time per request, in seconds.
    #[arg(long, default_value_t = 30.0)]
    time_limit: f64,
}

/// Options of an executable built from a script.
#[derive(Parser)]
#[command(about = "A PHV script built with `phv build`")]
struct ScriptArgs {
    /// Where the files the script reads and writes are.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Stop the script after this many seconds.
    #[arg(long)]
    time_limit: Option<f64>,
}

/// Runs the bundle of this executable.
pub fn run_bundle(b: Bundle) -> ExitCode {
    let fail = |e: String| {
        eprintln!("error: {e}");
        ExitCode::FAILURE
    };
    if b.site {
        let args = SiteArgs::parse();
        let mut pages = HashMap::new();
        for p in b.pages {
            let path = p.path.clone();
            match p.into_compiled() {
                Ok(c) => pages.insert(path, Arc::new(c)),
                Err(e) => return fail(e),
            };
        }
        let statics = b.statics.into_iter().map(|(k, v)| (k, bytes::Bytes::from(v))).collect();
        let loopback =
            args.host == "localhost" || args.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
        let opts = phv_server::ServeOptions {
            root: args.root,
            host: args.host,
            port: args.port,
            dev: args.dev || (!args.prod && loopback),
            time_limit: Duration::from_secs_f64(args.time_limit),
        };
        let embedded = phv_server::Embedded { pages, statics, config: b.config };
        return match phv_server::serve_embedded(opts, embedded) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        };
    }
    let args = ScriptArgs::parse();
    let Some(page) = b.pages.into_iter().next() else { return fail("the bundle has no script".into()) };
    let compiled = match page.into_compiled() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let root = match args.root.canonicalize() {
        Ok(r) => r,
        Err(e) => return fail(format!("cannot open {}: {e}", args.root.display())),
    };
    let config = match phv_driver::config::parse_config(&root, b.config.as_deref().unwrap_or("")) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let limits = Limits {
        max_time: args.time_limit.map(Duration::from_secs_f64),
        max_memory: Some(config.memory_limit_mb as usize * 1024 * 1024),
        ..Limits::default()
    };
    let outcome = run(&compiled, RunOptions { html: false, limits, http: None, host: Some(config.host.clone()) });
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(outcome.output.as_bytes());
    let _ = stdout.flush();
    match outcome.error {
        Some(err) => {
            eprint!("{}", err.render());
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}
