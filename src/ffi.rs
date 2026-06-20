//! Safe-ish Rust wrapper around the Lean runtime and the `glue/Glue.lean`
//! exports.
//!
//! All of the `unsafe` Lean object juggling is confined here; the rest of the
//! program works with plain Rust `String`s and `Vec`s.
//!
//! ## Ownership notes
//!
//! Lean uses reference counting with a "consume the argument" calling
//! convention: a function consumes (decrements) the reference count of each of
//! its object arguments unless that argument is *borrowed*.  Our glue accessors
//! (`lc_env_*`) take the `Environment` by value, so they consume one reference
//! each.  To call several accessors on the same environment we therefore
//! [`lean_inc`] the handle before every call and [`lean_dec`] it once at the
//! end (see [`Environment::drop`]).

use std::ffi::CString;

use lean_sys::{
    lean_array_push, lean_ctor_get, lean_ctor_get_uint8, lean_ctor_num_objs, lean_dec, lean_inc,
    lean_init_task_manager, lean_initialize, lean_initialize_runtime_module, lean_io_error_to_string,
    lean_io_mark_end_initialization, lean_io_result_get_error, lean_io_result_is_ok,
    lean_io_result_take_value, lean_is_scalar, lean_mk_empty_array, lean_mk_string, lean_object,
    lean_ptr_tag, lean_string_cstr, lean_string_size, lean_unbox,
};

/// An owned reference to a Lean heap object: `Drop` decrements its refcount and
/// `Clone` increments it. This lets the FFI extraction layer manage Lean
/// refcounts via RAII (rather than leaking), so the process can check repeatedly
/// — e.g. as a long-lived language server — without unbounded growth.
pub struct LeanObj(*mut lean_object);

impl LeanObj {
    /// Take ownership of an already-owned reference (no refcount change).
    ///
    /// # Safety
    /// `o` must carry a reference that this `LeanObj` is now responsible for.
    pub unsafe fn from_owned(o: *mut lean_object) -> Self { LeanObj(o) }

    /// Acquire a fresh reference to a borrowed object (increments).
    ///
    /// # Safety
    /// `o` must be a live Lean object.
    pub unsafe fn from_borrowed(o: *mut lean_object) -> Self {
        unsafe { lean_inc(o) };
        LeanObj(o)
    }

    pub fn as_ptr(&self) -> *mut lean_object { self.0 }

    /// Whether this is an unboxed scalar (e.g. `Nat.zero`, `Name.anonymous`).
    pub fn is_scalar(&self) -> bool { lean_is_scalar(self.0) }

    /// # Safety: `self` must be a constructor object.
    pub unsafe fn tag(&self) -> u8 { unsafe { lean_ptr_tag(self.0) } }

    /// # Safety: `self` must be a constructor object.
    pub unsafe fn num_objs(&self) -> u32 { unsafe { lean_ctor_num_objs(self.0) } }

    /// # Safety: `self` must be an unboxed scalar.
    pub unsafe fn unbox(&self) -> usize { lean_unbox(self.0) }

    /// The `i`-th object field, as a new owned reference.
    ///
    /// # Safety: `self` must be a constructor with more than `i` object fields.
    pub unsafe fn child(&self, i: u32) -> LeanObj { unsafe { LeanObj::from_borrowed(lean_ctor_get(self.0, i)) } }

    /// A scalar byte at `offset` (bytes) past the object fields.
    ///
    /// # Safety: `offset` must lie within `self`'s scalar region.
    pub unsafe fn ctor_u8(&self, offset: u32) -> u8 { unsafe { lean_ctor_get_uint8(self.0, offset) } }
}

impl Clone for LeanObj {
    fn clone(&self) -> Self {
        unsafe { lean_inc(self.0) };
        LeanObj(self.0)
    }
}

impl Drop for LeanObj {
    fn drop(&mut self) {
        unsafe { lean_dec(self.0) };
    }
}

