//! Build script: compile the Lean glue module (`glue/Glue.lean`) down to a
//! native object file and link it into the binary.
//!
//! Pipeline:  `Glue.lean` --(lean --c)--> `Glue.c` --(leanc -c)--> `Glue.o`
//!
//! The actual Lean runtime (`libleanshared`) is linked by the `lean-sys`
//! build script; here we only need to add our own object and make sure the
//! Lean library search path is reachable for the glue's `import Lean`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=glue/Glue.lean");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let glue_dir = manifest.join("glue");
    let glue_c = out_dir.join("Glue.c");
    let glue_o = out_dir.join("Glue.o");

    // 1. Lean source -> C.  `lean` resolves `import Lean` against its own
    //    toolchain prefix, so no extra search path is required here.  We run it
    //    from the `glue/` directory and pass the bare file name so the module is
    //    named `Glue` (hence the generated initializer is `initialize_Glue`),
    //    rather than `glue.Glue` if a path were passed.
    run(
        Command::new("lean")
            .current_dir(&glue_dir)
            .arg(format!("--c={}", glue_c.display()))
            .arg("Glue.lean"),
        "lean --c",
    );

    // 2. C -> object, using `leanc` so the correct `lean.h` include path and
    //    compiler flags are applied.
    run(
        Command::new("leanc")
            .args(["-c", "-o"])
            .arg(&glue_o)
            .arg(&glue_c),
        "leanc -c",
    );

    // 3. Link the glue object into our binary.
    println!("cargo:rustc-link-arg={}", glue_o.display());
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{what}`: {e}"));
    if !status.success() {
        panic!("`{what}` failed with {status}");
    }
}
