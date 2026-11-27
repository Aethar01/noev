/*
 * noev.h — C and C++ embedding API for the Noev hybrid classical–quantum language
 * (`spec/interop.md` §1, §3).
 *
 * Link against `libnoev_c.a` (and `-lm -lpthread -ldl` on Linux).
 */

#ifndef NOEV_H
#define NOEV_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** Opaque handle to a compiled Noev program. */
typedef struct NoevProgram NoevProgram;

/** Opaque handle to a Noev host runtime holding registered host functions and backend state. */
typedef struct NoevHostRuntime NoevHostRuntime;

/** Opaque callback context passed to registered C host functions for re-entrant Noev calls. */
typedef struct NoevHostContext NoevHostContext;

/** Status codes returned by every fallible Noev C API function. */
typedef enum NoevStatus {
    NOEV_OK = 0,
    NOEV_ERR_LOAD = 1,
    NOEV_ERR_COMPILE = 2,
    NOEV_ERR_MISSING_HOST_FN = 3,
    NOEV_ERR_UNKNOWN_HOST_FN = 4,
    NOEV_ERR_UNKNOWN_FN = 5,
    NOEV_ERR_SIGNATURE = 6,
    NOEV_ERR_BOUNDARY_TYPE = 7,
    NOEV_ERR_CONVERSION = 8,
    NOEV_ERR_RUNTIME = 9,
    NOEV_ERR_NULL = 10
} NoevStatus;

/** Discriminant for `NoevValue`. */
typedef enum NoevValueKind {
    NOEV_VAL_UNIT = 0,
    NOEV_VAL_INT = 1,
    NOEV_VAL_FLOAT = 2,
    NOEV_VAL_F32 = 3,
    NOEV_VAL_I8 = 4,
    NOEV_VAL_I16 = 5,
    NOEV_VAL_I32 = 6,
    NOEV_VAL_I64 = 7,
    NOEV_VAL_U8 = 8,
    NOEV_VAL_U16 = 9,
    NOEV_VAL_U32 = 10,
    NOEV_VAL_U64 = 11,
    NOEV_VAL_BOOL = 12,
    NOEV_VAL_BIT = 13,
    NOEV_VAL_STRING = 14,
    NOEV_VAL_COMPLEX = 15,
    NOEV_VAL_C32 = 16,
    /** Raw C-layout bytes for an `extern "C" struct` or `[T; N]` array. */
    NOEV_VAL_BYTES = 17
} NoevValueKind;

typedef struct NoevComplex64 {
    double re;
    double im;
} NoevComplex64;

typedef struct NoevComplex32 {
    float re;
    float im;
} NoevComplex32;

typedef struct NoevBytes {
    uint8_t *ptr;
    size_t len;
} NoevBytes;

/** Tagged classical value crossing the C embedding boundary (`spec/interop.md` §1). */
typedef struct NoevValue {
    NoevValueKind kind;
    union {
        int64_t i64;
        double f64;
        float f32;
        int8_t i8;
        int16_t i16;
        int32_t i32;
        uint8_t u8;
        uint16_t u16;
        uint32_t u32;
        uint64_t u64;
        bool b;
        uint8_t bit;
        char *str;
        NoevComplex64 c64;
        NoevComplex32 c32;
        NoevBytes bytes;
    } as;
} NoevValue;

/**
 * Signature of a C/C++ host function implementing a Noev `extern fn`.
 *
 * - `ctx` may be passed to `noev_host_ctx_call` to call back into Noev quantum/classical code.
 * - `args[0..argc]` are borrowed for the duration of the call; do not free them.
 * - Write the result to `*out` (defaults to `NOEV_VAL_UNIT`).
 * - Return `NOEV_OK` on success, or an error status code to abort the Noev call cleanly.
 */
typedef NoevStatus (*NoevHostFn)(
    NoevHostContext *ctx,
    const NoevValue *args,
    size_t argc,
    NoevValue *out,
    void *user_data
);

/** Returns the thread-local error message from the most recent failing C API call, or `""`. */
const char *noev_last_error(void);

/** Compiles a `.nv` file or package directory (`noev.toml`) from disk. */
NoevStatus noev_program_compile_file(const char *path, NoevProgram **out);

/** Compiles a single-file Noev program from a NUL-terminated UTF-8 source string. */
NoevStatus noev_program_compile_source(const char *source, NoevProgram **out);

