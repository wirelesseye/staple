//! The syntax runtime that compiled macro code calls.
//!
//! Macro code reaches the compiler through the `std.syntax` intrinsics. They
//! are lowered as ordinary external calls; the host supplies their addresses
//! from [`intrinsics::TABLE`]. In tests, [`harness`] runs the checked code
//! in-process through LLVM's JIT.

pub(crate) mod arena;
pub(crate) mod args;
pub(crate) mod intrinsics;
pub(crate) mod manifest;
pub(crate) mod quote;
pub(crate) mod wire;

#[cfg(test)]
pub(crate) mod compare;
#[cfg(test)]
pub(crate) mod harness;
