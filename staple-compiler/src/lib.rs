//! The Staple compiler pipeline.
//!
//! Source parsing produces syntax modules in a loaded [`Program`]. Macro expansion
//! rewrites that syntax before [`NameResolver`] produces a [`ResolvedModule`]
//! with stable semantic identities. [`TypeChecker`] consumes the resolved program
//! and produces a [`TypedModule`] with checked types, effects, ownership facts,
//! and selected trait evidence. [`Lowerer`] consumes that checked program and
//! produces an owned [`LoweredModule`] containing a validated `LoweredProgram`.
//! [`CodeGenerator`] emits LLVM IR and object files from that artifact.
//! The CLI links those objects into executables.
//!
//! The phase order is parse → expand → resolve → check → lower → emit. Each phase
//! reads its predecessor's output, including facts carried forward in that output.
//! Lowering closes concrete specialization and generated-artifact catalogs before
//! emission. Codegen reads only the lowered program; it never queries the checker
//! or walks source syntax to recover semantic decisions.

mod codegen;
mod coroutine_lower;
mod expansion_render;
mod lower;
mod macro_expand;
mod ownership;
mod program;
mod resolve;
mod specialization;
mod typecheck;

pub use codegen::*;
pub use expansion_render::render_expanded_module;
pub use lower::*;
pub use macro_expand::expand_macros;
pub use program::*;
pub use resolve::*;
pub use typecheck::*;
