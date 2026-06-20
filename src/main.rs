//! `lean-checker`
//!
//! Embeds the Lean 4 runtime via FFI, initializes the search path from the
//! detected sysroot (as `leanprover/lean4export`'s `Main.lean` does), imports a
//! set of modules, then walks the environment's `lean_object`s and re-checks
//! every declaration with the embedded `sokonanoda` kernel — all in one shot.

mod extract;
mod ffi;

use palc::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::ffi::Runtime;

/// 1 GiB worker stack: the FFI walkers recurse deeply over the imported terms.
const STACK_SIZE: usize = 1 << 30;

/// Import Lean modules and type-check their declarations with the kernel.
#[derive(Parser, Debug)]
#[command(name = "lean-checker")]
struct Cli {
    /// Modules to import and check, e.g. `Init`, `Init.Data.List`, `MyProject.Foo`.
    modules: Vec<String>,

    /// Extra search-path entries (like `LEAN_PATH`), e.g. a local project's
    /// `.lake/build/lib/lean`. May be given multiple times.
    #[arg(long = "lean-path", value_name = "DIR")]
    lean_path: Vec<String>,

    /// Restrict checking to these constants and their transitive dependencies.
    /// May be given multiple times. (Default: check the whole environment.)
    #[arg(long = "const", value_name = "NAME")]
    constants: Vec<String>,

    /// Number of kernel checking threads (default: available parallelism).
    #[arg(long, value_name = "N")]
    threads: Option<usize>,
}

fn main() {
    // Progress logging via `tracing` on stderr (so stdout carries only the
    // result); level controlled by `RUST_LOG` (default `info`).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Run everything on one large-stack thread: the Lean runtime must be driven
    // from a single thread, and the FFI walkers recurse far past the default
    // 8 MiB stack.
    let ok = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || run(cli))
        .expect("spawn worker thread")
        .join()
        .unwrap_or_else(|_| Err("worker thread panicked".to_string()));

    match ok {
        Ok(true) => {}
        Ok(false) => std::process::exit(1), // checking found errors
        Err(err) => {
            error!("{err}");
            std::process::exit(1);
        }
    }
}

/// Returns `Ok(true)` if everything checked, `Ok(false)` if the kernel rejected
/// some declaration, `Err` on a setup failure.
fn run(cli: Cli) -> Result<bool, String> {
    if cli.modules.is_empty() {
        return Err("no modules given; pass at least one, e.g. `lean-checker Init`".into());
    }

    info!("initializing Lean runtime");
    let rt = Runtime::init()?;
    rt.init_search_path(&cli.lean_path)?;

    info!(modules = ?cli.modules, "importing modules");
    let env = rt.import_modules(&cli.modules)?;

    let threads = cli.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    if cli.constants.is_empty() {
        info!(threads, "checking environment");
    } else {
        info!(threads, selected = ?cli.constants, "checking selected constants and dependencies");
    }

    let report = extract::check_environment(env.raw(), &cli.constants, threads);

    if report.failures.is_empty() {
        info!(checked = report.total, "kernel check passed");
        println!("Checked {} declarations with no errors", report.total);
        Ok(true)
    } else {
        for (name, msg) in &report.failures {
            error!(declaration = %name, "{msg}");
        }
        println!(
            "Checked {} declarations: {} failed",
            report.total,
            report.failures.len()
        );
        Ok(false)
    }
}
