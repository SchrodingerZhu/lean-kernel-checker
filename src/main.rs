//! `lean-checker` (phase 0)
//!
//! A command-line tool that embeds the Lean 4 runtime via FFI, initializes the
//! search path from the detected sysroot (as `leanprover/lean4export`'s
//! `Main.lean` does), imports a set of modules, and dumps their imports and
//! constants.

mod ffi;

use palc::Parser;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::ffi::Runtime;

/// Embed Lean, import modules, and dump their constants and imports.
#[derive(Parser, Debug)]
#[command(name = "lean-checker")]
struct Cli {
    /// Modules to import, e.g. `Init`, `Init.Data.List`, `MyProject.Foo`.
    modules: Vec<String>,

    /// Extra search-path entries (like `LEAN_PATH`), e.g. a local project's
    /// `.lake/build/lib`. May be given multiple times.
    #[arg(long = "lean-path", value_name = "DIR")]
    lean_path: Vec<String>,

    /// Only dump imports; skip the (potentially large) constant listing.
    #[arg(long)]
    imports_only: bool,

    /// Print at most this many constants.
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

    if let Err(err) = run(&cli) {
        tracing::error!("{err}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    if cli.modules.is_empty() {
        return Err("no modules given; pass at least one, e.g. `lean-checker Init`".into());
    }

    info!("initializing Lean runtime");
    let rt = Runtime::init()?;

    debug!(?cli.lean_path, "initializing search path from sysroot");
    rt.init_search_path(&cli.lean_path)?;

    info!(modules = ?cli.modules, "importing modules");
    let env = rt.import_modules(&cli.modules)?;

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
