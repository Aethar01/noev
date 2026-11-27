//! Python bindings for the Noev hybrid classical–quantum language (`spec/interop.md` §1, §2).
//!
//! Exposed as `noev._noev` and re-exported by the `python/noev` package.

mod convert;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use noev::embed::{check_boundary_ty, show_ty};
use noev_ir::ir::{FuncId, Function, Program as IrProgram};
use noev_runtime::backend::{Backend, Exporter};
use noev_runtime::host::{HostCallContext, HostTable};
use noev_runtime::machine::{Machine, Output, RuntimeError as NoevRtError};
use noev_runtime::value::Value;
use pyo3::exceptions::{PyException, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::convert::{PyDistribution, PyEnum, PyObservable, PyStruct, noev_to_py, py_to_noev};

mod exc {
    use pyo3::create_exception;
    use pyo3::exceptions::PyException;

    create_exception!(_noev, NoevError, PyException, "Base error for Noev.");
    create_exception!(
        _noev,
        CompileError,
        NoevError,
        "A Noev program failed to parse, type-check, or ownership-check."
    );
    create_exception!(
        _noev,
        ValidationError,
        NoevError,
        "A Noev host-function registration or call signature check failed."
    );
    create_exception!(
        _noev,
        RuntimeError,
        NoevError,
        "A Noev program failed at runtime."
    );
}

fn map_noev_error(err: noev::Error) -> PyErr {
    match err {
        noev::Error::Load(m) | noev::Error::Compile(m) => exc::CompileError::new_err(m),
        noev::Error::MissingHostFn(m)
        | noev::Error::UnknownHostFn(m)
        | noev::Error::UnknownFunction(m)
        | noev::Error::SignatureMismatch(m)
        | noev::Error::BoundaryType(m) => exc::ValidationError::new_err(m),
        noev::Error::Conversion(m) => PyTypeError::new_err(m),
        noev::Error::Runtime(r) => exc::RuntimeError::new_err(r.message),
    }
}

/// A compiled Noev program, ready to be executed in a [`PyRuntime`].
#[pyclass(name = "Program", module = "noev._noev")]
#[derive(Clone)]
pub struct PyProgram {
    inner: Arc<noev::Program>,
}

#[pymethods]
impl PyProgram {
    /// Compiles a `.nv` file or package directory (`noev.toml`) from disk.
    #[staticmethod]
    fn compile_file(path: &str) -> PyResult<Self> {
        let program = noev::Program::compile_file(path).map_err(map_noev_error)?;
        Ok(PyProgram {
            inner: Arc::new(program),
        })
    }

    /// Compiles a single-file Noev program from source text.
    #[staticmethod]
    fn compile_source(source: &str) -> PyResult<Self> {
        let program = noev::Program::compile_source(source).map_err(map_noev_error)?;
        Ok(PyProgram {
            inner: Arc::new(program),
        })
    }

    /// Names of all `extern fn` host functions declared by the program.
    #[getter]
    fn extern_fns(&self) -> Vec<String> {
        self.inner.extern_fns().map(str::to_string).collect()
    }

    /// Names of all non-`extern` functions defined in the program.
    #[getter]
    fn functions(&self) -> Vec<String> {
        self.inner.functions().map(str::to_string).collect()
    }

    /// Whether the program declares a function named `name`.
    fn has_function(&self, name: &str) -> bool {
        self.inner.has_function(name)
    }

    fn __repr__(&self) -> String {
        let n_fns = self.inner.functions().count();
        let n_ext = self.inner.extern_fns().count();
        format!("Program(functions={n_fns}, extern_fns={n_ext})")
    }
}

/// RAII guard that clears a scoped [`HostCallContext`] pointer on exit or panic.
struct ScopeGuard {
    slot: Rc<Cell<Option<NonNull<HostCallContext<'static>>>>>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.slot.set(None);
    }
}

