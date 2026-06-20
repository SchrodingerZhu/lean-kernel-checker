//! Programmatic construction of a checkable [`ExportFile`].
//!
//! Historically the environment was materialised by parsing lean4export's text
//! (or JSON) format. That parser has been removed; instead, a producer (for
//! example a Rust FFI layer that walks Lean `lean_object`s directly) drives the
//! [`Builder`] below to intern names/levels/expressions into the persistent
//! hash-consed DAG and submit declarations, then calls [`Builder::finish`] to
//! obtain an [`ExportFile`] whose `check_all_declars` runs the kernel.
//!
//! The interning here is byte-for-byte equivalent to what the old parser did:
//! the same `FxHasher`-based hashes (so structurally-equal terms dedupe), the
//! same `Name::Anon` / `Level::Zero` pre-seeded at index 0, and the same
//! `ExportFile`-tagged [`Ptr`]s.
//!
//! Pointers returned by the builder are valid only for *this* builder's DAG;
//! callers thread them back in to build larger terms.

use std::sync::Arc;

use num_bigint::BigUint;

use crate::env::{
    ConstructorData, Declar, DeclarInfo, InductiveData, NotationMap, RecRule, RecursorData, ReducibilityHint,
};
use crate::expr::{
    BinderStyle, Expr, APP_HASH, CONST_HASH, LAMBDA_HASH, LET_HASH, NAT_LIT_HASH, PI_HASH, PROJ_HASH, SORT_HASH,
    STRING_LIT_HASH, VAR_HASH,
};
use crate::hash64;
use crate::level::{Level, IMAX_HASH, MAX_HASH, PARAM_HASH, SUCC_HASH};
use crate::name::{Name, NUM_HASH, STR_HASH};
use crate::util::{
    new_fx_hash_map, new_fx_index_map, BigUintPtr, Config, CowStr, DagMarker, ExportFile, ExprPtr, FxHashMap,
    FxIndexMap, LeanDag, LevelPtr, LevelsPtr, NamePtr, Ptr, StringPtr,
};

/// Builds the persistent DAG and declaration set for an [`ExportFile`].
pub struct Builder<'p> {
    dag: LeanDag<'p>,
    declars: FxIndexMap<NamePtr<'p>, Declar<'p>>,
    notations: NotationMap<'p>,
    mutual_block_sizes: FxHashMap<NamePtr<'p>, (usize, usize)>,
    config: Config,
}

impl<'p> Builder<'p> {
    /// A fresh builder with `Name::Anon` and `Level::Zero` pre-seeded at index 0
    /// (the convention the rest of the kernel relies on).
    pub fn new(config: Config) -> Self {
        Self {
            dag: LeanDag::new(&config),
            declars: new_fx_index_map(),
            notations: new_fx_hash_map(),
            mutual_block_sizes: new_fx_hash_map(),
            config,
        }
    }

