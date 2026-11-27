//! Host-function dispatch and re-entrant callbacks into Noev (`spec/interop.md` §2).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use noev_ir::ir::{FuncId, Program};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::backend::Backend;
use crate::machine::{Machine, Output, RuntimeError};
use crate::value::Value;

/// Context passed to a host function while it runs, letting it call back into the Noev program
/// (`spec/interop.md` §2).
pub struct HostCallContext<'p> {
    program: &'p Program,
    backend: &'p dyn Backend,
    hosts: Rc<HostTable<'p>>,
    rng: SmallRng,
    output: Option<Rc<RefCell<Output>>>,
}

impl<'p> HostCallContext<'p> {
    pub(crate) fn new(
        program: &'p Program,
        backend: &'p dyn Backend,
        hosts: Rc<HostTable<'p>>,
        seed: u64,
        output: Option<Rc<RefCell<Output>>>,
    ) -> Self {
        HostCallContext {
            program,
            backend,
            hosts,
            rng: SmallRng::seed_from_u64(seed),
            output,
        }
    }

    /// The compiled IR program currently executing.
    pub fn program(&self) -> &'p Program {
        self.program
    }

    /// Calls a Noev function in this program with `args`.
    ///
    /// Runs on a fresh [`Machine`] (its own quantum state, creating and consuming its own
    /// qubits), sharing the same backend, host-function table, and output streams.
    pub fn call(&mut self, func: FuncId, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let mut machine = Machine::new(self.program, self.backend, self.rng.random())
            .with_hosts(self.hosts.clone());
        if let Some(output) = &self.output {
            machine = machine.with_output(output.clone());
        }
        machine.call(func, args)
    }
}

/// Erased host-function callback stored in a [`HostTable`].
pub type HostFn<'h> =
    dyn FnMut(&mut HostCallContext<'_>, FuncId, Vec<Value>) -> Result<Value, RuntimeError> + 'h;

/// Host implementations registered for a program's `extern fn` declarations.
#[derive(Clone, Default)]
pub struct HostTable<'h> {
    fns: HashMap<String, Rc<RefCell<HostFn<'h>>>>,
}

impl<'h> HostTable<'h> {
    /// An empty host-function table.
    pub fn new() -> Self {
        HostTable {
            fns: HashMap::new(),
        }
    }

    /// Registers `f` as the host implementation for `extern fn <name>`.
    pub fn register<F>(&mut self, name: impl Into<String>, f: F)
    where
        F: FnMut(&mut HostCallContext<'_>, FuncId, Vec<Value>) -> Result<Value, RuntimeError> + 'h,
    {
        self.fns.insert(name.into(), Rc::new(RefCell::new(f)));
    }

    /// Whether a host implementation has been registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.fns.contains_key(name)
    }

    /// Calls the host implementation registered for `name`.
    pub fn invoke(
        &self,
        name: &str,
        func: FuncId,
        ctx: &mut HostCallContext<'_>,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let handler = self.fns.get(name).ok_or_else(|| {
            RuntimeError::new(format!(
                "no host implementation registered for `extern fn {name}`"
            ))
        })?;
        let mut borrow = handler.try_borrow_mut().map_err(|_| {
            RuntimeError::new(format!(
                "host function `{name}` cannot be called re-entrantly while it is already running"
            ))
        })?;
        (borrow)(ctx, func, args)
    }
}