/// RAII guard that restores a scoped [`HostCallContext`] pointer after a nested `ctx.call`.
struct RestoreGuard {
    slot: Rc<Cell<Option<NonNull<HostCallContext<'static>>>>>,
    ptr: NonNull<HostCallContext<'static>>,
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        self.slot.set(Some(self.ptr));
    }
}

/// Context passed as the first argument to callbacks registered via
/// [`PyRuntime::host_fn_with_context`], letting Python host functions call back into Noev
/// (`spec/interop.md` §2).
#[pyclass(name = "HostContext", module = "noev._noev", unsendable)]
pub struct PyHostContext {
    slot: Rc<Cell<Option<NonNull<HostCallContext<'static>>>>>,
    program: Arc<noev::Program>,
    pending_err: Rc<RefCell<Option<PyErr>>>,
}

#[pymethods]
impl PyHostContext {
    /// Calls a Noev function named `name` with positional `args` and returns its result.
    ///
    /// Runs on a fresh quantum state (creating and consuming its own qubits). A runtime error
    /// inside Noev is raised as `noev.RuntimeError`.
    #[pyo3(signature = (name, *args))]
    fn call<'py>(
        &self,
        py: Python<'py>,
        name: &str,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let Some(mut ptr) = self.slot.take() else {
            return Err(exc::NoevError::new_err(
                "HostContext is only valid while its host function is running",
            ));
        };
        let _restore = RestoreGuard {
            slot: self.slot.clone(),
            ptr,
        };

        let ir = self.program.ir();
        let (func_id, func) = lookup_callable(self.program.as_ref(), name)?;
        let values = convert_py_args(py, ir, func, args)?;
        let ret_ty = func.locals[0].ty;

        // SAFETY: `slot` is populated only while the `HostCallContext` stack frame in
        // `invoke_py_host` is alive, and `slot.take()` guarantees exclusive access during this
        // call. `_restore` puts the pointer back on exit/unwind.
        let ctx: &mut HostCallContext<'_> = unsafe { ptr.as_mut() };
        match ctx.call(func_id, values) {
            Ok(val) => noev_to_py(py, ir, &val, ret_ty),
            Err(err) => {
                if let Some(py_err) = self.pending_err.borrow_mut().take() {
                    Err(py_err)
                } else {
                    Err(exc::RuntimeError::new_err(err.message))
                }
            }
        }
    }
}

struct RegisteredPyHost {
    callable: Py<PyAny>,
    with_context: bool,
}

#[derive(Clone, Copy)]
enum BackendChoice {
    #[cfg(feature = "ketrs")]
    Ketrs,
    Export,
}

/// Execution environment for a compiled [`PyProgram`], holding registered Python host
/// functions, quantum backend, and PRNG seed (`spec/interop.md` §2).
#[pyclass(name = "Runtime", module = "noev._noev", unsendable)]
pub struct PyRuntime {
    program: Arc<noev::Program>,
    backend: BackendChoice,
    hosts: HashMap<String, RegisteredPyHost>,
    seed_state: u64,
    validated: bool,
    capture_output: bool,
    last_stdout: String,
    last_stderr: String,
}

#[pymethods]
impl PyRuntime {
    /// Creates a runtime for `program`.
    ///
    /// Uses the `ketrs` simulator by default (when built with the `ketrs` feature), or
    /// `backend="export"` for circuit-only / classical execution.
    #[new]
    #[pyo3(signature = (program, *, seed = 0, backend = None))]
    fn new(program: PyRef<'_, PyProgram>, seed: u64, backend: Option<&str>) -> PyResult<Self> {
        let choice = match backend {
            #[cfg(feature = "ketrs")]
            None | Some("ketrs") => BackendChoice::Ketrs,
            #[cfg(not(feature = "ketrs"))]
            None => BackendChoice::Export,
            Some("export") => BackendChoice::Export,
            Some(other) => {
                return Err(exc::ValidationError::new_err(format!(
                    "unknown backend `{other}` (expected 'ketrs' or 'export')"
                )));
            }
        };
        Ok(PyRuntime {
            program: program.inner.clone(),
            backend: choice,
            hosts: HashMap::new(),
            seed_state: seed,
            validated: false,
            capture_output: false,
            last_stdout: String::new(),
            last_stderr: String::new(),
        })
    }

