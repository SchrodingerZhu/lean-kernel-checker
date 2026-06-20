/-
Glue layer between the Rust `lean-checker` binary and the Lean 4 runtime.

The Rust side cannot conveniently build the Lean-typed arguments that the real
`Lean.importModules` / `Lean.initSearchPath` API expects (e.g. `Array Import`,
`Options`).  Instead we expose a handful of `@[export]`-ed wrappers with a flat,
C-friendly surface — every function takes/returns `Array String`, `String`, or
an opaque `Environment` handle — and let Lean do the heavy lifting.

This mirrors the bootstrap performed by `leanprover/lean4export`'s `Main.lean`:
initialise the search path from the detected sysroot, import the requested
modules, then read back the imports and constants of the resulting environment.
-/
import Lean
open Lean

/-- `initSearchPath (← findSysroot)`, plus any extra `LEAN_PATH`-style entries
    supplied by the caller (e.g. a local project's `.lake/build/lib`). -/
@[export lc_init_search_path]
def lcInitSearchPath (extra : Array String) : IO Unit := do
  initSearchPath (← findSysroot) (extra.toList.map System.FilePath.mk)

/-- Import `mods` and return the resulting kernel environment as an opaque
    handle.  The caller is responsible for releasing it. -/
@[export lc_import_modules]
def lcImportModules (mods : Array String) : IO Environment := do
  let imports := mods.map fun m => ({ module := m.toName } : Import)
  importModules imports {}

/-- Module names of the environment's direct imports. -/
@[export lc_env_imports]
def lcEnvImports (env : Environment) : Array String :=
  env.imports.map (·.module.toString)

/-- A short, lean4export-flavoured tag for the kind of a constant. -/
private def constKind : ConstantInfo → String
  | .axiomInfo _  => "axiom"
  | .defnInfo _   => "def"
  | .thmInfo _    => "theorem"
  | .opaqueInfo _ => "opaque"
  | .quotInfo _   => "quot"
  | .inductInfo _ => "inductive"
  | .ctorInfo _   => "ctor"
  | .recInfo _    => "rec"

/-- The non-internal constant names of the environment, in `constants` order. -/
@[export lc_env_const_names]
def lcEnvConstNames (env : Environment) : Array String := Id.run do
  let mut out := #[]
  for (n, _) in env.constants.toList do
    if !n.isInternal then
      out := out.push n.toString
  return out

/-- The constant kinds, parallel (same order/filter) to `lc_env_const_names`. -/
@[export lc_env_const_kinds]
def lcEnvConstKinds (env : Environment) : Array String := Id.run do
  let mut out := #[]
  for (n, ci) in env.constants.toList do
    if !n.isInternal then
      out := out.push (constKind ci)
  return out
