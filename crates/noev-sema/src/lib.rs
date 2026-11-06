//! Name resolution, type checking and ownership checking for Noev.
//!
//! Implements `spec/types.md` and `spec/ownership.md`.

pub mod check;
pub mod prelude;
pub mod ty;

pub use check::{Typed, check};
pub use ty::{Ty, TyKind, Types};
