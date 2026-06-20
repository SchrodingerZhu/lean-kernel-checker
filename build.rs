//! Build script: compile the glue layer and link it into the binary.
//!
//! Pipeline:
//!   `Glue.lean` --(lean --c)--> `Glue.c` ─┐
//!   `nat_bytes.c` ───────────────────────┴─(cc + leanc cflags)--> static lib
//!
//! We generate C from Lean with `lean --c`, then compile it with the `cc` crate
//! using the flags reported by `leanc --print-cflags` (the Lean include dir and
//! friends) rather than invoking `leanc` as a compiler ourselves. The GMP
//! library that `nat_bytes.c` calls is discovered from `leanc --print-ldflags`,
//! so nothing here hard-codes a toolchain path. (`libleanshared` itself is
//! linked by the `lean-sys` build script.)

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=glue/Glue.lean");
    println!("cargo:rerun-if-changed=glue/nat_bytes.c");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let glue_dir = manifest.join("glue");
    let glue_c = out_dir.join("Glue.c");
    let nat_c = glue_dir.join("nat_bytes.c");

    // 1. Lean source -> C.  Run `lean` from the `glue/` directory with the bare
    //    file name so the module is `Glue` (hence the generated initializer is
    //    `initialize_Glue`, not `initialize_glue_Glue`).
    run(
        Command::new("lean")
            .current_dir(&glue_dir)
            .arg(format!("--c={}", glue_c.display()))
            .arg("Glue.lean"),
        "lean --c",
    );

    // 2. Compile the C with `cc`, applying `leanc`'s own cflags (Lean include
    //    dir, codegen flags) so `lean.h` resolves correctly.
    let mut build = cc::Build::new();
    build.file(&glue_c).file(&nat_c);
    apply_flags(&mut build, &leanc_flags("--print-cflags"));
    build.compile("lcglue");

    // 3. Link the GMP library that `leanc` links, so `nat_bytes.c`'s `__gmpz_*`
    //    references resolve.
    link_gmp(&leanc_flags("--print-ldflags"));
}

/// Tokens printed by `leanc --print-{c,ld}flags`.
fn leanc_flags(which: &str) -> Vec<String> {
    let out = Command::new("leanc").arg(which).output().unwrap_or_else(|e| panic!("spawn `leanc {which}`: {e}"));
    if !out.status.success() {
        panic!("`leanc {which}` failed with {}", out.status);
    }
    String::from_utf8_lossy(&out.stdout).split_whitespace().map(str::to_string).collect()
}

/// Feed compiler flags into a `cc::Build`, turning `-I <dir>` (and `-I<dir>`)
/// into include paths and passing everything else through verbatim.
fn apply_flags(build: &mut cc::Build, flags: &[String]) {
    let mut i = 0;
    while i < flags.len() {
        let f = &flags[i];
        if f == "-I" {
            if let Some(dir) = flags.get(i + 1) {
                build.include(dir);
                i += 2;
                continue;
            }
        } else if let Some(dir) = f.strip_prefix("-I") {
            build.include(dir);
        } else {
            build.flag(f);
        }
        i += 1;
    }
}

/// Emit the cargo directives needed to link GMP, derived from `leanc`'s ldflags.
/// Handles both an absolute `libgmp` path (Nix) and a `-L<dir> -lgmp` pair
/// (typical elsewhere); always ends by requesting `-lgmp`.
fn link_gmp(ldflags: &[String]) {
    let is_gmp_lib = |t: &str| {
        t.contains("libgmp") && (t.ends_with(".so") || t.contains(".so.") || t.ends_with(".dylib") || t.ends_with(".a"))
    };

    if let Some(path) = ldflags.iter().find(|t| is_gmp_lib(t) && std::path::Path::new(t.as_str()).is_absolute()) {
        if let Some(dir) = std::path::Path::new(path).parent() {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    } else {
        // Pass through any `-L<dir>` search paths from the ldflags.
        let mut i = 0;
        while i < ldflags.len() {
            let f = &ldflags[i];
            if f == "-L" {
                if let Some(dir) = ldflags.get(i + 1) {
                    println!("cargo:rustc-link-search=native={dir}");
                    i += 2;
                    continue;
                }
            } else if let Some(dir) = f.strip_prefix("-L") {
                if !dir.is_empty() {
                    println!("cargo:rustc-link-search=native={dir}");
                }
            }
            i += 1;
        }
    }

    println!("cargo:rustc-link-lib=gmp");
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to spawn `{what}`: {e}"));
    if !status.success() {
        panic!("`{what}` failed with {status}");
    }
}
