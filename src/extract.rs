//! Walk Lean `lean_object`s for the imported environment and rebuild them as a
//! checkable sokonanoda environment, then run the kernel.
//!
//! The expression / level / name trees are read **directly** from their
//! `lean_object` representation via `lean-sys` (constructor tags + fields, with
//! the layout pinned down empirically — see the offsets used in
//! [`Walker::walk_expr`]). The scalar `ConstantInfo` metadata and the
//! dependency edges come from the thin `@[export]` accessors in `glue/Glue.lean`
//! (and the hand-written `glue/nat_bytes.c` for big-`Nat` literals).
//!
//! ## Memory
//!
//! Lean refcounts are managed with [`LeanObj`] (RAII: `Drop` decrements, `Clone`
//! increments), so nothing is leaked and the environment can be extracted and
//! checked repeatedly within one process. The convention here is that the
//! `@[export]` accessors *consume* their argument, so the wrappers below clone
//! (increment) before each call; constructor fields read via [`LeanObj::child`]
//! acquire their own reference.
//!
//! ## Ordering
//!
//! sokonanoda checks each declaration against the environment of everything
//! declared *before* it, so declarations must be submitted dependency-first.
//! Mutually-recursive inductive blocks (the inductive types, their
//! constructors, and their recursors) form one unit submitted together. We
//! build a graph over such units and emit them in post-order DFS.

use std::collections::HashMap;

use lean_sys::{
    lean_array_get_core, lean_array_size, lean_inc, lean_object, lean_sarray_cptr, lean_sarray_size,
    lean_string_cstr, lean_string_size,
};
use num_bigint::BigUint;
use sokonanoda::builder::{Builder, ConstructorInput, InductiveInput, RecRuleInput, RecursorInput};
use sokonanoda::env::ReducibilityHint;
use sokonanoda::expr::BinderStyle;
use sokonanoda::util::{Config, ExprPtr, LevelPtr, LevelsPtr, NamePtr};

use crate::ffi::LeanObj;

type Obj = *mut lean_object;

// Glue accessors (see glue/Glue.lean). All consume their object argument.
unsafe extern "C" {
    fn lc_env_all_consts(env: Obj) -> Obj;
    fn lc_ci_deps(ci: Obj) -> Obj;
    fn lc_ci_kind(ci: Obj) -> u8;
    fn lc_ci_name(ci: Obj) -> Obj;
    fn lc_name_to_string(n: Obj) -> Obj;
    fn lc_ci_level_params(ci: Obj) -> Obj;
    fn lc_ci_type(ci: Obj) -> Obj;
    fn lc_ci_value(ci: Obj) -> Obj;
    fn lc_ci_def_hint_kind(ci: Obj) -> u8;
    fn lc_ci_def_hint_height(ci: Obj) -> u32;
    fn lc_ci_ind_num_params(ci: Obj) -> u32;
    fn lc_ci_ind_num_indices(ci: Obj) -> u32;
    fn lc_ci_ind_is_rec(ci: Obj) -> u8;
    fn lc_ci_ind_num_nested(ci: Obj) -> u32;
    fn lc_ci_ind_all(ci: Obj) -> Obj;
    fn lc_ci_ind_ctors(ci: Obj) -> Obj;
    fn lc_ci_ctor_induct(ci: Obj) -> Obj;
    fn lc_ci_ctor_cidx(ci: Obj) -> u32;
    fn lc_ci_ctor_num_params(ci: Obj) -> u32;
    fn lc_ci_ctor_num_fields(ci: Obj) -> u32;
    fn lc_ci_rec_num_params(ci: Obj) -> u32;
    fn lc_ci_rec_num_indices(ci: Obj) -> u32;
    fn lc_ci_rec_num_motives(ci: Obj) -> u32;
    fn lc_ci_rec_num_minors(ci: Obj) -> u32;
    fn lc_ci_rec_k(ci: Obj) -> u8;
    fn lc_ci_rec_all(ci: Obj) -> Obj;
    fn lc_ci_rec_num_rules(ci: Obj) -> u32;
    fn lc_ci_rec_rule_ctor(ci: Obj, i: u32) -> Obj;
    fn lc_ci_rec_rule_nfields(ci: Obj, i: u32) -> u32;
    fn lc_ci_rec_rule_rhs(ci: Obj, i: u32) -> Obj;
    // Hand-written C (glue/nat_bytes.c): borrows its argument.
    fn lc_nat_to_le_bytes(n: Obj) -> Obj;
}

