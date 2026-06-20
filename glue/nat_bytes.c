/*
 * Hand-written C companion to `glue/Glue.lean`.
 *
 * Converts a Lean `Nat` to its little-endian byte representation (a Lean
 * `ByteArray`), so the Rust side can rebuild an arbitrary-precision integer with
 * `BigUint::from_bytes_le` in O(n) — avoiding O(n^2) decimal stringification for
 * large literals.
 *
 * Lean's big `Nat`s are GMP `mpz` values; we read them with the runtime's
 * `lean_extract_mpz_value` and serialise with `mpz_export`. We deliberately do
 * NOT include <gmp.h> or <lean/lean_gmp.h>: the Lean toolchain links GMP
 * statically into libleanshared and does not put gmp.h on `leanc`'s include
 * path, so instead we declare the tiny, ABI-stable slice of GMP we use. The
 * `__gmpz_*` symbols come from the GMP library named by `leanc --print-ldflags`;
 * `lean_extract_mpz_value` comes from libleanshared.
 *
 * The argument is borrowed (`b_lean_obj_arg`): we don't consume a reference.
 */
#include <lean/lean.h>
#include <stddef.h>
#include <stdint.h>

/* Minimal GMP interface (stable ABI; mirrors <gmp.h>'s `mpz_t`). */
typedef struct {
  int _mp_alloc;
  int _mp_size;
  void *_mp_d;
} __lc_mpz_struct;
typedef __lc_mpz_struct __lc_mpz_t[1];

extern void __gmpz_init(__lc_mpz_struct *);
extern void __gmpz_clear(__lc_mpz_struct *);
extern size_t __gmpz_sizeinbase(const __lc_mpz_struct *, int);
extern void *__gmpz_export(void *, size_t *, int, size_t, int, size_t, const __lc_mpz_struct *);

/* From libleanshared: set `v` to the value of the mpz object `o`. */
extern void lean_extract_mpz_value(lean_object *, __lc_mpz_struct *);

LEAN_EXPORT lean_object *lc_nat_to_le_bytes(b_lean_obj_arg n) {
  if (lean_is_scalar(n)) {
    size_t v = lean_unbox(n);
    lean_object *arr = lean_alloc_sarray(1, sizeof(size_t), sizeof(size_t));
    uint8_t *data = lean_sarray_cptr(arr);
    for (unsigned i = 0; i < sizeof(size_t); i++) {
      data[i] = (uint8_t)(v >> (8 * i));
    }
    return arr;
  }

  __lc_mpz_t v;
  __gmpz_init(v);
  lean_extract_mpz_value((lean_object *)n, v);

  size_t nbytes = (__gmpz_sizeinbase(v, 2) + 7) / 8;
  if (nbytes == 0) {
    nbytes = 1; /* value is 0 */
  }
  lean_object *arr = lean_alloc_sarray(1, nbytes, nbytes);
  uint8_t *data = lean_sarray_cptr(arr);
  for (size_t i = 0; i < nbytes; i++) {
    data[i] = 0;
  }
  size_t written = 0;
  __gmpz_export(data, &written, -1 /* least-significant word first */,
                1 /* word size */, 0 /* native endianness */, 0, v);
  __gmpz_clear(v);
  return arr;
}
