use crate::builder::Builder;
use crate::util::{Config, CowStr, ExportFile, ExprPtr, LevelPtr, TcCtx};
use rand::distributions::Alphanumeric;
use rand::{rngs::ThreadRng, Rng};
use std::error::Error;
use std::path::Path;

/// The config the test harness checks against. Equivalent to the old
/// `test_resources/Empty/export` run: an empty environment with the kernel
/// literal extensions disabled.
fn test_config() -> Config {
    Config {
        nat_extension: false,
        string_extension: false,
        print_success_message: true,
        print_axioms: true,
        ..Config::default()
    }
}

/// An empty environment, built via [`Builder`] instead of parsing an export
/// file. Equivalent to what the parser produced from an empty export file.
pub(crate) fn empty_export_file<'p>() -> ExportFile<'p> { Builder::new(test_config()).finish() }

/// `_path` is retained for source compatibility with existing call sites (which
/// all pass `None`); only the empty environment is supported now that the
/// export-file parser has been removed.
pub(crate) fn test_export_file<A>(
    _path: Option<&Path>,
    f: impl FnOnce(&ExportFile) -> A,
) -> Result<A, Box<dyn Error>> {
    Ok(f(&empty_export_file()))
}

pub(crate) fn test_ctx<'p, A>(_path: Option<&Path>, f: impl FnOnce(&mut TcCtx) -> A) -> Result<A, Box<dyn Error>> {
    test_export_file(None, |export_file| export_file.with_ctx(|ctx, _arena| f(ctx)))
}

impl<'t, 'p: 't> TcCtx<'t, 'p> {
    #[cfg(test)]
    pub(crate) fn level_n(&mut self, mut l: LevelPtr<'t>, n: u64) -> LevelPtr<'t> {
        for _ in 0..n {
            l = self.succ(l);
        }
        l
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn mk_succ_app(&mut self, n: usize) -> ExprPtr<'t> {
        let mut out = self.c_nat_zero().unwrap();
        let succ = self.c_nat_succ().unwrap();
        for _ in 0..n {
            out = self.mk_app(succ, out);
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn param_quick(&mut self, s: &'static str) -> LevelPtr<'t> {
        let n = self.str1(&s);
        self.param(n)
    }
}

#[test]
fn check_empty() -> Result<(), Box<dyn Error>> {
    test_export_file(None, |export| {
        for declar in export.declars.values() {
            export.check_declar(declar);
        }
    })
}

// `check_proj_from_prop` was removed with the export-file parser: it loaded a
// real export fixture (`test_resources/ProjFromProp`) to assert the kernel
// rejects projecting out of a `Prop`. It should be reinstated as a `Builder`-
// constructed fixture (or driven through the outer FFI crate) in a later pass.

pub(crate) fn rand_string<'t>(rng: &mut ThreadRng, size: usize) -> CowStr<'t> {
    let rand_string: String = rng.sample_iter(&Alphanumeric).take(size).map(char::from).collect();
    CowStr::Owned(rand_string)
}

#[test]
fn hash_test0() -> Result<(), Box<dyn Error>> {
    use crate::hash64;
    use num_bigint::RandBigInt;
    use rand::thread_rng;
    test_export_file(None, |export| {
        let mut rng = thread_rng();
        export.with_ctx(|ctx, _arena| {
            for size in 0..100 {
                for _ in 0..100 {
                    let s = rand_string(&mut rng, size);
                    let (l, r) = (ctx.mk_string_lit_quick(s.clone()), ctx.mk_string_lit_quick(s));
                    assert_eq!(hash64!(l), hash64!(r));
                    assert_eq!(l, r)
                }
                for _ in 0..100 {
                    let s = rng.gen_biguint(size as u64);
                    let (l, r) = (ctx.mk_nat_lit_quick(s.clone()), ctx.mk_nat_lit_quick(s));
                    assert_eq!(hash64!(l), hash64!(r));
                    assert_eq!(l, r)
                }
            }
        })
    })
}