// Functions exported from `glue/Glue.lean`, plus the module initializer that
// Lean generates for it (which transitively initializes `Init` and `Lean`).
unsafe extern "C" {
    fn initialize_Glue(builtin: u8) -> *mut lean_object;
    fn lc_init_search_path(extra: *mut lean_object) -> *mut lean_object;
    fn lc_import_modules(mods: *mut lean_object) -> *mut lean_object;
}

/// Witness that the Lean runtime has been initialized. Construct exactly once,
/// at program start, via [`Runtime::init`].
pub struct Runtime {
    _private: (),
}

impl Runtime {
    /// Initialize the Lean runtime and our glue module.
    ///
    /// Mirrors the boilerplate that `leanc` emits in a generated `main`:
    /// bring up the runtime, run the module initializers, start the task
    /// manager (module import uses tasks), and mark initialization complete.
    pub fn init() -> Result<Self, String> {
        unsafe {
            lean_initialize_runtime_module();
            lean_initialize();

            let res = initialize_Glue(/* builtin */ 1);
            check_io_unit(res).map_err(|e| format!("initialize_Glue failed: {e}"))?;

            lean_init_task_manager();
            lean_io_mark_end_initialization();
        }
        Ok(Runtime { _private: () })
    }

    /// `initSearchPath (← findSysroot) extra` — set up module resolution.
    pub fn init_search_path(&self, extra: &[String]) -> Result<(), String> {
        unsafe {
            let arr = mk_string_array(extra);
            let res = lc_init_search_path(arr);
            check_io_unit(res).map_err(|e| format!("initSearchPath failed: {e}"))
        }
    }

    /// `importModules mods {}` — import the named modules and return the
    /// resulting environment.
    pub fn import_modules(&self, mods: &[String]) -> Result<Environment, String> {
        unsafe {
            let arr = mk_string_array(mods);
            let res = lc_import_modules(arr);
            if lean_io_result_is_ok(res) {
                let env = lean_io_result_take_value(res);
                Ok(Environment { handle: env })
            } else {
                Err(format!("importModules failed: {}", take_io_error(res)))
            }
        }
    }
}

/// An opaque, reference-counted handle to a Lean `Environment`.
pub struct Environment {
    handle: *mut lean_object,
}

impl Environment {
    /// The raw Lean `Environment` object, for the FFI extraction layer to walk.
    pub fn raw(&self) -> *mut lean_object { self.handle }
}

impl Drop for Environment {
    fn drop(&mut self) {
        unsafe { lean_dec(self.handle) };
    }
}

// --- low-level marshalling helpers -----------------------------------------

/// Build a Lean `Array String` from a Rust slice.
unsafe fn mk_string_array(items: &[String]) -> *mut lean_object {
    unsafe {
        let mut arr = lean_mk_empty_array();
        for s in items {
            let c = CString::new(s.as_bytes()).expect("argument contained an interior NUL byte");
            let ls = lean_mk_string(c.as_ptr() as *const u8);
            arr = lean_array_push(arr, ls);
        }
        arr
    }
}

/// Copy a (borrowed) Lean `String` into an owned Rust `String`.
unsafe fn read_lean_string(s: *mut lean_object) -> String {
    unsafe {
        let ptr = lean_string_cstr(s);
        // `lean_string_size` counts the trailing NUL; the UTF-8 payload is one
        // byte shorter.
        let len = lean_string_size(s).saturating_sub(1);
        let bytes = std::slice::from_raw_parts(ptr, len);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Check an `IO Unit` result, consuming it. Returns the error message on error.
unsafe fn check_io_unit(res: *mut lean_object) -> Result<(), String> {
    unsafe {
        if lean_io_result_is_ok(res) {
            lean_dec(res);
            Ok(())
        } else {
            Err(take_io_error(res))
        }
    }
}

/// Extract the human-readable message from an errored `IO` result, consuming it.
unsafe fn take_io_error(res: *mut lean_object) -> String {
    unsafe {
        let err = lean_io_result_get_error(res); // borrowed
        lean_inc(err);
        let s = lean_io_error_to_string(err); // consumes the ref we just added
        let msg = read_lean_string(s);
        lean_dec(s);
        lean_dec(res);
        msg
    }
}
