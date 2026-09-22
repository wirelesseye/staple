//! Typed lowering boundary.
//!
//! This module owns the transition from a successfully checked program to the
//! representation consumed by code generation. Stage 1 establishes the
//! boundary and validates the checked input. The existing typed module is kept
//! as a temporary backend bridge while later stages move each construct into
//! explicit lowered arenas.

use staple_syntax::Diagnostic;

use crate::TypedModule;

/// A program accepted by the lowering phase and ready for code generation.
///
/// The fields are intentionally private: callers may pass this value to later
/// compiler phases, but cannot depend on the transitional representation.
#[derive(Debug, Clone)]
pub struct LoweredModule {
    typed: TypedModule,
}

impl LoweredModule {
    pub(crate) fn typed(&self) -> &TypedModule {
        &self.typed
    }
}

/// Converts checked compiler state into the code-generation input.
#[derive(Debug, Default, Clone, Copy)]
pub struct Lowerer;

impl Lowerer {
    pub fn new() -> Self {
        Self
    }

    pub fn lower(&self, module: &TypedModule) -> Result<LoweredModule, Vec<Diagnostic>> {
        let diagnostics = validate_checked_module(module);
        if diagnostics.is_empty() {
            Ok(LoweredModule {
                typed: module.clone(),
            })
        } else {
            Err(diagnostics)
        }
    }
}

fn validate_checked_module(module: &TypedModule) -> Vec<Diagnostic> {
    module
        .functions()
        .iter()
        .chain(module.implicit_thunks())
        .filter(|function| module.type_of_function(function.id).is_none())
        .map(|function| {
            Diagnostic::new(
                function.body.syntax().span.clone(),
                format!(
                    "cannot lower unchecked function `{}` (function id {})",
                    function.name, function.id.0
                ),
            )
        })
        .collect()
}