    /// Sets the PRNG seed for deterministic simulation and returns `self` for chaining.
    fn seed(mut slf: PyRefMut<'_, Self>, seed: u64) -> PyRefMut<'_, Self> {
        slf.seed_state = seed;
        slf
    }

    /// Enables or disables capturing `print`/`eprint` output in `last_stdout`/`last_stderr`.
    fn capture_output(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyRefMut<'_, Self> {
        slf.capture_output = enabled;
        slf
    }

    /// Captured stdout from the most recent `call()` when `capture_output(True)` is set.
    #[getter]
    fn last_stdout(&self) -> &str {
        &self.last_stdout
    }

    /// Captured stderr from the most recent `call()` when `capture_output(True)` is set.
    #[getter]
    fn last_stderr(&self) -> &str {
        &self.last_stderr
    }

    /// Registers a Python callable for `extern fn <name>` and returns `self` for chaining.
    fn host_fn<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        func: Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        if !func.is_callable() {
            return Err(PyTypeError::new_err(format!(
                "host implementation for `{name}` must be callable"
            )));
        }
        slf.hosts.insert(
            name,
            RegisteredPyHost {
                callable: func.unbind(),
                with_context: false,
            },
        );
        slf.validated = false;
        Ok(slf)
    }

    /// Registers a Python callable that receives a [`PyHostContext`] as its first argument so it
    /// can call back into Noev functions (`spec/interop.md` §2). Returns `self` for chaining.
    fn host_fn_with_context<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        func: Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        if !func.is_callable() {
            return Err(PyTypeError::new_err(format!(
                "host implementation for `{name}` must be callable"
            )));
        }
        slf.hosts.insert(
            name,
            RegisteredPyHost {
                callable: func.unbind(),
                with_context: true,
            },
        );
        slf.validated = false;
        Ok(slf)
    }

    /// Validates that every `extern fn` in the program has a registered callable with matching
    /// arity, and that no unknown host functions were registered.
    fn validate(&self, py: Python<'_>) -> PyResult<()> {
        let ir = self.program.ir();
        for (name, reg) in &self.hosts {
            let Some(id) = self.program.function_id(name) else {
                return Err(exc::ValidationError::new_err(format!(
                    "registered host function `{name}` is not declared as an `extern fn` in the program"
                )));
            };
            let func = &ir.functions[id.index()];
            if !func.is_extern {
                return Err(exc::ValidationError::new_err(format!(
                    "`{name}` is a Noev function, not an `extern fn`"
                )));
            }
            let expected_arity = func.params + usize::from(reg.with_context);
            check_python_arity(py, name, reg.callable.bind(py), expected_arity)?;
        }
        for func in &ir.functions {
            if func.is_extern && !self.hosts.contains_key(&func.name) {
                return Err(exc::ValidationError::new_err(format!(
                    "missing host implementation for `extern fn {}`",
                    func.name
                )));
            }
        }
        Ok(())
    }

    /// Calls the Noev function `name` with positional `args` and returns its result.
    ///
    /// Validates host functions on first call, verifies all boundary types are classical and
    /// copyable without references, converts arguments, and runs on a fresh quantum state.
    #[pyo3(signature = (name, *args))]
    fn call<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !self.validated {
            self.validate(py)?;
            self.validated = true;
        }
        let seed = self.next_seed();
        let exporter = Exporter::default();
        let backend: &dyn Backend = match self.backend {
            #[cfg(feature = "ketrs")]
            BackendChoice::Ketrs => &noev::Ketrs,
            BackendChoice::Export => &exporter,
        };

        let ir = self.program.ir();
        let (func_id, func) = lookup_callable(self.program.as_ref(), name)?;
        let values = convert_py_args(py, ir, func, args)?;
        let ret_ty = func.locals[0].ty;

        let pending_err: Rc<RefCell<Option<PyErr>>> = Rc::new(RefCell::new(None));
        let host_table = build_host_table(py, &self.program, &self.hosts, pending_err.clone());

        let mut machine = Machine::new(ir, backend, seed).with_hosts(Rc::new(host_table));
        let stdout_buf = Rc::new(RefCell::new(Vec::<u8>::new()));
        let stderr_buf = Rc::new(RefCell::new(Vec::<u8>::new()));
        if self.capture_output {
            let output = Output {
                stdout: Box::new(SharedVecWriter(stdout_buf.clone())),
                stderr: Box::new(SharedVecWriter(stderr_buf.clone())),
            };
            machine = machine.with_output(Rc::new(RefCell::new(output)));
        }

        let res = machine.call(func_id, values);

        if self.capture_output {
            self.last_stdout = String::from_utf8_lossy(&stdout_buf.borrow()).into_owned();
            self.last_stderr = String::from_utf8_lossy(&stderr_buf.borrow()).into_owned();
        }

        match res {
            Ok(val) => noev_to_py(py, ir, &val, ret_ty),
            Err(err) => {
                if let Some(py_err) = pending_err.borrow_mut().take() {
                    Err(py_err)
                } else {
                    Err(exc::RuntimeError::new_err(err.message))
                }
            }
        }
    }
}