// --- accessor wrappers (clone-before-consume) -----------------------------

unsafe fn acc_obj(ci: &LeanObj, f: unsafe extern "C" fn(Obj) -> Obj) -> LeanObj {
    unsafe {
        lean_inc(ci.as_ptr());
        LeanObj::from_owned(f(ci.as_ptr()))
    }
}
unsafe fn acc_u8(ci: &LeanObj, f: unsafe extern "C" fn(Obj) -> u8) -> u8 {
    unsafe {
        lean_inc(ci.as_ptr());
        f(ci.as_ptr())
    }
}
unsafe fn acc_u32(ci: &LeanObj, f: unsafe extern "C" fn(Obj) -> u32) -> u32 {
    unsafe {
        lean_inc(ci.as_ptr());
        f(ci.as_ptr())
    }
}
unsafe fn acc_rule_obj(ci: &LeanObj, i: u32, f: unsafe extern "C" fn(Obj, u32) -> Obj) -> LeanObj {
    unsafe {
        lean_inc(ci.as_ptr());
        LeanObj::from_owned(f(ci.as_ptr(), i))
    }
}
unsafe fn acc_rule_u32(ci: &LeanObj, i: u32, f: unsafe extern "C" fn(Obj, u32) -> u32) -> u32 {
    unsafe {
        lean_inc(ci.as_ptr());
        f(ci.as_ptr(), i)
    }
}

// --- low-level readers (arguments borrowed) -------------------------------

/// Copy a borrowed Lean `String` into an owned Rust `String`.
unsafe fn read_string(s: &LeanObj) -> String {
    unsafe {
        let ptr = lean_string_cstr(s.as_ptr());
        let len = lean_string_size(s.as_ptr()).saturating_sub(1);
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
    }
}

/// The dotted string form of a Lean `Name` object.
unsafe fn name_string(n: &LeanObj) -> String { unsafe { read_string(&acc_obj(n, lc_name_to_string)) } }

/// Elements of a Lean `Array`, each as a fresh owned reference.
unsafe fn array_elems(arr: &LeanObj) -> Vec<LeanObj> {
    unsafe {
        (0..lean_array_size(arr.as_ptr()))
            .map(|i| LeanObj::from_borrowed(lean_array_get_core(arr.as_ptr(), i)))
            .collect()
    }
}

/// Names of an `Array Name`.
unsafe fn name_array(arr: &LeanObj) -> Vec<String> {
    unsafe { array_elems(arr).iter().map(|n| name_string(n)).collect() }
}

/// A `Nat` as an arbitrary-precision integer, without decimal stringification:
/// small values unbox directly, big values go through GMP byte export.
unsafe fn nat_to_biguint(o: &LeanObj) -> BigUint {
    unsafe {
        if o.is_scalar() {
            BigUint::from(o.unbox() as u64)
        } else {
            let bytes = LeanObj::from_owned(lc_nat_to_le_bytes(o.as_ptr())); // borrowed arg
            let n = lean_sarray_size(bytes.as_ptr());
            let data = lean_sarray_cptr(bytes.as_ptr());
            BigUint::from_bytes_le(std::slice::from_raw_parts(data, n))
        }
    }
}

unsafe fn nat_to_u64(o: &LeanObj) -> u64 {
    unsafe {
        if o.is_scalar() {
            o.unbox() as u64
        } else {
            u64::try_from(nat_to_biguint(o)).unwrap_or(u64::MAX)
        }
    }
}