/** Frees a program returned by `noev_program_compile_*`. Null-safe. */
void noev_program_free(NoevProgram *program);

/** Creates a runtime for `program` using the default simulator (`ketrs` when enabled). */
NoevStatus noev_runtime_new(const NoevProgram *program, NoevHostRuntime **out);

/** Creates a runtime for `program` using the circuit-export backend (no simulator required). */
NoevStatus noev_runtime_new_export(const NoevProgram *program, NoevHostRuntime **out);

/** Sets the PRNG seed for deterministic simulation. */
void noev_runtime_set_seed(NoevHostRuntime *rt, uint64_t seed);

/** Registers a C/C++ host implementation for `extern fn <name>`. */
NoevStatus noev_runtime_register_host_fn(
    NoevHostRuntime *rt,
    const char *name,
    NoevHostFn fn,
    void *user_data
);

/** Validates that every `extern fn` in the program has a registered implementation. */
NoevStatus noev_runtime_validate(const NoevHostRuntime *rt);

/**
 * Calls the Noev function `name` with `args[0..argc]`, writing the result to `*out` (if non-NULL).
 *
 * When `*out` holds a `NOEV_VAL_STRING` or `NOEV_VAL_BYTES`, release it with `noev_value_free`.
 */
NoevStatus noev_runtime_call(
    NoevHostRuntime *rt,
    const char *name,
    const NoevValue *args,
    size_t argc,
    NoevValue *out
);

/** Re-entrantly calls a Noev function from inside a `NoevHostFn` callback. */
NoevStatus noev_host_ctx_call(
    NoevHostContext *ctx,
    const char *name,
    const NoevValue *args,
    size_t argc,
    NoevValue *out
);

/** Frees any heap buffer owned by `val` (`NOEV_VAL_STRING`, `NOEV_VAL_BYTES`) and resets it to unit. */
void noev_value_free(NoevValue *val);

/** Frees a runtime returned by `noev_runtime_new*`. Null-safe. */
void noev_runtime_free(NoevHostRuntime *rt);

/* Convenience value constructors. */

static inline NoevValue noev_val_unit(void) {
    NoevValue v;
    v.kind = NOEV_VAL_UNIT;
    v.as.u64 = 0;
    return v;
}

static inline NoevValue noev_val_int(int64_t n) {
    NoevValue v;
    v.kind = NOEV_VAL_INT;
    v.as.i64 = n;
    return v;
}

static inline NoevValue noev_val_float(double x) {
    NoevValue v;
    v.kind = NOEV_VAL_FLOAT;
    v.as.f64 = x;
    return v;
}

static inline NoevValue noev_val_f32(float x) {
    NoevValue v;
    v.kind = NOEV_VAL_F32;
    v.as.f32 = x;
    return v;
}

static inline NoevValue noev_val_i64(int64_t n) {
    NoevValue v;
    v.kind = NOEV_VAL_I64;
    v.as.i64 = n;
    return v;
}

static inline NoevValue noev_val_i32(int32_t n) {
    NoevValue v;
    v.kind = NOEV_VAL_I32;
    v.as.i32 = n;
    return v;
}

static inline NoevValue noev_val_u64(uint64_t n) {
    NoevValue v;
    v.kind = NOEV_VAL_U64;
    v.as.u64 = n;
    return v;
}

static inline NoevValue noev_val_bool(bool b) {
    NoevValue v;
    v.kind = NOEV_VAL_BOOL;
    v.as.b = b;
    return v;
}

static inline NoevValue noev_val_bit(uint8_t bit) {
    NoevValue v;
    v.kind = NOEV_VAL_BIT;
    v.as.bit = bit;
    return v;
}

static inline NoevValue noev_val_string(const char *s) {
    NoevValue v;
    v.kind = NOEV_VAL_STRING;
    v.as.str = (char *)s;
    return v;
}

static inline NoevValue noev_val_complex(double re, double im) {
    NoevValue v;
    v.kind = NOEV_VAL_COMPLEX;
    v.as.c64.re = re;
    v.as.c64.im = im;
    return v;
}

static inline NoevValue noev_val_bytes(const void *ptr, size_t len) {
    NoevValue v;
    v.kind = NOEV_VAL_BYTES;
    v.as.bytes.ptr = (uint8_t *)ptr;
    v.as.bytes.len = len;
    return v;
}

#ifdef __cplusplus
}
#endif

#endif /* NOEV_H */
