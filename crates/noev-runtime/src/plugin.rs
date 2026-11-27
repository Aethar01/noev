//! Dynamically loaded plug-in quantum backends (`spec/runtime.md` §3–§5,
//! `include/noev_backend.h`).
//!
//! Loads a shared library (`.so` / `.dylib`) exporting `noev_backend_vtable`, checks its ABI
//! version (`NOEV_BACKEND_ABI_VERSION == 1`), extracts its [`TargetSpec`], and exposes it as
//! a first-class [`Backend`].

use std::ffi::{CStr, c_char, c_void};
#[cfg(unix)]
use std::ffi::{CString, c_int};
use std::path::Path;

use noev_sema::prelude::Gate;
use noev_sema::{BackendLevel, TargetCapabilities, TargetSpec};

use crate::backend::{Backend, BackendError, Circuit, Counts, Op, Session};

/// Current plug-in ABI version (must match `NOEV_BACKEND_ABI_VERSION` in `include/noev_backend.h`).
pub const NOEV_BACKEND_ABI_VERSION: u32 = 1;

/// Callback invoked by a plug-in's `run_circuit` for each observed shot outcome.
pub type NoevCountCallback = extern "C" fn(cb_env: *mut c_void, bits: u64, count: usize);

/// Circuit-level execution callback in [`NoevBackendVTable`].
pub type RunCircuitFn = unsafe extern "C" fn(
    backend: *mut c_void,
    qasm3: *const c_char,
    qasm3_len: usize,
    n_qubits: usize,
    n_clbits: usize,
    shots: usize,
    seed: u64,
    on_count: NoevCountCallback,
    cb_env: *mut c_void,
    err_buf: *mut c_char,
    err_cap: usize,
) -> bool;

/// Gate-level apply callback in [`NoevBackendVTable`].
pub type SessionApplyFn = unsafe extern "C" fn(
    session: *mut c_void,
    gate_id: u8,
    adjoint: bool,
    params: *const f64,
    n_params: usize,
    controls: *const usize,
    n_controls: usize,
    targets: *const usize,
    n_targets: usize,
    err_buf: *mut c_char,
    err_cap: usize,
) -> bool;