// --- tree walkers ----------------------------------------------------------

/// Walks `lean_object` trees into a sokonanoda [`Builder`], memoizing shared
/// expression nodes (Lean hash-conses, so the same pointer recurs often).
struct Walker<'b, 'p> {
    b: &'b mut Builder<'p>,
    expr_memo: HashMap<usize, ExprPtr<'p>>,
}

impl<'b, 'p> Walker<'b, 'p> {
    fn new(b: &'b mut Builder<'p>) -> Self { Walker { b, expr_memo: HashMap::new() } }

    /// `Name`: anonymous = scalar; str = tag 1 (pre, String); num = tag 2 (pre, Nat).
    unsafe fn walk_name(&mut self, o: &LeanObj) -> NamePtr<'p> {
        unsafe {
            if o.is_scalar() {
                return self.b.name_anon();
            }
            match o.tag() {
                1 => {
                    let pre = self.walk_name(&o.child(0));
                    let s = read_string(&o.child(1));
                    self.b.name_str(pre, &s)
                }
                2 => {
                    let pre = self.walk_name(&o.child(0));
                    let n = nat_to_u64(&o.child(1));
                    self.b.name_num(pre, n)
                }
                t => panic!("unexpected Name tag {t}"),
            }
        }
    }

    /// `Level`: zero = scalar; succ=1, max=2, imax=3, param=4, mvar=5.
    unsafe fn walk_level(&mut self, o: &LeanObj) -> LevelPtr<'p> {
        unsafe {
            if o.is_scalar() {
                return self.b.level_zero();
            }
            match o.tag() {
                1 => {
                    let l = self.walk_level(&o.child(0));
                    self.b.level_succ(l)
                }
                2 => {
                    let l = self.walk_level(&o.child(0));
                    let r = self.walk_level(&o.child(1));
                    self.b.level_max(l, r)
                }
                3 => {
                    let l = self.walk_level(&o.child(0));
                    let r = self.walk_level(&o.child(1));
                    self.b.level_imax(l, r)
                }
                4 => {
                    let n = self.walk_name(&o.child(0));
                    self.b.level_param(n)
                }
                t => panic!("unexpected Level tag {t} (uninstantiated universe metavariable?)"),
            }
        }
    }

    /// A `List Level` into a vector of level pointers.
    unsafe fn walk_level_list(&mut self, o: &LeanObj) -> Vec<LevelPtr<'p>> {
        unsafe {
            let mut out = Vec::new();
            let mut cur = o.clone();
            while !cur.is_scalar() {
                out.push(self.walk_level(&cur.child(0)));
                cur = cur.child(1);
            }
            out
        }
    }

    /// `Expr`: tags bvar=0, fvar=1, mvar=2, sort=3, const=4, app=5, lam=6,
    /// forallE=7, letE=8, lit=9, mdata=10, proj=11. The cached `Expr.Data`
    /// (`UInt64`) is the first scalar field, so a ctor's `binderInfo`/`nondep`
    /// byte sits at `num_objs*8 + 8`.
    unsafe fn walk_expr(&mut self, o: &LeanObj) -> ExprPtr<'p> {
        unsafe {
            let key = o.as_ptr() as usize;
            if let Some(&e) = self.expr_memo.get(&key) {
                return e;
            }
            let e = self.walk_expr_uncached(o);
            self.expr_memo.insert(key, e);
            e
        }
    }

    unsafe fn walk_expr_uncached(&mut self, o: &LeanObj) -> ExprPtr<'p> {
        unsafe {
            match o.tag() {
                0 => {
                    let idx = nat_to_u64(&o.child(0)) as u16;
                    self.b.expr_var(idx)
                }
                3 => {
                    let l = self.walk_level(&o.child(0));
                    self.b.expr_sort(l)
                }
                4 => {
                    let name = self.walk_name(&o.child(0));
                    let us = self.walk_level_list(&o.child(1));
                    let levels = self.b.levels(&us);
                    self.b.expr_const(name, levels)
                }
                5 => {
                    let f = self.walk_expr(&o.child(0));
                    let a = self.walk_expr(&o.child(1));
                    self.b.expr_app(f, a)
                }
                6 => {
                    let name = self.walk_name(&o.child(0));
                    let ty = self.walk_expr(&o.child(1));
                    let body = self.walk_expr(&o.child(2));
                    let style = binder_style(self.scalar_u8(o));
                    self.b.expr_lambda(name, style, ty, body)
                }
                7 => {
                    let name = self.walk_name(&o.child(0));
                    let ty = self.walk_expr(&o.child(1));
                    let body = self.walk_expr(&o.child(2));
                    let style = binder_style(self.scalar_u8(o));
                    self.b.expr_pi(name, style, ty, body)
                }
                8 => {
                    let name = self.walk_name(&o.child(0));
                    let ty = self.walk_expr(&o.child(1));
                    let val = self.walk_expr(&o.child(2));
                    let body = self.walk_expr(&o.child(3));
                    let nondep = self.scalar_u8(o) != 0;
                    self.b.expr_let(name, ty, val, body, nondep)
                }
                9 => self.walk_literal(&o.child(0)),
                10 => self.walk_expr(&o.child(1)), // mdata: transparent
                11 => {
                    let ty_name = self.walk_name(&o.child(0));
                    let idx = nat_to_u64(&o.child(1)) as usize;
                    let structure = self.walk_expr(&o.child(2));
                    self.b.expr_proj(ty_name, idx, structure)
                }
                1 => panic!("free variable in exported term"),
                2 => panic!("metavariable in exported term"),
                t => panic!("unexpected Expr tag {t}"),
            }
        }
    }

    /// `Literal`: natVal = tag 0 (Nat), strVal = tag 1 (String).
    unsafe fn walk_literal(&mut self, lit: &LeanObj) -> ExprPtr<'p> {
        unsafe {
            match lit.tag() {
                0 => {
                    let n = nat_to_biguint(&lit.child(0));
                    self.b.expr_nat_lit(n)
                }
                1 => {
                    let s = read_string(&lit.child(0));
                    self.b.expr_string_lit(&s)
                }
                t => panic!("unexpected Literal tag {t}"),
            }
        }
    }

    /// The first scalar byte after the object fields (`binderInfo` / `nondep`),
    /// past the cached `Expr.Data` `UInt64`.
    unsafe fn scalar_u8(&self, o: &LeanObj) -> u8 { unsafe { o.ctor_u8(o.num_objs() * 8 + 8) } }

    /// A declaration's universe parameters, as a `LevelsPtr` of `param` levels.
    unsafe fn uparams(&mut self, ci: &LeanObj) -> LevelsPtr<'p> {
        unsafe {
            let elems = array_elems(&acc_obj(ci, lc_ci_level_params));
            let mut levels = Vec::with_capacity(elems.len());
            for n in &elems {
                let np = self.walk_name(n);
                levels.push(self.b.level_param(np));
            }
            self.b.levels(&levels)
        }
    }

    unsafe fn walk_name_array(&mut self, arr: &LeanObj) -> Vec<NamePtr<'p>> {
        unsafe { array_elems(arr).iter().map(|n| self.walk_name(n)).collect() }
    }
}

