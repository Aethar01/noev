//! Name resolution, type checking and ownership checking for Noev.
//!
//! Implements `spec/types.md` and `spec/ownership.md`.

pub mod check;
pub mod circuit;
pub mod facts;
pub mod ownership;
pub mod prelude;
pub mod ty;

pub use check::{Typed, check};
pub use circuit::{CircuitInfo, Circuits, check_circuits};
pub use ownership::{check_ownership, reversibility};
pub use ty::{Ty, TyKind, Types};
