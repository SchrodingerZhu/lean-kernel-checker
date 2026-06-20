{
  description = "lean-checker dev environment (Rust + LLVM 22 + Lean 4)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Latest nightly Rust via rust-overlay, with the usual dev components.
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        # Full LLVM 22 toolchain (clang + lld + tools).
        llvm = pkgs.llvmPackages_22;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            # Rust
            rustToolchain

            # LLVM 22 toolchain
            llvm.clang        # wrapped clang / clang++
            llvm.lld          # ld.lld linker
            llvm.bintools     # llvm-ar, llvm-nm, llvm-objcopy, ...
            llvm.llvm         # opt, llc, llvm-config, ...
            llvm.lldb         # debugger

            # Lean 4 + Lake
            pkgs.lean4
          ];

          # Point bindgen-style tooling at LLVM 22's libclang.
          LIBCLANG_PATH = "${llvm.libclang.lib}/lib";

          shellHook = ''
            export CC=clang
            export CXX=clang++
            echo "lean-checker dev shell"
            echo "  rust : $(rustc --version)"
            echo "  clang: $(clang --version | head -n1)"
            echo "  lld  : $(ld.lld --version)"
            echo "  lean : $(lean --version)"
            echo "  lake : $(lake --version)"
          '';
        };
      });
}