fn binder_style(b: u8) -> BinderStyle {
    match b {
        0 => BinderStyle::Default,
        1 => BinderStyle::Implicit,
        2 => BinderStyle::StrictImplicit,
        3 => BinderStyle::InstanceImplicit,
        other => panic!("unexpected binderInfo {other}"),
    }
}

// --- declaration units + dependency ordering -------------------------------

const KIND_AXIOM: u8 = 0;
const KIND_DEF: u8 = 1;
const KIND_THM: u8 = 2;
const KIND_OPAQUE: u8 = 3;
const KIND_QUOT: u8 = 4;
const KIND_INDUCT: u8 = 5;
const KIND_CTOR: u8 = 6;
const KIND_REC: u8 = 7;

/// Lightweight per-constant metadata gathered before any expression walking.
/// Owns its `ConstantInfo` reference for the duration of the pass.
struct ConstMeta {
    obj: LeanObj,
    name: String,
    kind: u8,
    deps: Vec<String>,
    unit: usize,
}

/// One declaration unit: either a singleton constant or a whole mutual
/// inductive block. Holds indices into the `ConstMeta` slice.
struct Unit {
    members: Vec<usize>,
    is_block: bool,
}

/// Drive the whole pipeline: extract `env`'s constants, order them, build a
/// sokonanoda environment, and check it. `selected` (if non-empty) restricts to
/// those constants and their transitive dependencies. Returns the number of
/// declarations checked.
pub fn check_environment(env: Obj, selected: &[String], num_threads: usize) -> Result<usize, String> {
    let config = Config { num_threads, ..Config::default() };
    let mut builder = Builder::new(config);

    let count: usize = unsafe {
        let mut metas = gather_consts(env);
        let units = assign_units(&mut metas);
        let order = topo_order(&metas, &units, selected);
        emit_units(&mut builder, &metas, &units, &order);
        order.iter().map(|&u| units[u].members.len()).sum()
    };

    let export = builder.finish();
    // `check_all_declars` panics (assert / unwrap) on a kernel rejection; turn
    // that into an error rather than unwinding out of the FFI boundary.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| export.check_all_declars()))
        .map(|()| count)
        .map_err(|e| {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "kernel rejected a declaration".to_string());
            format!("kernel check failed: {msg}")
        })
}

