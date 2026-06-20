//! `lean-checker`
//!
//! Embeds the Lean 4 runtime via FFI, initializes the search path from the
//! detected sysroot (as `leanprover/lean4export`'s `Main.lean` does), and
//! imports a set of modules. It can either dump the imports/constants, or walk
//! the environment's `lean_object`s and re-check every declaration with the
//! embedded `sokonanoda` kernel.

mod extract;
mod ffi;

use palc::Parser;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::ffi::Runtime;

/// 1 GiB worker stack: the kernel and the expression walkers recurse deeply.
const STACK_SIZE: usize = 1 << 30;

/// Embed Lean, import modules, and dump or check their declarations.
#[derive(Parser, Debug)]
#[command(name = "lean-checker")]
struct Cli {
    /// Modules to import, e.g. `Init`, `Init.Data.List`, `MyProject.Foo`.
    modules: Vec<String>,

    /// Extra search-path entries (like `LEAN_PATH`), e.g. a local project's
    /// `.lake/build/lib`. May be given multiple times.
    #[arg(long = "lean-path", value_name = "DIR")]
    lean_path: Vec<String>,

    /// Type-check the imported environment with the embedded kernel instead of
    /// dumping it.
    #[arg(long)]
    check: bool,

    /// Restrict checking to these constants and their transitive dependencies
    /// (implies `--check`). May be given multiple times.
    #[arg(long = "const", value_name = "NAME")]
    constants: Vec<String>,

    /// Number of kernel checking threads (default 1).
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// Only dump imports; skip the (potentially large) constant listing.
    #[arg(long)]
    imports_only: bool,

    /// Print at most this many constants when dumping.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
}

fn main() {
    // Logging via `tracing`. Level is controlled by `RUST_LOG` (default `info`).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Run everything on one large-stack thread: the Lean runtime must be driven
    // from a single thread, and both the kernel and the FFI walkers recurse far
    // past the default 8 MiB stack.
    let result = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || run(cli))
        .expect("spawn worker thread")
        .join()
        .unwrap_or_else(|_| Err("worker thread panicked".to_string()));

    if let Err(err) = result {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    if cli.modules.is_empty() {
        return Err("no modules given; pass at least one, e.g. `lean-checker Init`".into());
    }

    info!("initializing Lean runtime");
    let rt = Runtime::init()?;

    debug!(?cli.lean_path, "initializing search path from sysroot");
    rt.init_search_path(&cli.lean_path)?;

    info!(modules = ?cli.modules, "importing modules");
    let env = rt.import_modules(&cli.modules)?;

    if cli.check || !cli.constants.is_empty() {
        return run_check(&cli, &env);
    }

    run_dump(&cli, &env)
}

fn run_check(cli: &Cli, env: &ffi::Environment) -> Result<(), String> {
    let threads = cli.threads.unwrap_or(1).max(1);
    if cli.constants.is_empty() {
        info!(threads, "checking entire environment");
    } else {
        info!(threads, selected = ?cli.constants, "checking selected constants and dependencies");
    }

    let checked = extract::check_environment(env.raw(), &cli.constants, threads)?;
    info!(checked, "kernel check passed");
    println!("Checked {checked} declarations with no errors");
    Ok(())
}

fn run_dump(cli: &Cli, env: &ffi::Environment) -> Result<(), String> {
    // --- imports ---
    let imports = env.imports();
    info!(count = imports.len(), "imports");
    println!("-- imports ({}) --", imports.len());
    for m in &imports {
        println!("import {m}");
    }

    if cli.imports_only {
        return Ok(());
    }

    // --- constants ---
    let constants = env.constants();
    info!(count = constants.len(), "constants");
    let shown = cli.limit.unwrap_or(constants.len()).min(constants.len());
    println!("\n-- constants ({}) --", constants.len());
    for (name, kind) in constants.iter().take(shown) {
        println!("{kind} {name}");
    }
    if shown < constants.len() {
        println!("... {} more (use --limit to show more)", constants.len() - shown);
    }

    Ok(())
}
