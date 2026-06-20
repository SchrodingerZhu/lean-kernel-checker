/-
Glue layer between the Rust `lean-checker` binary and the Lean 4 runtime.

Two roles:

1. Environment bootstrap (phase 0): initialise the search path from the detected
   sysroot (as `leanprover/lean4export`'s `Main.lean` does), import modules, and
   report imports / constant names.

2. Kernel extraction (phase 2): hand the Rust side the `ConstantInfo`s of the
   environment plus typed accessors for their metadata. The expression / level /
   name *trees* are returned as opaque `lean_object`s and walked directly in
   Rust; only the scalar metadata (kinds, arities, recursor rules, …) and the
   dependency edges are computed here, where the Lean API makes them trivial.

Everything is `@[export]`-ed with a flat, C-friendly surface. Accessors are pure
and consume their argument under Lean's calling convention; the Rust side keeps
sources alive by incrementing refcounts before each call.
-/
import Lean
open Lean

/-! ## Phase 0: bootstrap -/

/-- `initSearchPath (← findSysroot)`, plus any extra `LEAN_PATH`-style entries. -/
@[export lc_init_search_path]
def lcInitSearchPath (extra : Array String) : IO Unit := do
  initSearchPath (← findSysroot) (extra.toList.map System.FilePath.mk)

/-- Import `mods` and return the resulting kernel environment as an opaque handle. -/
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

/-! ## Phase 2: constant extraction -/

/-- Every constant in the environment (no filtering: the kernel needs the full
    dependency closure, including compiler-internal declarations). -/
@[export lc_env_all_consts]
def lcEnvAllConsts (env : Environment) : Array ConstantInfo := Id.run do
  let mut out := #[]
  for (_, ci) in env.constants.toList do
    out := out.push ci
  return out

/-- Resolve a name to its `ConstantInfo`, if present. -/
@[export lc_env_find]
def lcEnvFind (env : Environment) (n : Name) : Option ConstantInfo :=
  env.find? n

/-- The constants referenced by a declaration's type, value, and (for
    recursors) computation-rule right-hand sides — its dependency edges. -/
@[export lc_ci_deps]
def lcCiDeps (ci : ConstantInfo) : Array Name := Id.run do
  let mut s := ci.type.getUsedConstants
  -- `allowOpaque` so the dependency edges match the values we actually walk.
  if let some v := ci.value? (allowOpaque := true) then
    s := s ++ v.getUsedConstants
  if let .recInfo rv := ci then
    for r in rv.rules do
      s := s ++ r.rhs.getUsedConstants
  return s

/-- Discriminant: 0 axiom, 1 def, 2 theorem, 3 opaque, 4 quot, 5 inductive,
    6 constructor, 7 recursor. -/
@[export lc_ci_kind]
def lcCiKind (ci : ConstantInfo) : UInt8 :=
  match ci with
  | .axiomInfo _  => 0
  | .defnInfo _   => 1
  | .thmInfo _    => 2
  | .opaqueInfo _ => 3
  | .quotInfo _   => 4
  | .inductInfo _ => 5
  | .ctorInfo _   => 6
  | .recInfo _    => 7

/-! Common fields (valid for every `ConstantInfo`). -/

@[export lc_ci_name]
def lcCiName (ci : ConstantInfo) : Name := ci.name

/-- The dotted string form of a name (used as a stable key on the Rust side). -/
@[export lc_name_to_string]
def lcNameToString (n : Name) : String := n.toString

@[export lc_ci_level_params]
def lcCiLevelParams (ci : ConstantInfo) : Array Name := ci.levelParams.toArray

@[export lc_ci_type]
def lcCiType (ci : ConstantInfo) : Expr := ci.type

/-- The value of a def / theorem / opaque (else `none`). `allowOpaque` so that
    opaque definitions expose their body to the kernel. -/
@[export lc_ci_value]
def lcCiValue (ci : ConstantInfo) : Option Expr := ci.value? (allowOpaque := true)

/-! Definition reducibility hint: kind 0 opaque, 1 abbrev, 2 regular(height). -/

@[export lc_ci_def_hint_kind]
def lcCiDefHintKind (ci : ConstantInfo) : UInt8 :=
  match ci with
  | .defnInfo dv => match dv.hints with
    | .opaque    => 0
    | .abbrev    => 1
    | .regular _ => 2
  | _ => 0

@[export lc_ci_def_hint_height]
def lcCiDefHintHeight (ci : ConstantInfo) : UInt32 :=
  match ci with
  | .defnInfo dv => match dv.hints with
    | .regular h => h
    | _          => 0
  | _ => 0

/-! Inductive metadata. -/