unsafe fn gather_consts(env: Obj) -> Vec<ConstMeta> {
    unsafe {
        lean_inc(env);
        let consts = LeanObj::from_owned(lc_env_all_consts(env));
        array_elems(&consts)
            .into_iter()
            .map(|ci| {
                let name = name_string(&acc_obj(&ci, lc_ci_name));
                let kind = acc_u8(&ci, lc_ci_kind);
                let deps = name_array(&acc_obj(&ci, lc_ci_deps));
                ConstMeta { obj: ci, name, kind, deps, unit: usize::MAX }
            })
            .collect()
    }
}

/// Group constants into units (mutual inductive blocks share a unit) and record
/// each constant's unit index.
unsafe fn assign_units(metas: &mut [ConstMeta]) -> Vec<Unit> {
    unsafe {
        let name_to_idx: HashMap<&str, usize> =
            metas.iter().enumerate().map(|(i, m)| (m.name.as_str(), i)).collect();

        // Block key for each inductive/ctor/recursor; `None` for singletons.
        let block_key: Vec<Option<String>> = metas
            .iter()
            .map(|m| match m.kind {
                KIND_INDUCT => Some(block_key_from_all(name_array(&acc_obj(&m.obj, lc_ci_ind_all)))),
                KIND_REC => Some(block_key_from_all(name_array(&acc_obj(&m.obj, lc_ci_rec_all)))),
                KIND_CTOR => {
                    let induct = name_string(&acc_obj(&m.obj, lc_ci_ctor_induct));
                    name_to_idx
                        .get(induct.as_str())
                        .map(|&j| block_key_from_all(name_array(&acc_obj(&metas[j].obj, lc_ci_ind_all))))
                        .or(Some(induct))
                }
                _ => None,
            })
            .collect();

        let mut units: Vec<Unit> = Vec::new();
        let mut key_to_unit: HashMap<String, usize> = HashMap::new();

        for i in 0..metas.len() {
            let unit = match &block_key[i] {
                Some(key) => *key_to_unit.entry(key.clone()).or_insert_with(|| {
                    units.push(Unit { members: Vec::new(), is_block: true });
                    units.len() - 1
                }),
                None => {
                    units.push(Unit { members: Vec::new(), is_block: false });
                    units.len() - 1
                }
            };
            units[unit].members.push(i);
            metas[i].unit = unit;
        }
        units
    }
}