    /// Consume the builder, producing a checkable environment. Run the kernel
    /// with `export_file.check_all_declars()`.
    pub fn finish(self) -> ExportFile<'p> {
        let name_cache = self.dag.mk_name_cache();
        ExportFile {
            dag: self.dag,
            declars: self.declars,
            notations: self.notations,
            name_cache,
            config: self.config,
            mutual_block_sizes: self.mutual_block_sizes,
        }
    }

    // --- names ------------------------------------------------------------

    /// The anonymous name (the root of every `Name`).
    pub fn name_anon(&self) -> NamePtr<'p> { Ptr::from(DagMarker::ExportFile, 0) }

    /// `pre.s` — extend a name with a string component.
    pub fn name_str(&mut self, pre: NamePtr<'p>, s: &str) -> NamePtr<'p> {
        let sfx = self.intern_string(s);
        let hash = hash64!(STR_HASH, pre, sfx);
        let (idx, _) = self.dag.names.insert_full(Name::Str(pre, sfx, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    /// `pre.n` — extend a name with a numeric component.
    pub fn name_num(&mut self, pre: NamePtr<'p>, n: u64) -> NamePtr<'p> {
        let hash = hash64!(NUM_HASH, pre, n);
        let (idx, _) = self.dag.names.insert_full(Name::Num(pre, n, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    // --- levels -----------------------------------------------------------

    /// Universe level `0`.
    pub fn level_zero(&self) -> LevelPtr<'p> { Ptr::from(DagMarker::ExportFile, 0) }

    pub fn level_succ(&mut self, l: LevelPtr<'p>) -> LevelPtr<'p> {
        let hash = hash64!(SUCC_HASH, l);
        let (idx, _) = self.dag.levels.insert_full(Level::Succ(l, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    pub fn level_max(&mut self, l: LevelPtr<'p>, r: LevelPtr<'p>) -> LevelPtr<'p> {
        let hash = hash64!(MAX_HASH, l, r);
        let (idx, _) = self.dag.levels.insert_full(Level::Max(l, r, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    pub fn level_imax(&mut self, l: LevelPtr<'p>, r: LevelPtr<'p>) -> LevelPtr<'p> {
        let hash = hash64!(IMAX_HASH, l, r);
        let (idx, _) = self.dag.levels.insert_full(Level::IMax(l, r, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    pub fn level_param(&mut self, n: NamePtr<'p>) -> LevelPtr<'p> {
        let hash = hash64!(PARAM_HASH, n);
        let (idx, _) = self.dag.levels.insert_full(Level::Param(n, hash));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    /// Intern a list of levels (used for both a declaration's universe
    /// parameters and a `const`'s universe arguments).
    pub fn levels(&mut self, ls: &[LevelPtr<'p>]) -> LevelsPtr<'p> {
        let (idx, _) = self.dag.uparams.insert_full(Arc::from(ls.to_vec()));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    // --- expressions ------------------------------------------------------

    /// A bound variable (de Bruijn index).
    pub fn expr_var(&mut self, dbj_idx: u16) -> ExprPtr<'p> {
        let hash = hash64!(VAR_HASH, dbj_idx);
        self.push_expr(Expr::Var { dbj_idx, hash })
    }

    pub fn expr_sort(&mut self, level: LevelPtr<'p>) -> ExprPtr<'p> {
        let hash = hash64!(SORT_HASH, level);
        self.push_expr(Expr::Sort { level, hash })
    }

    pub fn expr_const(&mut self, name: NamePtr<'p>, levels: LevelsPtr<'p>) -> ExprPtr<'p> {
        let hash = hash64!(CONST_HASH, name, levels);
        self.push_expr(Expr::Const { name, levels, hash })
    }

    pub fn expr_app(&mut self, fun: ExprPtr<'p>, arg: ExprPtr<'p>) -> ExprPtr<'p> {
        let hash = hash64!(APP_HASH, fun, arg);
        let num_loose_bvars = self.num_loose_bvars(fun).max(self.num_loose_bvars(arg));
        let has_fvars = self.has_fvars(fun) || self.has_fvars(arg);
        self.push_expr(Expr::App { fun, arg, num_loose_bvars, has_fvars, hash })
    }

    pub fn expr_lambda(
        &mut self,
        binder_name: NamePtr<'p>,
        binder_style: BinderStyle,
        binder_type: ExprPtr<'p>,
        body: ExprPtr<'p>,
    ) -> ExprPtr<'p> {
        let hash = hash64!(LAMBDA_HASH, binder_name, binder_style, binder_type, body);
        let num_loose_bvars = self.num_loose_bvars(binder_type).max(self.num_loose_bvars(body).saturating_sub(1));
        let has_fvars = self.has_fvars(binder_type) || self.has_fvars(body);
        self.push_expr(Expr::Lambda { binder_name, binder_style, binder_type, body, num_loose_bvars, has_fvars, hash })
    }

    pub fn expr_pi(
        &mut self,
        binder_name: NamePtr<'p>,
        binder_style: BinderStyle,
        binder_type: ExprPtr<'p>,
        body: ExprPtr<'p>,
    ) -> ExprPtr<'p> {
        let hash = hash64!(PI_HASH, binder_name, binder_style, binder_type, body);
        let num_loose_bvars = self.num_loose_bvars(binder_type).max(self.num_loose_bvars(body).saturating_sub(1));
        let has_fvars = self.has_fvars(binder_type) || self.has_fvars(body);
        self.push_expr(Expr::Pi { binder_name, binder_style, binder_type, body, num_loose_bvars, has_fvars, hash })
    }

    pub fn expr_let(
        &mut self,
        binder_name: NamePtr<'p>,
        binder_type: ExprPtr<'p>,
        val: ExprPtr<'p>,
        body: ExprPtr<'p>,
        nondep: bool,
    ) -> ExprPtr<'p> {
        let hash = hash64!(LET_HASH, binder_name, binder_type, val, body, nondep);
        let num_loose_bvars = self
            .num_loose_bvars(binder_type)
            .max(self.num_loose_bvars(val).max(self.num_loose_bvars(body).saturating_sub(1)));
        let has_fvars = self.has_fvars(binder_type) || self.has_fvars(val) || self.has_fvars(body);
        self.push_expr(Expr::Let { binder_name, binder_type, val, body, num_loose_bvars, has_fvars, hash, nondep })
    }

    pub fn expr_proj(&mut self, ty_name: NamePtr<'p>, idx: usize, structure: ExprPtr<'p>) -> ExprPtr<'p> {
        let hash = hash64!(PROJ_HASH, ty_name, idx, structure);
        let num_loose_bvars = self.num_loose_bvars(structure);
        let has_fvars = self.has_fvars(structure);
        self.push_expr(Expr::Proj { ty_name, idx, structure, num_loose_bvars, has_fvars, hash })
    }

    /// A `Nat` literal. Requires `config.nat_extension`.
    pub fn expr_nat_lit(&mut self, n: BigUint) -> ExprPtr<'p> {
        let i = self
            .dag
            .bignums
            .as_mut()
            .expect("expr_nat_lit requires config.nat_extension")
            .insert_full(n)
            .0;
        let ptr: BigUintPtr<'p> = Ptr::from(DagMarker::ExportFile, i);
        let hash = hash64!(NAT_LIT_HASH, ptr);
        self.push_expr(Expr::NatLit { ptr, hash })
    }

    /// A `String` literal.
    pub fn expr_string_lit(&mut self, s: &str) -> ExprPtr<'p> {
        let ptr = self.intern_string(s);
        let hash = hash64!(STRING_LIT_HASH, ptr);
        self.push_expr(Expr::StringLit { ptr, hash })
    }

    // --- declarations -----------------------------------------------------

    pub fn add_axiom(&mut self, name: NamePtr<'p>, uparams: LevelsPtr<'p>, ty: ExprPtr<'p>) {
        let info = DeclarInfo { name, uparams, ty };
        self.insert_declar(name, Declar::Axiom { info });
    }

    pub fn add_quot(&mut self, name: NamePtr<'p>, uparams: LevelsPtr<'p>, ty: ExprPtr<'p>) {
        let info = DeclarInfo { name, uparams, ty };
        self.insert_declar(name, Declar::Quot { info });
    }

    pub fn add_theorem(&mut self, name: NamePtr<'p>, uparams: LevelsPtr<'p>, ty: ExprPtr<'p>, val: ExprPtr<'p>) {
        let info = DeclarInfo { name, uparams, ty };
        self.insert_declar(name, Declar::Theorem { info, val });
    }

    pub fn add_definition(
        &mut self,
        name: NamePtr<'p>,
        uparams: LevelsPtr<'p>,
        ty: ExprPtr<'p>,
        val: ExprPtr<'p>,
        hint: ReducibilityHint,
    ) {
        let info = DeclarInfo { name, uparams, ty };
        self.insert_declar(name, Declar::Definition { info, val, hint });
    }

    pub fn add_opaque(&mut self, name: NamePtr<'p>, uparams: LevelsPtr<'p>, ty: ExprPtr<'p>, val: ExprPtr<'p>) {
        let info = DeclarInfo { name, uparams, ty };
        self.insert_declar(name, Declar::Opaque { info, val });
    }

    /// Submit a whole mutual inductive block (the unit Lean exports together:
    /// the inductive types, then all their constructors, then all recursors).
    ///
    /// The block is registered contiguously, in that order, and each inductive
    /// name is recorded in `mutual_block_sizes` — both of which the inductive
    /// checker relies on. Mirrors the old parser's handling exactly.
    pub fn add_inductive_block(
        &mut self,
        inds: Vec<InductiveInput<'p>>,
        ctors: Vec<ConstructorInput<'p>>,
        recs: Vec<RecursorInput<'p>>,
    ) {
        let block_start = self.declars.len();
        let block_size = inds.len() + ctors.len() + recs.len();

        for ind in inds {
            self.mutual_block_sizes.insert(ind.name, (block_start, block_size));
            let info = DeclarInfo { name: ind.name, uparams: ind.uparams, ty: ind.ty };
            let declar = Declar::Inductive(InductiveData {
                info,
                is_recursive: ind.is_recursive,
                is_nested: ind.is_nested,
                num_params: ind.num_params,
                num_indices: ind.num_indices,
                all_ind_names: Arc::from(ind.all_ind_names),
                all_ctor_names: Arc::from(ind.all_ctor_names),
            });
            self.insert_declar(ind.name, declar);
        }

        for ctor in ctors {
            let info = DeclarInfo { name: ctor.name, uparams: ctor.uparams, ty: ctor.ty };
            let declar = Declar::Constructor(ConstructorData {
                info,
                inductive_name: ctor.inductive_name,
                ctor_idx: ctor.ctor_idx,
                num_params: ctor.num_params,
                num_fields: ctor.num_fields,
            });
            self.insert_declar(ctor.name, declar);
        }

        for rec in recs {
            let info = DeclarInfo { name: rec.name, uparams: rec.uparams, ty: rec.ty };
            let rec_rules = rec
                .rec_rules
                .into_iter()
                .map(|r| RecRule { ctor_name: r.ctor_name, ctor_telescope_size_wo_params: r.num_fields, val: r.rhs })
                .collect::<Vec<_>>();
            let declar = Declar::Recursor(RecursorData {
                info,
                all_inductives: Arc::from(rec.all_inductives),
                num_params: rec.num_params,
                num_indices: rec.num_indices,
                num_motives: rec.num_motives,
                num_minors: rec.num_minors,
                rec_rules: Arc::from(rec_rules),
                is_k: rec.is_k,
            });
            self.insert_declar(rec.name, declar);
        }
    }

    // --- internals --------------------------------------------------------

    fn intern_string(&mut self, s: &str) -> StringPtr<'p> {
        let (idx, _) = self.dag.strings.insert_full(CowStr::Owned(s.to_string()));
        Ptr::from(DagMarker::ExportFile, idx)
    }

    fn push_expr(&mut self, e: Expr<'p>) -> ExprPtr<'p> {
        let (idx, _) = self.dag.exprs.insert_full(e);
        Ptr::from(DagMarker::ExportFile, idx)
    }

    fn num_loose_bvars(&self, e: ExprPtr<'p>) -> u16 {
        self.dag.exprs.get_index(e.idx()).unwrap().num_loose_bvars()
    }

    fn has_fvars(&self, e: ExprPtr<'p>) -> bool { self.dag.exprs.get_index(e.idx()).unwrap().has_fvars() }

    fn insert_declar(&mut self, name: NamePtr<'p>, declar: Declar<'p>) {
        assert!(self.declars.insert(name, declar).is_none(), "duplicate declaration submitted to builder");
    }
}

/// One inductive type within a mutual block, for [`Builder::add_inductive_block`].
pub struct InductiveInput<'p> {
    pub name: NamePtr<'p>,
    pub uparams: LevelsPtr<'p>,
    pub ty: ExprPtr<'p>,
    pub is_recursive: bool,
    /// Whether this inductive has nested occurrences (`numNested > 0`).
    pub is_nested: bool,
    pub num_params: u16,
    pub num_indices: u16,
    /// All inductive types in this mutual block.
    pub all_ind_names: Vec<NamePtr<'p>>,
    /// The constructors of *this* inductive.
    pub all_ctor_names: Vec<NamePtr<'p>>,
}

/// One constructor, for [`Builder::add_inductive_block`].
pub struct ConstructorInput<'p> {
    pub name: NamePtr<'p>,
    pub uparams: LevelsPtr<'p>,
    pub ty: ExprPtr<'p>,
    pub inductive_name: NamePtr<'p>,
    pub ctor_idx: u16,
    pub num_params: u16,
    pub num_fields: u16,
}

/// One recursor, for [`Builder::add_inductive_block`].
pub struct RecursorInput<'p> {
    pub name: NamePtr<'p>,
    pub uparams: LevelsPtr<'p>,
    pub ty: ExprPtr<'p>,
    pub all_inductives: Vec<NamePtr<'p>>,
    pub num_params: u16,
    pub num_indices: u16,
    pub num_motives: u16,
    pub num_minors: u16,
    pub rec_rules: Vec<RecRuleInput<'p>>,
    pub is_k: bool,
}

/// One recursor computation rule, for [`RecursorInput`].
pub struct RecRuleInput<'p> {
    pub ctor_name: NamePtr<'p>,
    /// The constructor's argument count excluding the inductive's parameters
    /// (`ctor_telescope_size_wo_params`).
    pub num_fields: u16,
    pub rhs: ExprPtr<'p>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Submit a real declaration through the public builder API and run the
    /// kernel on it: `def Foo : Type 0 := Prop` (i.e. `Sort 1 := Sort 0`),
    /// which is well-typed and must be accepted.
    #[test]
    fn builder_checks_a_definition() {
        let mut b = Builder::new(Config::default());

        let anon = b.name_anon();
        let foo = b.name_str(anon, "Foo");
        let uparams = b.levels(&[]);
        let one = b.level_succ(b.level_zero());
        let type0 = b.expr_sort(one); // Sort 1
        let zero = b.level_zero();
        let prop = b.expr_sort(zero); // Sort 0
        b.add_definition(foo, uparams, type0, prop, ReducibilityHint::Regular(0));

        // Must not panic: the kernel accepts the declaration.
        b.finish().check_all_declars();
    }

    /// An ill-typed definition (`def Bad : Prop := Type 0`, i.e. claiming the
    /// universe `Sort 0` has type `Sort 0`) must be rejected by the kernel.
    #[test]
    #[should_panic]
    fn builder_rejects_ill_typed_definition() {
        let mut b = Builder::new(Config::default());

        let anon = b.name_anon();
        let bad = b.name_str(anon, "Bad");
        let uparams = b.levels(&[]);
        let zero = b.level_zero();
        let prop = b.expr_sort(zero); // claimed type: Sort 0
        let one = b.level_succ(b.level_zero());
        let type0 = b.expr_sort(one); // value: Sort 1, whose type is Sort 2 != Sort 0
        b.add_definition(bad, uparams, prop, type0, ReducibilityHint::Regular(0));

        b.finish().check_all_declars();
    }
}
