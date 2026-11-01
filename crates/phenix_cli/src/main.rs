mod lsp;
mod migrate;
mod pkg;
mod tools;

use clap::{Parser, Subcommand};

#[global_allocator]
static ALLOC: phenix_vm::alloc::CountingAlloc = phenix_vm::alloc::CountingAlloc;

use phenix_driver::{compile, compile_cached, find_root, load_config, run, RunOptions};
use phenix_vm::Limits;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "phenix", version, about = "The Phenix programming language")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Type-check and run a script.
    Run {
        file: PathBuf,
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
    /// Run the `test_*` functions of every `*_test.phx` file.
    Test {
        /// Files or directories (default: the current directory).
        paths: Vec<PathBuf>,
    },
    /// Format `.phx` files (indentation and whitespace).
    Fmt {
        paths: Vec<PathBuf>,
        /// Only report files that would change.
        #[arg(long)]
        check: bool,
    },
    /// Start the language server (for editors) on stdin/stdout.
    Lsp,
    /// Fetch the [dependencies] listed in phenix.toml.
    Install {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Convert a PHP file to Phenix, marking what needs manual work.
    Migrate {
        file: PathBuf,
        /// Output file (default: the input with a .phx extension).
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
    },
}

fn main() -> ExitCode {
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
        Command::Run { file, html, time_limit } => {
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
            let outcome = run(&compiled, RunOptions { html, limits, http: None, host: Some(config.host.clone()) });
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
        Command::Test { paths } => tools::test(paths, color),
        Command::Fmt { paths, check } => tools::fmt(paths, check),
        Command::Lsp => {
            lsp::run();
            ExitCode::SUCCESS
        }
        Command::Install { dir } => pkg::install(&dir),
        Command::Migrate { file, output, force } => {
            let src = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: cannot read {}: {e}", file.display());
                    return ExitCode::FAILURE;
                }
            };
            let m = migrate::convert(&src);
            let out_path = output.unwrap_or_else(|| file.with_extension("phx"));
            if out_path == file {
                eprintln!("error: refusing to overwrite the input file");
                return ExitCode::FAILURE;
            }
            if out_path.exists() && !force {
                eprintln!("error: {} already exists (use --force to overwrite)", out_path.display());
                return ExitCode::FAILURE;
            }
            if let Err(e) = std::fs::write(&out_path, &m.text) {
                eprintln!("error: cannot write {}: {e}", out_path.display());
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
            let is_template = file.to_string_lossy().ends_with(".phx.html");
            let mut sources = phenix_syntax::SourceMap::default();
            let id = sources.add(file.display().to_string(), Some(file.clone()), text);
            let (module, diags) = phenix_syntax::parse_module(&sources.get(id).text, id, is_template);
            println!("{module:#?}");
            if diags.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprint!("{}", phenix_syntax::diag::render(&sources, &diags, color));
                ExitCode::FAILURE
            }
        }
        Command::Serve { dir, host, port, prod, dev, time_limit } => {
            let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            let opts = phenix_server::ServeOptions {
                root: dir,
                host,
                port,
                dev: dev || (!prod && loopback),
                time_limit: Duration::from_secs_f64(time_limit),
            };
            match phenix_server::serve(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