/// C ABI vtable exported by a plug-in backend (`include/noev_backend.h`).
#[repr(C)]
pub struct NoevBackendVTable {
    /// Must equal [`NOEV_BACKEND_ABI_VERSION`].
    pub abi_version: u32,
    /// Backend name (UTF-8, NUL-terminated).
    pub name: *const c_char,
    /// Execution level (`0` = Circuit, `1` = Gate, `2` = Both).
    pub level: u8,
    /// Maximum simultaneous qubits, or `-1` if unbounded.
    pub qubits: i64,
    /// Capability bitmask (`TargetCapabilities::BIT_*`).
    pub capabilities: u32,
    /// Optional full TOML target description (nullable).
    pub target_toml: *const c_char,
    /// Creates a backend instance (`device_or_null` may be null).
    pub create: Option<unsafe extern "C" fn(device_or_null: *const c_char) -> *mut c_void>,
    /// Destroys a backend instance.
    pub destroy: Option<unsafe extern "C" fn(backend: *mut c_void)>,
    /// Circuit-level execution (optional).
    pub run_circuit: Option<RunCircuitFn>,
    /// Gate-level session constructor (optional).
    pub session_new: Option<unsafe extern "C" fn(backend: *mut c_void, seed: u64) -> *mut c_void>,
    /// Gate-level session destructor (optional).
    pub session_free: Option<unsafe extern "C" fn(session: *mut c_void)>,
    /// Adds a qubit in |0⟩.
    pub session_add_qubit: Option<
        unsafe extern "C" fn(
            session: *mut c_void,
            out_qubit: *mut usize,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> bool,
    >,
    /// Applies a gate.
    pub session_apply: Option<SessionApplyFn>,
    /// Measures a qubit in Z.
    pub session_measure: Option<
        unsafe extern "C" fn(
            session: *mut c_void,
            qubit: usize,
            out_bit: *mut bool,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> bool,
    >,
    /// Resets a qubit to |0⟩.
    pub session_reset: Option<
        unsafe extern "C" fn(
            session: *mut c_void,
            qubit: usize,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> bool,
    >,
    /// Releases a qubit claimed to be in |0⟩.
    pub session_release: Option<
        unsafe extern "C" fn(
            session: *mut c_void,
            qubit: usize,
            err_buf: *mut c_char,
            err_cap: usize,
        ) -> bool,
    >,
}

#[cfg(unix)]
const RTLD_NOW: c_int = 2;

#[cfg(unix)]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

/// A dynamically loaded plug-in backend implementing [`Backend`].
pub struct PluginBackend {
    #[cfg(unix)]
    lib_handle: *mut c_void,
    vtable: *const NoevBackendVTable,
    instance: *mut c_void,
    target: TargetSpec,
}

impl PluginBackend {
    /// Loads a plug-in backend shared library from `path`, passing optional `device` to its
    /// `create` callback.
    pub fn load(path: &Path, device: Option<&str>) -> Result<Self, BackendError> {
        #[cfg(not(unix))]
        {
            let _ = (path, device);
            return Err(BackendError(
                "dynamic plug-in backends are only supported on Unix platforms".into(),
            ));
        }
        #[cfg(unix)]
        {
            let cpath = CString::new(path.to_string_lossy().as_bytes())
                .map_err(|_| BackendError("plug-in path contains NUL byte".into()))?;
            let handle = unsafe { dlopen(cpath.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                let reason = unsafe {
                    let e = dlerror();
                    if e.is_null() {
                        "unknown dlopen error".to_string()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    }
                };
                return Err(BackendError(format!(
                    "cannot load backend plug-in `{}`: {reason}",
                    path.display()
                )));
            }
            let sym_name = c"noev_backend_vtable";
            let sym = unsafe { dlsym(handle, sym_name.as_ptr()) };
            if sym.is_null() {
                unsafe { dlclose(handle) };
                return Err(BackendError(format!(
                    "backend plug-in `{}` does not export `noev_backend_vtable`",
                    path.display()
                )));
            }
            let get_vtable: unsafe extern "C" fn() -> *const NoevBackendVTable =
                unsafe { std::mem::transmute(sym) };
            let vtable = unsafe { get_vtable() };
            if vtable.is_null() {
                unsafe { dlclose(handle) };
                return Err(BackendError(format!(
                    "backend plug-in `{}` returned a null vtable",
                    path.display()
                )));
            }
            let vt = unsafe { &*vtable };
            if vt.abi_version != NOEV_BACKEND_ABI_VERSION {
                let ver = vt.abi_version;
                unsafe { dlclose(handle) };
                return Err(BackendError(format!(
                    "backend plug-in `{}` uses ABI version {ver}, but this runtime expects {NOEV_BACKEND_ABI_VERSION}",
                    path.display()
                )));
            }
            let mut target = extract_target(vt, path)?;
            let canonical = path
                .canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .to_string_lossy()
                .into_owned();
            target.plugin = Some(canonical);
            if let Some(dev) = device {
                target.device = Some(dev.to_string());
            }

            let instance = if let Some(create) = vt.create {
                let cdev = match device {
                    Some(d) => Some(
                        CString::new(d)
                            .map_err(|_| BackendError("device name contains NUL byte".into()))?,
                    ),
                    None => None,
                };
                let ptr = unsafe { create(cdev.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())) };
                if ptr.is_null() {
                    unsafe { dlclose(handle) };
                    return Err(BackendError(format!(
                        "backend plug-in `{}` failed to initialise",
                        target.name
                    )));
                }
                ptr
            } else {
                std::ptr::null_mut()
            };

            Ok(PluginBackend {
                lib_handle: handle,
                vtable,
                instance,
                target,
            })
        }
    }

    /// Reads the [`TargetSpec`] from a plug-in without keeping it loaded.
    pub fn probe_target(path: &Path, device: Option<&str>) -> Result<TargetSpec, BackendError> {
        let plugin = Self::load(path, device)?;
        Ok(plugin.target.clone())
    }
}

impl Drop for PluginBackend {
    fn drop(&mut self) {
        let vt = unsafe { &*self.vtable };
        if let Some(destroy) = vt.destroy
            && !self.instance.is_null()
        {
            unsafe { destroy(self.instance) };
        }
        #[cfg(unix)]
        if !self.lib_handle.is_null() {
            unsafe { dlclose(self.lib_handle) };
        }
    }
}

#[cfg(unix)]
fn extract_target(vt: &NoevBackendVTable, path: &Path) -> Result<TargetSpec, BackendError> {
    if !vt.target_toml.is_null() {
        let text = unsafe { CStr::from_ptr(vt.target_toml) }
            .to_str()
            .map_err(|_| BackendError("plug-in `target_toml` is not valid UTF-8".into()))?;
        return TargetSpec::from_toml(text)
            .map_err(|e| BackendError(format!("invalid plug-in `target_toml`: {e}")));
    }
    let name = if vt.name.is_null() {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string()
    } else {
        unsafe { CStr::from_ptr(vt.name) }
            .to_string_lossy()
            .into_owned()
    };
    let level = BackendLevel::from_abi(vt.level).ok_or_else(|| {
        BackendError(format!(
            "plug-in `{name}` declared invalid level {}",
            vt.level
        ))
    })?;
    let qubits = if vt.qubits < 0 {
        None
    } else {
        Some(vt.qubits as usize)
    };
    Ok(TargetSpec {
        name,
        device: None,
        level,
        qubits,
        gates: None,
        coupling: None,
        capabilities: TargetCapabilities::from_bits(vt.capabilities),
        plugin: None,
    })
}

fn read_err(buf: &[c_char], fallback: &str) -> BackendError {
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_string();
    if msg.is_empty() {
        BackendError(fallback.into())
    } else {
        BackendError(msg)
    }
}

fn gate_id(gate: Gate) -> u8 {
    match gate {
        Gate::X => 0,
        Gate::Y => 1,
        Gate::Z => 2,
        Gate::H => 3,
        Gate::S => 4,
        Gate::T => 5,
        Gate::Rx => 6,
        Gate::Ry => 7,
        Gate::Rz => 8,
        Gate::P => 9,
        Gate::U => 10,
        Gate::Swap => 11,
        Gate::Unitary => 12,
    }
}

extern "C" fn collect_count(cb_env: *mut c_void, bits: u64, count: usize) {
    let counts = unsafe { &mut *(cb_env as *mut Counts) };
    counts.push((bits, count));
}

impl Backend for PluginBackend {
    fn name(&self) -> &str {
        &self.target.name
    }

    fn target(&self) -> TargetSpec {
        self.target.clone()
    }

    fn exact_expectations(&self) -> bool {
        self.target.capabilities.exact_expectations
    }

    fn session(&self, seed: u64) -> Option<Box<dyn Session>> {
        let vt = unsafe { &*self.vtable };
        let new_fn = vt.session_new?;
        let raw = unsafe { new_fn(self.instance, seed) };
        if raw.is_null() {
            return None;
        }
        Some(Box::new(PluginSession {
            vtable: self.vtable,
            session: raw,
        }))
    }

    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>> {
        let vt = unsafe { &*self.vtable };
        let run_fn = vt.run_circuit?;
        let qasm3 = crate::qasm::to_qasm3(circuit, &[]);
        let Ok(c_qasm) = CString::new(qasm3) else {
            return Some(Err(BackendError(
                "circuit OpenQASM 3 contains NUL byte".into(),
            )));
        };
        let mut counts: Counts = Vec::new();
        let mut err_buf = [0 as c_char; 512];
        let ok = unsafe {
            run_fn(
                self.instance,
                c_qasm.as_ptr(),
                c_qasm.as_bytes().len(),
                circuit.qubits,
                circuit.clbits,
                shots,
                seed,
                collect_count,
                (&mut counts as *mut Counts).cast::<c_void>(),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        };
        if ok {
            Some(Ok(counts))
        } else {
            Some(Err(read_err(&err_buf, "plug-in `run_circuit` failed")))
        }
    }
}

struct PluginSession {
    vtable: *const NoevBackendVTable,
    session: *mut c_void,
}

impl Drop for PluginSession {
    fn drop(&mut self) {
        let vt = unsafe { &*self.vtable };
        if let Some(free_fn) = vt.session_free
            && !self.session.is_null()
        {
            unsafe { free_fn(self.session) };
        }
    }
}

impl Session for PluginSession {
    fn add_qubit(&mut self) -> Result<usize, BackendError> {
        let vt = unsafe { &*self.vtable };
        let f = vt
            .session_add_qubit
            .ok_or_else(|| BackendError("plug-in does not implement `session_add_qubit`".into()))?;
        let mut q = 0usize;
        let mut err_buf = [0 as c_char; 512];
        if unsafe { f(self.session, &mut q, err_buf.as_mut_ptr(), err_buf.len()) } {
            Ok(q)
        } else {
            Err(read_err(&err_buf, "plug-in `session_add_qubit` failed"))
        }
    }

    fn apply(&mut self, op: &Op) -> Result<(), BackendError> {
        let vt = unsafe { &*self.vtable };
        let f = vt
            .session_apply
            .ok_or_else(|| BackendError("plug-in does not implement `session_apply`".into()))?;
        let mut err_buf = [0 as c_char; 512];
        if unsafe {
            f(
                self.session,
                gate_id(op.gate),
                op.adjoint,
                op.params.as_ptr(),
                op.params.len(),
                op.controls.as_ptr(),
                op.controls.len(),
                op.targets.as_ptr(),
                op.targets.len(),
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        } {
            Ok(())
        } else {
            Err(read_err(&err_buf, "plug-in `session_apply` failed"))
        }
    }

    fn measure(&mut self, qubit: usize) -> Result<bool, BackendError> {
        let vt = unsafe { &*self.vtable };
        let f = vt
            .session_measure
            .ok_or_else(|| BackendError("plug-in does not implement `session_measure`".into()))?;
        let mut bit = false;
        let mut err_buf = [0 as c_char; 512];
        if unsafe {
            f(
                self.session,
                qubit,
                &mut bit,
                err_buf.as_mut_ptr(),
                err_buf.len(),
            )
        } {
            Ok(bit)
        } else {
            Err(read_err(&err_buf, "plug-in `session_measure` failed"))
        }
    }

    fn reset(&mut self, qubit: usize) -> Result<(), BackendError> {
        let vt = unsafe { &*self.vtable };
        let f = vt
            .session_reset
            .ok_or_else(|| BackendError("plug-in does not implement `session_reset`".into()))?;
        let mut err_buf = [0 as c_char; 512];
        if unsafe { f(self.session, qubit, err_buf.as_mut_ptr(), err_buf.len()) } {
            Ok(())
        } else {
            Err(read_err(&err_buf, "plug-in `session_reset` failed"))
        }
    }

    fn release(&mut self, qubit: usize) -> Result<(), BackendError> {
        let vt = unsafe { &*self.vtable };
        let Some(f) = vt.session_release else {
            return Ok(());
        };
        let mut err_buf = [0 as c_char; 512];
        if unsafe { f(self.session, qubit, err_buf.as_mut_ptr(), err_buf.len()) } {
            Ok(())
        } else {
            Err(read_err(&err_buf, "plug-in `session_release` failed"))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Target cache and backend specification resolution
// ---------------------------------------------------------------------------------------

/// Returns the directory where `noev target fetch` caches target TOML files.
///
/// Respects `NOEV_TARGET_CACHE` first, then `$XDG_CACHE_HOME/noev/targets`, then
/// `$HOME/.cache/noev/targets`.
pub fn target_cache_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("NOEV_TARGET_CACHE")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return std::path::PathBuf::from(xdg).join("noev").join("targets");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return std::path::PathBuf::from(home)
            .join(".cache")
            .join("noev")
            .join("targets");
    }
    std::env::temp_dir().join("noev-targets")
}

/// Sanitizes a `NAME[:DEVICE]` target key into a cache filename (`<key>.toml`).
pub fn target_cache_path(key: &str) -> std::path::PathBuf {
    let safe: String = key
        .chars()
        .map(|c| match c {
            ':' => '_',
            '/' | '\\' => '-',
            _ => c,
        })
        .collect();
    target_cache_dir().join(format!("{safe}.toml"))
}

/// Splits a backend spec into `(base, Option<device>)`.
///
/// If the full string names an existing path on disk, it is treated as `(spec, None)` so
/// colons inside filenames are not misinterpreted.
pub fn split_backend_spec(spec: &str) -> (&str, Option<&str>) {
    if Path::new(spec).exists() {
        return (spec, None);
    }
    if let Some((left, right)) = spec.rsplit_once(':')
        && !left.is_empty()
        && !right.is_empty()
    {
        return (left, Some(right));
    }
    (spec, None)
}

/// Reads a cached [`TargetSpec`] by name (`NAME` or `NAME:DEVICE`).
pub fn read_cached_target(spec: &str) -> Result<Option<TargetSpec>, String> {
    let path = target_cache_path(spec);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read cached target `{}`: {e}", path.display()))?;
    TargetSpec::from_toml(&text).map(Some)
}

/// Writes `target` into the local target cache under `key`.
pub fn write_cached_target(key: &str, target: &TargetSpec) -> Result<std::path::PathBuf, String> {
    let path = target_cache_path(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create target cache `{}`: {e}", parent.display()))?;
    }
    std::fs::write(&path, target.to_toml())
        .map_err(|e| format!("cannot write cached target `{}`: {e}", path.display()))?;
    Ok(path)
}

/// Lists all cached `(key, TargetSpec)` entries in the target cache directory.
pub fn list_cached_targets() -> Result<Vec<(String, TargetSpec)>, String> {
    let dir = target_cache_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot read target cache `{}`: {e}", dir.display()))?;
    for entry in read_dir {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") && path.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
            let target = TargetSpec::from_toml(&text)?;
            let key = match &target.device {
                Some(dev) => format!("{}:{dev}", target.name),
                None => target.name.clone(),
            };
            entries.push((key, target));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn is_plugin_path(path: &Path) -> bool {
    path.is_file()
        && matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("so" | "dylib" | "dll")
        )
}

/// Resolves a backend spec (`ketrs`, `export`, a `.toml` file, a plug-in `.so`/`.dylib`
/// optionally with `:device`, or a cached target name) into its [`TargetSpec`].
pub fn resolve_target_spec(spec: &str) -> Result<TargetSpec, String> {
    if let Some(builtin) = TargetSpec::builtin(spec) {
        return Ok(builtin);
    }
    let (base, device) = split_backend_spec(spec);
    let base_path = Path::new(base);
    if base_path.is_file() {
        if base_path.extension().is_some_and(|e| e == "toml") {
            let text = std::fs::read_to_string(base_path)
                .map_err(|e| format!("cannot read target file `{}`: {e}", base_path.display()))?;
            let mut target = TargetSpec::from_toml(&text)?;
            if let Some(dev) = device {
                target.device = Some(dev.to_string());
            }
            return Ok(target);
        }
        if is_plugin_path(base_path) {
            return PluginBackend::probe_target(base_path, device).map_err(|e| e.0);
        }
    }
    if let Some(target) = read_cached_target(spec)? {
        return Ok(target);
    }
    if let Some(dev) = device
        && let Some(mut target) = read_cached_target(base)?
    {
        target.device = Some(dev.to_string());
        return Ok(target);
    }
    Err(format!(
        "unknown backend target `{spec}`: use `ketrs`, `export`, a target `.toml` or plug-in \
         library path, or run `noev target fetch {spec} --from <PATH>`"
    ))
}

/// Loads a plug-in backend from a backend spec (direct `.so`/`.dylib` path with optional
/// `:device`, or a `.toml` / cached target whose `plugin = "…"` points to a plug-in library).
pub fn load_plugin_spec(spec: &str) -> Result<PluginBackend, BackendError> {
    let (base, device) = split_backend_spec(spec);
    let base_path = Path::new(base);
    if is_plugin_path(base_path) {
        return PluginBackend::load(base_path, device);
    }
    let target = resolve_target_spec(spec).map_err(BackendError)?;
    let Some(plugin_path) = target.plugin.as_deref() else {
        return Err(BackendError(format!(
            "target `{spec}` is a description only and has no `plugin` library to run"
        )));
    };
    let dev = device.or(target.device.as_deref());
    PluginBackend::load(Path::new(plugin_path), dev)
}
