mod build;
mod db;
mod lsp;
mod migrate;
mod new;
mod pkg;
mod tools;

use clap::{Parser, Subcommand};

#[global_allocator]
static ALLOC: phv_vm::alloc::CountingAlloc = phv_vm::alloc::CountingAlloc;

use phv_driver::{compile, compile_cached, find_root, load_config, run, RunOptions};
use phv_vm::Limits;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "phv", version, about = "The PHV programming language (https://phv-lang.org)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum DbAction {
    /// Apply the migrations that have not run yet, in order.
    Migrate(DbArgs),
    /// List the migrations and whether each has been applied.
    Status(DbArgs),
}

#[derive(clap::Args)]
struct DbArgs {
    /// A database of [databases] in phv.toml (PostgreSQL or MySQL).
    #[arg(long)]
    db: Option<String>,
    /// A SQLite file inside the project.
    #[arg(long)]
    sqlite: Option<String>,
    /// The directory of the migration files.
    #[arg(long, default_value = "migrations")]
    dir: String,
    /// The project's directory.
    #[arg(default_value = ".")]
    project: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Type-check and run a script.
    Run {
        file: PathBuf,
        /// Arguments for the script (read with `argv()`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Escape echoed strings as HTML, like a web page.
        #[arg(long)]
        html: bool,
        /// Stop the script after this many seconds.
        #[arg(long)]
        time_limit: Option<f64>,
    },
    /// Type-check a script and everything it imports, without running it.
    Check { file: PathBuf },
    /// Print the syntax tree of a file.
    Parse { file: PathBuf },
    /// Run the `test_*` functions of every `*_test.phv` file.
    Test {
        /// Files or directories (default: the current directory).
        paths: Vec<PathBuf>,
        /// Only the tests whose name contains this text.
        #[arg(long)]
        filter: Option<String>,
        /// Print one JSON document with every result instead of the report.
        #[arg(long)]
        json: bool,
        /// Also write the results as JUnit XML to this file (for CI).
        #[arg(long)]
        junit: Option<PathBuf>,
        /// Report the statements of the project's files that the tests ran.
        #[arg(long)]
        coverage: bool,
    },
    /// Format `.phv` files (indentation, whitespace and long lines).
    Fmt {
        paths: Vec<PathBuf>,
        /// Only report files that would change.
        #[arg(long)]
        check: bool,
        /// Wrap lines longer than this (default: `[fmt] width` in phv.toml, or 100).
        #[arg(long)]
        width: Option<usize>,
    },
    /// Start the language server (for editors) on stdin/stdout.
    Lsp,
    /// Fetch the [dependencies] listed in phv.toml.
    Install {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Install exactly what phv.lock records; fail if it is out of date.
        #[arg(long)]
        locked: bool,
    },
    /// Compile a site (a directory with public/) or a script into one executable
    /// that runs it without the sources.
    Build {
        /// The site's directory, or a script.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// The executable to write.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Apply the SQL files of migrations/ to a database, or list them.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Convert a PHP file to PHV, marking what needs manual work.
    Migrate {
        file: PathBuf,
        /// Output file (default: the input with a .phv extension).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite the output file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Print the bytecode of a script's functions.
    Disasm {
        file: PathBuf,
        /// Only functions whose name contains this text.
        #[arg(long)]
        only: Option<String>,
    },
    /// Serve a site: `public/` is routed by file name.
    Serve {
        /// Site directory (containing `public/`).
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Hide error details from visitors (the default unless the host is loopback).
        #[arg(long, conflicts_with = "dev")]
        prod: bool,
        /// Show error details in the browser (the default on 127.0.0.1 / localhost).
        #[arg(long)]
        dev: bool,
        /// Per-request time limit in seconds.
        #[arg(long, default_value_t = 30.0)]
        time_limit: f64,
        /// Reload pages in the browser when a file of the site changes (development only).
        #[arg(long, conflicts_with = "prod")]
        watch: bool,
    },
    /// Create a new project: a script, or a site with `--site`.
    New {
        /// The directory to create; its name is the project's name.
        dir: PathBuf,
        /// A web site (public/, a layout, a page test) instead of a script.
        #[arg(long)]
        site: bool,
    },
}

/// Writes the result of `phv migrate`. The output is never the input file
/// (also not under another spelling, e.g. `X.PHV` on Windows), never a link
/// (a planted `x.phv` symlink would send the text anywhere), and an existing
/// file is replaced only with `--force`.
fn write_migrated(input: &Path, out: &Path, text: &str, force: bool) -> Result<(), String> {
    use std::io::Write as _;
    let shown = out.display();
    if let (Ok(a), Ok(b)) = (input.canonicalize(), out.canonicalize()) {
        if a == b {
            return Err("refusing to overwrite the input file".into());
        }
    }
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = out.file_name().ok_or_else(|| format!("{shown} is not a file name"))?;
    let (dir, name) =
        phv_std::safefs::open_parent(&parent, Path::new(name)).map_err(|e| format!("cannot write {shown}: {e:?}"))?;
    match dir.exists(&name) {
        Ok(true) if !force => return Err(format!("{shown} already exists (use --force to overwrite)")),
        Ok(_) => {}
        Err(phv_std::safefs::FsError::Link) => return Err(format!("{shown} is a link; refusing to write through it")),
        Err(e) => return Err(format!("cannot write {shown}: {e:?}")),
    }
    let mut f = dir.open(&name, true).map_err(|e| match e {
        phv_std::safefs::FsError::Link | phv_std::safefs::FsError::HardLink => {
            format!("{shown} is a link; refusing to write through it")
        }
        e => format!("cannot write {shown}: {e:?}"),
    })?;
    f.write_all(text.as_bytes()).map_err(|e| format!("cannot write {shown}: {e}"))
}

fn main() -> ExitCode {
    // An executable made by `phv build` runs what it carries instead.
    if let Some(bundle) = phv_driver::bundle::read_own() {
        let bundle = match bundle {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || build::run_bundle(bundle))
            .expect("failed to start main thread");
        return handle.join().unwrap_or(ExitCode::FAILURE);
    }
    let cli = Cli::parse();
    // Scripts run on a thread with a large stack so deep recursion through
    // builtins (e.g. nested `map` callbacks) has room.
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || exec(cli.command))
        .expect("failed to start main thread");
    handle.join().unwrap_or(ExitCode::FAILURE)
}

fn exec(cmd: Command) -> ExitCode {
    let color = std::io::stderr().is_terminal();
    match cmd {
        Command::Run { file, html, time_limit, args } => {
            let root = find_root(&file);
            let mut compiled = match compile_cached(&root, &file) {
                Ok(c) => c,
                Err(e) => {
                    eprint!("{}", e.render(color));
                    return ExitCode::FAILURE;
                }
            };
            eprint!("{}", compiled.warnings);
            compiled.enable_jit();
            let config = match load_config(&root) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let limits = Limits {
                max_time: time_limit.map(Duration::from_secs_f64),
                max_memory: Some(config.memory_limit_mb as usize * 1024 * 1024),
                ..Limits::default()
            };
            let mut host = (*config.host).clone();
            host.cli = true;
            host.args = args;
            let outcome =
                run(&compiled, RunOptions { html, limits, http: None, host: Some(std::sync::Arc::new(host)) });
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
        Command::Check { file } => {
            let root = find_root(&file);
            match compile(&root, &file) {
                Ok(c) => {
                    eprint!("{}", c.warnings);
                    println!("ok: no errors in {}", file.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprint!("{}", e.render(color));
                    ExitCode::FAILURE
                }
            }
        }
        Command::Test { paths, filter, json, junit, coverage } => {
            let color = std::io::stdout().is_terminal();
            tools::test(paths, tools::TestOptions { filter, json, junit, coverage, color })
        }
        Command::Fmt { paths, check, width } => tools::fmt(paths, check, width),
        Command::Lsp => {
            lsp::run();
            ExitCode::SUCCESS
        }
        Command::Install { dir, locked } => pkg::install(&dir, locked),
        Command::Build { path, output } => build::build(&path, &output),
        Command::Db { action } => {
            let (a, apply) = match &action {
                DbAction::Migrate(a) => (a, true),
                DbAction::Status(a) => (a, false),
            };
            db::run(&a.project, a.db.as_deref(), a.sqlite.as_deref(), &a.dir, apply)
        }
        Command::Migrate { file, output, force } => {
            let src = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: cannot read {}: {e}", file.display());
                    return ExitCode::FAILURE;
                }
            };
            let m = migrate::convert(&src);
            let out_path = output.unwrap_or_else(|| file.with_extension("phv"));
            if let Err(e) = write_migrated(&file, &out_path, &m.text, force) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
            println!("wrote {}", out_path.display());
            for (what, n) in &m.changes {
                println!("  {n:>4} x {what}");
            }
            if m.todos > 0 {
                println!("  {:>4} x TODO_migrate(...) to resolve by hand (they do not compile on purpose)", m.todos);
            }
            if file.components().any(|c| c.as_os_str() == "public") {
                println!("note: delete the original .php file; it must not stay in public/");
            }
            let root = find_root(&out_path);
            match compile(&root, &out_path) {
                Ok(_) => println!("the result type-checks"),
                Err(e) => {
                    let n = e.diags.iter().filter(|d| d.is_error()).count() + e.messages.len();
                    println!("\n{n} issue(s) left for manual work (types, removed PHP features):\n");
                    print!("{}", e.render(color));
                }
            }
            ExitCode::SUCCESS
        }
        Command::Disasm { file, only } => {
            let root = find_root(&file);
            let compiled = match compile(&root, &file) {
                Ok(c) => c,
                Err(e) => {
                    eprint!("{}", e.render(color));
                    return ExitCode::FAILURE;
                }
            };
            for f in &compiled.program.funcs {
                if only.as_ref().is_some_and(|o| !f.name.contains(o.as_str())) {
                    continue;
                }
                println!("fn {} (params {}, registers {})", f.name, f.num_params, f.num_regs);
                for (i, ins) in f.code.iter().enumerate() {
                    println!("  {i:4}  {ins:?}");
                }
            }
            ExitCode::SUCCESS
        }
        Command::Parse { file } => {
            let text = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: cannot read {}: {e}", file.display());
                    return ExitCode::FAILURE;
                }
            };
            let is_template = file.to_string_lossy().ends_with(".phv.html");
            let mut sources = phv_syntax::SourceMap::default();
            let id = sources.add(file.display().to_string(), Some(file.clone()), text);
            let (module, diags) = phv_syntax::parse_module(&sources.get(id).text, id, is_template);
            println!("{module:#?}");
            if diags.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprint!("{}", phv_syntax::diag::render(&sources, &diags, color));
                ExitCode::FAILURE
            }
        }
        Command::New { dir, site } => match new::create(&dir, site) {
            Ok(files) => {
                for f in &files {
                    println!("  created {}", f.display());
                }
                let shown = dir.display();
                if site {
                    println!("\nNext: cd {shown} && phv serve --watch   (and phv test)");
                } else {
                    println!("\nNext: cd {shown} && phv run main.phv Ann   (and phv test)");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Serve { dir, host, port, prod, dev, time_limit, watch } => {
            let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if !dev && !prod && loopback {
                eprintln!(
                    "note: development mode (error details are shown in the browser); behind a reverse proxy, run with --prod"
                );
            }
            let dev = dev || (!prod && loopback);
            if watch && !dev {
                eprintln!("error: --watch is for development; it cannot be used with --prod or on a public address without --dev");
                return ExitCode::FAILURE;
            }
            let opts = phv_server::ServeOptions {
                root: dir,
                host,
                port,
                dev,
                time_limit: Duration::from_secs_f64(time_limit),
                watch,
            };
            match phv_server::serve(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
