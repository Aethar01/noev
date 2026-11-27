/*
 * Noev Quantum Backend Plug-in C ABI (spec/runtime.md §3–§5).
 *
 * A dynamically loaded quantum backend shared library (.so / .dylib) exports a single
 * C entry point:
 *
 *     const NoevBackendVTable* noev_backend_vtable(void);
 *
 * Backends written in C, C++, Rust, or compiled Noev share this exact interface and
 * can be loaded with `noev run --backend=PATH[:DEVICE]`, cached with
 * `noev target fetch NAME --from PATH`, or loaded dynamically at startup via
 * `NOEV_BACKEND=PATH[:DEVICE]` in compiled binaries.
 */

#ifndef NOEV_BACKEND_H
#define NOEV_BACKEND_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define NOEV_BACKEND_ABI_VERSION 1u

/* Execution level (`NoevBackendVTable.level`). */
#define NOEV_LEVEL_CIRCUIT 0u
#define NOEV_LEVEL_GATE    1u
#define NOEV_LEVEL_BOTH    2u

/* Capability bitmask (`NoevBackendVTable.capabilities`). */
#define NOEV_CAP_READ_STATE         (1u << 0)
#define NOEV_CAP_CHECK_RELEASE      (1u << 1)
#define NOEV_CAP_EXACT_EXPECTATIONS (1u << 2)
#define NOEV_CAP_SAMPLE_STATE       (1u << 3)
#define NOEV_CAP_FOREIGN_WHILE_LIVE (1u << 4)

/* Core gate IDs for gate-level sessions (`session_apply`). */
#define NOEV_GATE_X       0u
#define NOEV_GATE_Y       1u
#define NOEV_GATE_Z       2u
#define NOEV_GATE_H       3u
#define NOEV_GATE_S       4u
#define NOEV_GATE_T       5u
#define NOEV_GATE_RX      6u
#define NOEV_GATE_RY      7u
#define NOEV_GATE_RZ      8u
#define NOEV_GATE_P       9u
#define NOEV_GATE_U       10u
#define NOEV_GATE_SWAP    11u
#define NOEV_GATE_UNITARY 12u

/* Callback invoked by `run_circuit` for each observed shot outcome bitstring. */
typedef void (*NoevCountCallback)(void* cb_env, uint64_t bits, size_t count);

typedef struct NoevBackendVTable {
    /* Must equal NOEV_BACKEND_ABI_VERSION (1). */
    uint32_t abi_version;

    /* Canonical backend name (UTF-8, NUL-terminated). */
    const char* name;

    /* NOEV_LEVEL_CIRCUIT (0), NOEV_LEVEL_GATE (1), or NOEV_LEVEL_BOTH (2). */
    uint8_t level;

    /* Maximum simultaneous qubits, or -1 if unbounded. */
    int64_t qubits;

    /* Bitmask of NOEV_CAP_* flags. */
    uint32_t capabilities;

    /* Optional full TOML target description (nullable). */
    const char* target_toml;

    /* Create and destroy a backend instance (device_or_null may be NULL). */
    void* (*create)(const char* device_or_null);
    void (*destroy)(void* backend);

    /*
     * Circuit-level execution (may be NULL if level == NOEV_LEVEL_GATE).
     * `qasm3` is the recorded circuit as UTF-8 OpenQASM 3 (NUL-terminated).
     * Report shot outcome counts via `on_count(cb_env, bits, count)`.
     * Return true on success, or write a NUL-terminated error into `err_buf` and return false.
     */
    bool (*run_circuit)(
        void* backend,
        const char* qasm3,
        size_t qasm3_len,
        size_t n_qubits,
        size_t n_clbits,
        size_t shots,
        uint64_t seed,
        NoevCountCallback on_count,
        void* cb_env,
        char* err_buf,
        size_t err_cap
    );

    /*
     * Gate-level session interface (may be NULL if level == NOEV_LEVEL_CIRCUIT).
     */
    void* (*session_new)(void* backend, uint64_t seed);
    void (*session_free)(void* session);
    bool (*session_add_qubit)(void* session, size_t* out_qubit, char* err_buf, size_t err_cap);
    bool (*session_apply)(
        void* session,
        uint8_t gate_id,
        bool adjoint,
        const double* params,
        size_t n_params,
        const size_t* controls,
        size_t n_controls,
        const size_t* targets,
        size_t n_targets,
        char* err_buf,
        size_t err_cap
    );
    bool (*session_measure)(void* session, size_t qubit, bool* out_bit, char* err_buf, size_t err_cap);
    bool (*session_reset)(void* session, size_t qubit, char* err_buf, size_t err_cap);
    bool (*session_release)(void* session, size_t qubit, char* err_buf, size_t err_cap);
} NoevBackendVTable;

/* Symbol every plug-in shared library must export. */
typedef const NoevBackendVTable* (*NoevBackendVTableFn)(void);

#ifdef __cplusplus
}
#endif

#endif /* NOEV_BACKEND_H */
