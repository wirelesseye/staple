//! Typed lowering boundary and owned lowered representation.
//!
//! Lowering owns the transition from a successfully checked program to the
//! representation consumed by code generation. The explicit arenas are being
//! populated incrementally during Stage 2. The existing typed module remains
//! a temporary, private backend bridge until code generation is migrated.

#![allow(dead_code)] // Stage 2 populates and consumes this schema incrementally.

use std::marker::PhantomData;

use staple_syntax::{Diagnostic, Span, SyntaxId};

use crate::{
    CheckedCoercion, CheckedEffectSet, CheckedType, FunctionId, ModuleId, SymbolId, TypedModule,
};

macro_rules! arena_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(usize);

        impl ArenaId for $name {
            fn from_index(index: usize) -> Self {
                Self(index)
            }

            fn index(self) -> usize {
                self.0
            }
        }
    };
}

trait ArenaId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

arena_id!(ExpressionId);
arena_id!(PatternId);
arena_id!(BlockId);
arena_id!(ItemId);
arena_id!(LoweredFunctionId);
arena_id!(InitializerId);

/// Deterministic, append-only storage whose handles cannot be mixed with
/// handles from another lowered-node family.
#[derive(Debug, Clone)]
struct Arena<T, I> {
    values: Vec<T>,
    id: PhantomData<fn() -> I>,
}

impl<T, I> Default for Arena<T, I> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            id: PhantomData,
        }
    }
}

impl<T, I: ArenaId> Arena<T, I> {
    fn push(&mut self, value: T) -> I {
        let id = I::from_index(self.values.len());
        self.values.push(value);
        id
    }

    fn contains(&self, id: I) -> bool {
        id.index() < self.values.len()
    }

    fn iter(&self) -> impl Iterator<Item = (I, &T)> {
        self.values
            .iter()
            .enumerate()
            .map(|(index, value)| (I::from_index(index), value))
    }
}

/// Source identity retained on every lowered node for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    pub syntax: SyntaxId,
    pub span: Span,
}

impl Origin {
    fn compiler() -> Self {
        Self {
            syntax: SyntaxId::COMPILER,
            span: Span::Compiler,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredExpression {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub effects: CheckedEffectSet,
    pub coercion: Option<CheckedCoercion>,
    pub moved_symbols: Vec<SymbolId>,
    pub kind: LoweredExpressionKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredExpressionKind {
    Block(BlockId),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredPattern {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub kind: LoweredPatternKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredPatternKind {
    Wildcard,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBlock {
    pub origin: Origin,
    pub items: Vec<ItemId>,
    pub result: Option<ExpressionId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredItem {
    pub origin: Origin,
    pub kind: LoweredItemKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredItemKind {
    Expression(ExpressionId),
    Pattern(PatternId),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredFunction {
    pub origin: Origin,
    pub semantic_id: FunctionId,
    pub body: Option<BlockId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredInitializer {
    pub origin: Origin,
    pub module: ModuleId,
    pub body: BlockId,
}

/// The complete owned Stage 2 representation. Arena order is insertion order,
/// which lowering defines to be deterministic program/source order.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredProgram {
    expressions: Arena<LoweredExpression, ExpressionId>,
    patterns: Arena<LoweredPattern, PatternId>,
    blocks: Arena<LoweredBlock, BlockId>,
    items: Arena<LoweredItem, ItemId>,
    functions: Arena<LoweredFunction, LoweredFunctionId>,
    initializers: Arena<LoweredInitializer, InitializerId>,
}

impl LoweredProgram {
    fn validate(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, expression) in self.expressions.iter() {
            match expression.kind {
                LoweredExpressionKind::Block(id) if !self.blocks.contains(id) => diagnostics.push(
                    invalid_reference(&expression.origin, "expression", "block", id.index()),
                ),
                LoweredExpressionKind::Block(_) => {}
            }
        }
        for (_, block) in self.blocks.iter() {
            for item in &block.items {
                if !self.items.contains(*item) {
                    diagnostics.push(invalid_reference(
                        &block.origin,
                        "block",
                        "item",
                        item.index(),
                    ));
                }
            }
            if let Some(result) = block.result
                && !self.expressions.contains(result)
            {
                diagnostics.push(invalid_reference(
                    &block.origin,
                    "block",
                    "expression",
                    result.index(),
                ));
            }
        }
        for (_, item) in self.items.iter() {
            let (kind, index, valid) = match item.kind {
                LoweredItemKind::Expression(id) => {
                    ("expression", id.index(), self.expressions.contains(id))
                }
                LoweredItemKind::Pattern(id) => ("pattern", id.index(), self.patterns.contains(id)),
            };
            if !valid {
                diagnostics.push(invalid_reference(&item.origin, "item", kind, index));
            }
        }
        for (_, function) in self.functions.iter() {
            if let Some(body) = function.body
                && !self.blocks.contains(body)
            {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "block",
                    body.index(),
                ));
            }
        }
        for (_, initializer) in self.initializers.iter() {
            if !self.blocks.contains(initializer.body) {
                diagnostics.push(invalid_reference(
                    &initializer.origin,
                    "initializer",
                    "block",
                    initializer.body.index(),
                ));
            }
        }
        diagnostics
    }
}

fn invalid_reference(origin: &Origin, owner: &str, target: &str, index: usize) -> Diagnostic {
    Diagnostic::new(
        origin.span.clone(),
        format!("lowered {owner} has dangling {target} reference {index}"),
    )
}

/// A program accepted by the lowering phase and ready for code generation.
///
/// The fields are intentionally private: callers may pass this value to later
/// compiler phases, but cannot depend on the transitional representation.
#[derive(Debug, Clone)]
pub struct LoweredModule {
    program: LoweredProgram,
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
        let program = LoweredProgram::default();
        let mut diagnostics = validate_checked_module(module);
        diagnostics.extend(program.validate());
        if diagnostics.is_empty() {
            Ok(LoweredModule {
                program,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_program_has_valid_deterministic_arenas() {
        let program = LoweredProgram::default();
        assert!(program.validate().is_empty());
        assert_eq!(program.expressions.iter().count(), 0);
        assert_eq!(program.functions.iter().count(), 0);
    }

    #[test]
    fn arena_ids_are_dense_and_in_insertion_order() {
        let mut patterns = Arena::<LoweredPattern, PatternId>::default();
        let first = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            kind: LoweredPatternKind::Wildcard,
        });
        let second = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I64,
            kind: LoweredPatternKind::Wildcard,
        });
        assert_eq!(first.index(), 0);
        assert_eq!(second.index(), 1);
        assert_eq!(
            patterns
                .iter()
                .map(|(id, _)| id.index())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn validator_rejects_dangling_arena_references() {
        let mut program = LoweredProgram::default();
        program.expressions.push(LoweredExpression {
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Block(BlockId::from_index(4)),
        });
        let diagnostics = program.validate();
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("dangling block reference 4")
        );
    }
}