impl PyRuntime {
    fn next_seed(&mut self) -> u64 {
        self.seed_state = self.seed_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.seed_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

struct SharedVecWriter(Rc<RefCell<Vec<u8>>>);

impl Write for SharedVecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Looks up `name` in `program` and checks that all of its parameter and return types can cross
/// the host boundary (`spec/interop.md` §1).
fn lookup_callable<'a>(program: &'a noev::Program, name: &str) -> PyResult<(FuncId, &'a Function)> {
    let Some(id) = program.function_id(name) else {
        return Err(exc::ValidationError::new_err(format!(
            "no function named `{name}` in program"
        )));
    };
    let ir = program.ir();
    let func = &ir.functions[id.index()];
    for i in 0..func.params {
        let ty = func.locals[1 + i].ty;
        check_boundary_ty(ir, ty, &format!("parameter {}", i + 1), &func.name)
            .map_err(map_noev_error)?;
    }
    check_boundary_ty(ir, func.locals[0].ty, "return type", &func.name).map_err(map_noev_error)?;
    Ok((id, func))
}

fn convert_py_args(
    py: Python<'_>,
    ir: &IrProgram,
    func: &Function,
    args: &Bound<'_, PyTuple>,
) -> PyResult<Vec<Value>> {
    if args.len() != func.params {
        let expected: Vec<String> = (0..func.params)
            .map(|i| show_ty(ir, func.locals[1 + i].ty))
            .collect();
        return Err(exc::ValidationError::new_err(format!(
            "`{}` expects {} argument(s) ({}), got {}",
            func.name,
            func.params,
            expected.join(", "),
            args.len()
        )));
    }
    let mut values = Vec::with_capacity(func.params);
    for (i, item) in args.iter().enumerate() {
        let ty = func.locals[1 + i].ty;
        values.push(py_to_noev(py, ir, &item, ty)?);
    }
    Ok(values)
}

/// Uses Python's `inspect.signature` when available to check that `callable` can be called with
/// `expected_arity` positional arguments. Callables without inspectable signatures (e.g. C
/// builtins) are accepted and checked when called.
fn check_python_arity(
    py: Python<'_>,
    name: &str,
    callable: &Bound<'_, PyAny>,
    expected_arity: usize,
) -> PyResult<()> {
    let inspect = py.import("inspect")?;
    let Ok(sig) = inspect.call_method1("signature", (callable,)) else {
        return Ok(());
    };
    let placeholders: Vec<Bound<'_, PyAny>> = (0..expected_arity)
        .map(|_| py.None().into_bound(py))
        .collect();
    let tuple = PyTuple::new(py, placeholders)?;
    if let Err(err) = sig.call_method("bind", tuple, None) {
        return Err(exc::ValidationError::new_err(format!(
            "host function `{name}` has incompatible signature for {expected_arity} positional argument(s): {err}"
        )));
    }
    Ok(())
}

fn build_host_table<'py>(
    py: Python<'py>,
    program: &Arc<noev::Program>,
    hosts: &HashMap<String, RegisteredPyHost>,
    pending_err: Rc<RefCell<Option<PyErr>>>,
) -> HostTable<'py> {
    let mut table = HostTable::new();
    for (name, reg) in hosts {
        let callable = reg.callable.clone_ref(py);
        let with_context = reg.with_context;
        let program = program.clone();
        let pending_err = pending_err.clone();
        table.register(name.clone(), move |ctx, func_id, args| {
            invoke_py_host(
                py,
                &program,
                callable.bind(py),
                with_context,
                ctx,
                func_id,
                args,
                &pending_err,
            )
        });
    }
    table
}

