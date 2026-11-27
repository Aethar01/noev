//! Embedding Noev in Rust programs (`spec/interop.md` §1, §2).
//!
//! ```no_run
//! # fn main() -> Result<(), noev::Error> {
//! let program = noev::Program::compile_source(r#"
//!     extern fn add_one(x: Float) -> Float
//!     fn run(x: Float) -> Float { add_one(x) }
//! "#)?;
//! # #[cfg(feature = "ketrs")]
//! # {
//! let mut runtime = noev::Runtime::new(&program)
//!     .host_fn("add_one", |x: f64| x + 1.0);
//! let y: f64 = runtime.call("run", (41.0,))?;
//! assert_eq!(y, 42.0);
//! # }
//! # Ok(())
//! # }
//! ```

pub mod embed;
#[doc(hidden)]
pub mod package;

pub use embed::{
    Bit, C32, Complex, Error, FromNoev, HostContext, HostContextHandler, HostHandler, HostReturn,
    IntoNoev, NoevArgs, NoevType, Program, Runtime,
};
#[cfg(feature = "ketrs")]
pub use noev_runtime::Ketrs;
pub use noev_runtime::value::{Observable, Pauli};
pub use noev_runtime::{Backend, Output, RuntimeError, Value};
