//! The run-time representation of effect rows in the dynamic ABI (Stage 5).
//!
//! A row without a variable passes each of its resources by address, in
//! canonical order. A row that names a variable passes one *bundle* instead,
//! so the representation depends only on the row's type:
//!
//! * with fixed resources `r0..rn`, the bundle is an array of the addresses
//!   of `r0..rn` followed by the representation of the variable;
//! * with none, the bundle is the representation of the variable itself.
//!
//! The representation of a variable instantiated at a set `s0..sk` of
//! resources (plus, possibly, a variable of the caller's) is an array of their
//! addresses (then the caller's variable's representation), null when empty,
//! and the caller's own representation when that is all there is. Two function
//! types that are equal after substitution therefore share one ABI, and only
//! concrete function types ever need a reabstraction thunk.
use super::{CodeGenerationResult, Diagnostic, LoweredEmitter, compiler_diagnostic};
use crate::CheckedEffectSet;
use inkwell::{
    AddressSpace,
    values::{BasicMetadataValueEnum, PointerValue},
};

/// How a call's row maps onto a callee row that names a variable.
#[derive(Debug, Clone)]
pub(super) struct RowLayout {
    /// For each fixed resource of the callee's row, its index in the call row.
    pub fixed: Vec<usize>,
    /// Call-row resources the callee's variable stands for, in canonical order.
    pub rest: Vec<usize>,
    /// Whether the callee's variable also stands for a variable of the caller.
    pub nested: bool,
}

/// Matches the callee row's fixed resources against the call row.
pub(super) fn row_layout(
    callee_row: &CheckedEffectSet,
    call_row: &CheckedEffectSet,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<RowLayout> {
    let mut taken = vec![false; call_row.resources.len()];
    let mut fixed = Vec::with_capacity(callee_row.resources.len());
    for resource in &callee_row.resources {
        let position = call_row
            .resources
            .iter()
            .enumerate()
            .position(|(index, candidate)| {
                !taken[index]
                    && candidate.value_type == resource.value_type
                    && candidate.mutable == resource.mutable
            })
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "a call row lacks a fixed resource of its callee")
            })?;
        taken[position] = true;
        fixed.push(position);
    }
    Ok(RowLayout {
        fixed,
        rest: taken
            .iter()
            .enumerate()
            .filter(|(_, taken)| !**taken)
            .map(|(index, _)| index)
            .collect(),
        nested: call_row.variable.is_some(),
    })
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    /// Pushes the resource arguments of a dynamic call. `pointers` follow
    /// `call_row.resources`; `own_bundle` is the representation of the
    /// caller's own variable, when the call row names one.
    pub(super) fn push_dynamic_row_arguments(
        &self,
        span: &staple_syntax::Span,
        callee_row: &CheckedEffectSet,
        call_row: &CheckedEffectSet,
        pointers: Vec<PointerValue<'context>>,
        own_bundle: Option<PointerValue<'context>>,
        arguments: &mut Vec<BasicMetadataValueEnum<'context>>,
    ) -> CodeGenerationResult<()> {
        let layout = row_layout(callee_row, call_row, span)?;
        if callee_row.variable.is_none() {
            if !layout.rest.is_empty() || layout.nested {
                return Err(Diagnostic::new(
                    span.clone(),
                    "a dynamic call row has effects its callee does not take",
                ));
            }
            for position in &layout.fixed {
                arguments.push(pointers[*position].into());
            }
            return Ok(());
        }
        let bundle = self.build_row_bundle(span, &layout, &pointers, own_bundle)?;
        arguments.push(bundle.into());
        Ok(())
    }

    /// The bundle of a callee row that names a variable.
    pub(super) fn build_row_bundle(
        &self,
        span: &staple_syntax::Span,
        layout: &RowLayout,
        pointers: &[PointerValue<'context>],
        own_bundle: Option<PointerValue<'context>>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let null = self
            .backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null();
        let own = if layout.nested {
            Some(own_bundle.ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "a call row naming a variable needs the caller's bundle",
                )
            })?)
        } else {
            None
        };
        let mut variable_entries = layout
            .rest
            .iter()
            .map(|position| pointers[*position])
            .collect::<Vec<_>>();
        let variable = if variable_entries.is_empty() {
            own.unwrap_or(null)
        } else {
            variable_entries.extend(own);
            self.build_pointer_array(&variable_entries)?
        };
        if layout.fixed.is_empty() {
            return Ok(variable);
        }
        let mut entries = layout
            .fixed
            .iter()
            .map(|position| pointers[*position])
            .collect::<Vec<_>>();
        entries.push(variable);
        self.build_pointer_array(&entries)
    }

    /// An array of pointers in the entry block.
    fn build_pointer_array(
        &self,
        pointers: &[PointerValue<'context>],
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let array_type = pointer_type.array_type(pointers.len() as u32);
        let storage = self
            .backend
            .entry_builder()
            .build_alloca(array_type, "effect.bundle")
            .map_err(compiler_diagnostic)?;
        for (index, pointer) in pointers.iter().enumerate() {
            let slot = self.bundle_slot_address(storage, index)?;
            self.backend
                .builder
                .build_store(slot, *pointer)
                .map_err(compiler_diagnostic)?;
        }
        Ok(storage)
    }

    fn bundle_slot_address(
        &self,
        bundle: PointerValue<'context>,
        index: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        self.backend.dynamic_byte_offset(
            bundle,
            self.backend.size_type.const_int(
                index as u64 * self.backend.target_data.get_pointer_byte_size(None) as u64,
                false,
            ),
        )
    }

    /// Loads the pointer in one slot of a bundle.
    pub(super) fn load_bundle_slot(
        &self,
        bundle: PointerValue<'context>,
        index: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let slot = self.bundle_slot_address(bundle, index)?;
        Ok(self
            .backend
            .builder
            .build_load(
                self.backend.context.ptr_type(AddressSpace::default()),
                slot,
                "effect.bundle.entry",
            )
            .map_err(compiler_diagnostic)?
            .into_pointer_value())
    }

    /// The representation of a row's variable, given the row's whole bundle
    /// and the number of its fixed resources.
    pub(super) fn row_variable_bundle(
        &self,
        bundle: PointerValue<'context>,
        fixed: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if fixed == 0 {
            Ok(bundle)
        } else {
            self.load_bundle_slot(bundle, fixed)
        }
    }

    /// The address of the `index`-th resource a callee row's variable stands
    /// for, given the row's whole bundle and the number of its fixed resources.
    pub(super) fn row_rest_resource(
        &self,
        bundle: PointerValue<'context>,
        fixed: usize,
        index: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let variable = self.row_variable_bundle(bundle, fixed)?;
        self.load_bundle_slot(variable, index)
    }
}