@[export lc_ci_ind_num_params]
def lcCiIndNumParams (ci : ConstantInfo) : UInt32 :=
  match ci with | .inductInfo iv => UInt32.ofNat iv.numParams | _ => 0

@[export lc_ci_ind_num_indices]
def lcCiIndNumIndices (ci : ConstantInfo) : UInt32 :=
  match ci with | .inductInfo iv => UInt32.ofNat iv.numIndices | _ => 0

@[export lc_ci_ind_is_rec]
def lcCiIndIsRec (ci : ConstantInfo) : UInt8 :=
  match ci with | .inductInfo iv => (if iv.isRec then 1 else 0) | _ => 0

@[export lc_ci_ind_num_nested]
def lcCiIndNumNested (ci : ConstantInfo) : UInt32 :=
  match ci with | .inductInfo iv => UInt32.ofNat iv.numNested | _ => 0

@[export lc_ci_ind_all]
def lcCiIndAll (ci : ConstantInfo) : Array Name :=
  match ci with | .inductInfo iv => iv.all.toArray | _ => #[]

@[export lc_ci_ind_ctors]
def lcCiIndCtors (ci : ConstantInfo) : Array Name :=
  match ci with | .inductInfo iv => iv.ctors.toArray | _ => #[]

/-! Constructor metadata. -/

@[export lc_ci_ctor_induct]
def lcCiCtorInduct (ci : ConstantInfo) : Name :=
  match ci with | .ctorInfo cv => cv.induct | _ => Name.anonymous

@[export lc_ci_ctor_cidx]
def lcCiCtorCidx (ci : ConstantInfo) : UInt32 :=
  match ci with | .ctorInfo cv => UInt32.ofNat cv.cidx | _ => 0

@[export lc_ci_ctor_num_params]
def lcCiCtorNumParams (ci : ConstantInfo) : UInt32 :=
  match ci with | .ctorInfo cv => UInt32.ofNat cv.numParams | _ => 0

@[export lc_ci_ctor_num_fields]
def lcCiCtorNumFields (ci : ConstantInfo) : UInt32 :=
  match ci with | .ctorInfo cv => UInt32.ofNat cv.numFields | _ => 0

/-! Recursor metadata. -/

@[export lc_ci_rec_num_params]
def lcCiRecNumParams (ci : ConstantInfo) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat rv.numParams | _ => 0

@[export lc_ci_rec_num_indices]
def lcCiRecNumIndices (ci : ConstantInfo) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat rv.numIndices | _ => 0

@[export lc_ci_rec_num_motives]
def lcCiRecNumMotives (ci : ConstantInfo) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat rv.numMotives | _ => 0

@[export lc_ci_rec_num_minors]
def lcCiRecNumMinors (ci : ConstantInfo) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat rv.numMinors | _ => 0

@[export lc_ci_rec_k]
def lcCiRecK (ci : ConstantInfo) : UInt8 :=
  match ci with | .recInfo rv => (if rv.k then 1 else 0) | _ => 0

@[export lc_ci_rec_all]
def lcCiRecAll (ci : ConstantInfo) : Array Name :=
  match ci with | .recInfo rv => rv.all.toArray | _ => #[]

@[export lc_ci_rec_num_rules]
def lcCiRecNumRules (ci : ConstantInfo) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat rv.rules.length | _ => 0

@[export lc_ci_rec_rule_ctor]
def lcCiRecRuleCtor (ci : ConstantInfo) (i : UInt32) : Name :=
  match ci with | .recInfo rv => (rv.rules[i.toNat]?.map (·.ctor)).getD Name.anonymous | _ => Name.anonymous

@[export lc_ci_rec_rule_nfields]
def lcCiRecRuleNfields (ci : ConstantInfo) (i : UInt32) : UInt32 :=
  match ci with | .recInfo rv => UInt32.ofNat ((rv.rules[i.toNat]?.map (·.nfields)).getD 0) | _ => 0

@[export lc_ci_rec_rule_rhs]
def lcCiRecRuleRhs (ci : ConstantInfo) (i : UInt32) : Expr :=
  match ci with
  | .recInfo rv => (rv.rules[i.toNat]?.map (·.rhs)).getD (Expr.sort .zero)
  | _ => Expr.sort .zero

/-! Quotient metadata: 0 type, 1 ctor, 2 lift, 3 ind. -/

@[export lc_ci_quot_kind]
def lcCiQuotKind (ci : ConstantInfo) : UInt8 :=
  match ci with
  | .quotInfo qv => match qv.kind with
    | .type => 0
    | .ctor => 1
    | .lift => 2
    | .ind  => 3
  | _ => 0