fn block_key_from_all(mut all: Vec<String>) -> String {
    all.sort();
    all.join("\u{1}")
}

/// Post-order DFS over the unit dependency graph, yielding units in an order
/// where every dependency precedes its dependents. Cycles between units (should
/// not arise once mutual blocks are grouped) are broken by skipping back-edges.
fn topo_order(metas: &[ConstMeta], units: &[Unit], selected: &[String]) -> Vec<usize> {
    let name_to_idx: HashMap<&str, usize> =
        metas.iter().enumerate().map(|(i, m)| (m.name.as_str(), i)).collect();

    // Dependency edges between units.
    let mut unit_deps: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
    for (u, unit) in units.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        for &mi in &unit.members {
            for dep in &metas[mi].deps {
                if let Some(&di) = name_to_idx.get(dep.as_str()) {
                    let du = metas[di].unit;
                    if du != u && seen.insert(du) {
                        unit_deps[u].push(du);
                    }
                }
            }
        }
    }

    // Roots: units of the selected constants, or all units.
    let roots: Vec<usize> = if selected.is_empty() {
        (0..units.len()).collect()
    } else {
        selected
            .iter()
            .filter_map(|s| name_to_idx.get(s.as_str()).map(|&i| metas[i].unit))
            .collect()
    };

    let mut order = Vec::new();
    let mut state = vec![0u8; units.len()]; // 0 unvisited, 1 on-stack, 2 done
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (unit, next child index)
    for &root in &roots {
        if state[root] != 0 {
            continue;
        }
        stack.push((root, 0));
        state[root] = 1;
        while let Some(&(u, ci)) = stack.last() {
            if ci < unit_deps[u].len() {
                stack.last_mut().unwrap().1 += 1;
                let v = unit_deps[u][ci];
                if state[v] == 0 {
                    state[v] = 1;
                    stack.push((v, 0));
                }
                // state[v] == 1 is a back-edge (cycle); skip.
            } else {
                state[u] = 2;
                order.push(u);
                stack.pop();
            }
        }
    }
    order
}

/// Build each unit's declarations and submit them in dependency order.
unsafe fn emit_units<'p>(builder: &mut Builder<'p>, metas: &[ConstMeta], units: &[Unit], order: &[usize]) {
    unsafe {
        let mut w = Walker::new(builder);
        for &u in order {
            let unit = &units[u];
            if unit.is_block {
                emit_block(&mut w, metas, unit);
            } else {
                emit_singleton(&mut w, &metas[unit.members[0]]);
            }
        }
    }
}

unsafe fn emit_singleton<'p>(w: &mut Walker<'_, 'p>, m: &ConstMeta) {
    unsafe {
        let ci = &m.obj;
        let name = w.walk_name(&acc_obj(ci, lc_ci_name));
        let uparams = w.uparams(ci);
        let ty = w.walk_expr(&acc_obj(ci, lc_ci_type));
        match m.kind {
            KIND_AXIOM => w.b.add_axiom(name, uparams, ty),
            KIND_QUOT => w.b.add_quot(name, uparams, ty),
            KIND_THM => {
                let val = w.walk_expr(&value_of(ci));
                w.b.add_theorem(name, uparams, ty, val);
            }
            KIND_OPAQUE => {
                let val = w.walk_expr(&value_of(ci));
                w.b.add_opaque(name, uparams, ty, val);
            }
            KIND_DEF => {
                let val = w.walk_expr(&value_of(ci));
                let hint = def_hint(ci);
                w.b.add_definition(name, uparams, ty, val, hint);
            }
            other => panic!("constant {} has unexpected singleton kind {other}", m.name),
        }
    }
}