#[allow(clippy::too_many_arguments)]
fn invoke_py_host(
    py: Python<'_>,
    program: &Arc<noev::Program>,
    callable: &Bound<'_, PyAny>,
    with_context: bool,
    ctx: &mut HostCallContext<'_>,
    func_id: FuncId,
    args: Vec<Value>,
    pending_err: &Rc<RefCell<Option<PyErr>>>,
) -> Result<Value, NoevRtError> {
    let ir = program.ir();
    let func = &ir.functions[func_id.index()];
    let mut py_args = Vec::with_capacity(args.len() + usize::from(with_context));
    let mut _guard = None;

    if with_context {
        // Erase the stack lifetime to `'static` ONLY for the duration of this call;
        // `_guard` unconditionally clears `slot` to `None` on return or unwind.
        let raw_ptr = (ctx as *mut HostCallContext<'_>).cast::<HostCallContext<'static>>();
        let slot = Rc::new(Cell::new(NonNull::new(raw_ptr)));
        _guard = Some(ScopeGuard { slot: slot.clone() });
        let py_ctx = Py::new(
            py,
            PyHostContext {
                slot,
                program: program.clone(),
                pending_err: pending_err.clone(),
            },
        )
        .map_err(|e| store_py_err(pending_err, e))?;
        py_args.push(py_ctx.into_bound(py).into_any());
    }

    for (i, val) in args.iter().enumerate() {
        let param_ty = func.locals[1 + i].ty;
        let py_val = noev_to_py(py, ir, val, param_ty).map_err(|e| store_py_err(pending_err, e))?;
        py_args.push(py_val);
    }

    let tuple = PyTuple::new(py, py_args).map_err(|e| store_py_err(pending_err, e))?;
    let result = callable
        .call1(tuple)
        .map_err(|e| store_py_err(pending_err, e))?;

    let ret_ty = func.locals[0].ty;
    py_to_noev(py, ir, &result, ret_ty).map_err(|e| store_py_err(pending_err, e))
}

fn store_py_err(slot: &Rc<RefCell<Option<PyErr>>>, err: PyErr) -> NoevRtError {
    let msg = err.to_string();
    let mut guard = slot.borrow_mut();
    if guard.is_none() {
        *guard = Some(err);
    }
    NoevRtError::new(msg)
}

/// The `noev._noev` Python extension module.
#[pymodule]
fn _noev(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyProgram>()?;
    m.add_class::<PyRuntime>()?;
    m.add_class::<PyHostContext>()?;
    m.add_class::<PyObservable>()?;
    m.add_class::<PyStruct>()?;
    m.add_class::<PyEnum>()?;
    m.add_class::<PyDistribution>()?;
    m.add("NoevError", m.py().get_type::<exc::NoevError>())?;
    m.add("CompileError", m.py().get_type::<exc::CompileError>())?;
    m.add("ValidationError", m.py().get_type::<exc::ValidationError>())?;
    m.add("RuntimeError", m.py().get_type::<exc::RuntimeError>())?;
    // Ensure unused import warning doesn't fire if PyException is only used in `mod exc`.
    let _ = m.py().get_type::<PyException>();
    Ok(())
}