unsafe fn emit_block<'p>(w: &mut Walker<'_, 'p>, metas: &[ConstMeta], unit: &Unit) {
    unsafe {
        let mut inds = Vec::new();
        let mut ctors = Vec::new();
        let mut recs = Vec::new();

        for &mi in &unit.members {
            let m = &metas[mi];
            let ci = &m.obj;
            let name = w.walk_name(&acc_obj(ci, lc_ci_name));
            let uparams = w.uparams(ci);
            let ty = w.walk_expr(&acc_obj(ci, lc_ci_type));
            match m.kind {
                KIND_INDUCT => {
                    let all_ind_names = w.walk_name_array(&acc_obj(ci, lc_ci_ind_all));
                    let all_ctor_names = w.walk_name_array(&acc_obj(ci, lc_ci_ind_ctors));
                    inds.push(InductiveInput {
                        name,
                        uparams,
                        ty,
                        is_recursive: acc_u8(ci, lc_ci_ind_is_rec) != 0,
                        is_nested: acc_u32(ci, lc_ci_ind_num_nested) > 0,
                        num_params: acc_u32(ci, lc_ci_ind_num_params) as u16,
                        num_indices: acc_u32(ci, lc_ci_ind_num_indices) as u16,
                        all_ind_names,
                        all_ctor_names,
                    });
                }
                KIND_CTOR => {
                    let inductive_name = w.walk_name(&acc_obj(ci, lc_ci_ctor_induct));
                    ctors.push(ConstructorInput {
                        name,
                        uparams,
                        ty,
                        inductive_name,
                        ctor_idx: acc_u32(ci, lc_ci_ctor_cidx) as u16,
                        num_params: acc_u32(ci, lc_ci_ctor_num_params) as u16,
                        num_fields: acc_u32(ci, lc_ci_ctor_num_fields) as u16,
                    });
                }
                KIND_REC => {
                    let all_inductives = w.walk_name_array(&acc_obj(ci, lc_ci_rec_all));
                    let num_rules = acc_u32(ci, lc_ci_rec_num_rules);
                    let mut rec_rules = Vec::with_capacity(num_rules as usize);
                    for i in 0..num_rules {
                        let ctor_name = w.walk_name(&acc_rule_obj(ci, i, lc_ci_rec_rule_ctor));
                        let num_fields = acc_rule_u32(ci, i, lc_ci_rec_rule_nfields) as u16;
                        let rhs = w.walk_expr(&acc_rule_obj(ci, i, lc_ci_rec_rule_rhs));
                        rec_rules.push(RecRuleInput { ctor_name, num_fields, rhs });
                    }
                    recs.push(RecursorInput {
                        name,
                        uparams,
                        ty,
                        all_inductives,
                        num_params: acc_u32(ci, lc_ci_rec_num_params) as u16,
                        num_indices: acc_u32(ci, lc_ci_rec_num_indices) as u16,
                        num_motives: acc_u32(ci, lc_ci_rec_num_motives) as u16,
                        num_minors: acc_u32(ci, lc_ci_rec_num_minors) as u16,
                        rec_rules,
                        is_k: acc_u8(ci, lc_ci_rec_k) != 0,
                    });
                }
                other => panic!("constant {} has unexpected block kind {other}", m.name),
            }
        }
        w.b.add_inductive_block(inds, ctors, recs);
    }
}

/// The value of a def / theorem / opaque, as an owned `Expr`.
unsafe fn value_of(ci: &LeanObj) -> LeanObj {
    unsafe {
        let opt = acc_obj(ci, lc_ci_value); // Option Expr
        assert!(!opt.is_scalar(), "expected a value");
        opt.child(0)
    }
}

unsafe fn def_hint(ci: &LeanObj) -> ReducibilityHint {
    unsafe {
        match acc_u8(ci, lc_ci_def_hint_kind) {
            0 => ReducibilityHint::Opaque,
            1 => ReducibilityHint::Abbrev,
            _ => ReducibilityHint::Regular(acc_u32(ci, lc_ci_def_hint_height) as u16),
        }
    }
}
