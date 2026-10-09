//! Emission of lowering-owned recursive value conversions.
use super::{CodeGenerationResult, Diagnostic, LoweredEmitter, compiler_diagnostic};
use crate::specialization::{ArtifactOrdinal, ArtifactRequestKey, CanonicalType};
use crate::{CheckedType, PlannedArtifact, ValueConversionPlan};
use inkwell::{
    AddressSpace,
    values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, PointerValue},
};

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    fn planned_conversion_child(
        &self,
        plan: &ValueConversionPlan,
        concrete: &CheckedType,
        dynamic: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Option<FunctionValue<'context>>> {
        let origin = crate::Origin {
            span: span.clone(),
            syntax: staple_syntax::SyntaxId::COMPILER,
        };
        let concrete = crate::lower::conversion_concrete_key(concrete, &origin)?;
        let dynamic = CanonicalType::dynamic(dynamic, &origin)?;
        let child = plan.children.iter().find(|child| matches!(&child.key, ArtifactRequestKey::ValueConversion(key) if key.concrete == concrete && key.dynamic == dynamic));
        child
            .map(|child| self.planned_conversion_function(child, span))
            .transpose()
    }

    pub(super) fn reabstraction_writeback_code(
        &self,
        owner: crate::lower::EmissionOwner,
        call: crate::LoweredCallId,
        argument: usize,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let artifact = self
            .view
            .artifact_uses(owner)
            .and_then(|uses| {
                uses.iter().find(|use_| {
                    use_.site == crate::ArtifactUseSite::ReabstractionWriteback { call, argument }
                })
            })
            .and_then(|use_| self.artifacts.get(&use_.artifact))
            .and_then(|functions| functions.first())
            .copied();
        artifact.ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                "missing mutable boundary writeback conversion",
            )
        })
    }

    pub(super) fn planned_conversion_function(
        &self,
        child: &PlannedArtifact,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        child
            .artifact
            .and_then(|ordinal| self.artifacts.get(&ordinal))
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing planned value conversion"))
    }

    pub(super) fn emit_value_conversion(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &ValueConversionPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing value conversion declaration"))?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let destination = function.get_nth_param(0).unwrap().into_pointer_value();
        let source = function.get_nth_param(1).unwrap().into_pointer_value();
        if crate::lower::conversion_takes_descriptor(&plan.concrete) {
            let descriptor = function.get_nth_param(2).unwrap().into_pointer_value();
            return self.emit_descriptor_value_conversion(
                function,
                plan,
                destination,
                source,
                descriptor,
                span,
            );
        }
        let layout = self.backend.descriptor_layout(&plan.concrete)?;
        self.backend
            .builder
            .build_memcpy(
                destination,
                layout.align,
                source,
                layout.align,
                self.backend.size_type.const_int(layout.store_size, false),
            )
            .map_err(compiler_diagnostic)?;
        match (&plan.concrete, &plan.dynamic) {
            (CheckedType::Function(_), CheckedType::Function(_)) if !plan.children.is_empty() => {
                let thunk = self.planned_conversion_function(&plan.children[0], span)?;
                let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
                let environment = self.backend.build_gc_allocation(
                    self.backend.size_type.const_int(2 * pointer_bytes, false),
                    "conversion.environment",
                    span.clone(),
                )?;
                self.backend
                    .builder
                    .build_memcpy(
                        environment,
                        layout.align,
                        source,
                        layout.align,
                        self.backend.size_type.const_int(2 * pointer_bytes, false),
                    )
                    .map_err(compiler_diagnostic)?;
                let closure = self.backend.build_closure_value(thunk, environment)?;
                self.backend
                    .builder
                    .build_store(destination, closure)
                    .map_err(compiler_diagnostic)?
                    .set_alignment(layout.align)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
            }
            (CheckedType::Product(left), CheckedType::Product(right)) => {
                let mut offset = 0u64;
                for (left, right) in left.elements.iter().zip(&right.elements) {
                    let layout = self.backend.descriptor_layout(&left.value_type)?;
                    offset = offset.div_ceil(layout.align as u64) * layout.align as u64;
                    if let Some(child) = self.planned_conversion_child(
                        plan,
                        &left.value_type,
                        &right.value_type,
                        span,
                    )? {
                        let offset = self.backend.size_type.const_int(offset, false);
                        let destination = self.backend.dynamic_byte_offset(destination, offset)?;
                        let source = self.backend.dynamic_byte_offset(source, offset)?;
                        self.backend
                            .builder
                            .build_direct_call(child, &[destination.into(), source.into()], "")
                            .map_err(compiler_diagnostic)?;
                    }
                    offset += layout.store_size;
                }
            }
            (
                CheckedType::Wrapper {
                    representation: left,
                    ..
                },
                CheckedType::Wrapper {
                    representation: right,
                    ..
                },
            ) => {
                if let Some(child) = self.planned_conversion_child(plan, left, right, span)? {
                    self.backend
                        .builder
                        .build_direct_call(child, &[destination.into(), source.into()], "")
                        .map_err(compiler_diagnostic)?;
                }
            }
            (CheckedType::Sum(left), CheckedType::Sum(right)) => {
                let sum_type = self
                    .backend
                    .compile_type(&plan.concrete)?
                    .into_struct_type();
                let offset = self
                    .backend
                    .target_data
                    .offset_of_element(&sum_type, 1)
                    .unwrap();
                let payload_destination = self.backend.dynamic_byte_offset(
                    destination,
                    self.backend.size_type.const_int(offset, false),
                )?;
                let payload_source = self
                    .backend
                    .dynamic_byte_offset(source, self.backend.size_type.const_int(offset, false))?;
                let tag = self
                    .backend
                    .builder
                    .build_load(self.backend.context.i32_type(), source, "conversion.tag")
                    .map_err(compiler_diagnostic)?;
                tag.as_instruction_value()
                    .unwrap()
                    .set_alignment(self.backend.target_data.get_abi_alignment(&tag.get_type()))
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let merge = self
                    .backend
                    .context
                    .append_basic_block(function, "conversion.done");
                let mut cases = Vec::new();
                for (index, (left, right)) in left
                    .alternatives
                    .iter()
                    .zip(&right.alternatives)
                    .enumerate()
                {
                    if let Some(child) = self.planned_conversion_child(plan, left, right, span)? {
                        let block = self
                            .backend
                            .context
                            .append_basic_block(function, "conversion.alternative");
                        cases.push((
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            block,
                            child,
                        ));
                    }
                }
                self.backend
                    .builder
                    .build_switch(
                        tag.into_int_value(),
                        merge,
                        &cases
                            .iter()
                            .map(|(tag, block, _)| (*tag, *block))
                            .collect::<Vec<_>>(),
                    )
                    .map_err(compiler_diagnostic)?;
                for (_, block, child) in cases {
                    self.backend.builder.position_at_end(block);
                    self.backend
                        .builder
                        .build_direct_call(
                            child,
                            &[payload_destination.into(), payload_source.into()],
                            "",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
            }
            // Fixed repeated types normalize to products during materialization;
            // symbolic array counts are the runtime-Natural restriction.
            _ => {}
        }
        self.backend
            .builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Calls one planned conversion, passing `descriptor` when the
    /// conversion's layout depends on run-time parameters.
    pub(super) fn call_value_conversion(
        &self,
        conversion: FunctionValue<'context>,
        concrete: &CheckedType,
        destination: PointerValue<'context>,
        source: PointerValue<'context>,
        descriptor: Option<PointerValue<'context>>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let mut arguments: Vec<BasicMetadataValueEnum<'context>> =
            vec![destination.into(), source.into()];
        if crate::lower::conversion_takes_descriptor(concrete) {
            let descriptor = descriptor.ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing value conversion descriptor")
            })?;
            arguments.push(descriptor.into());
        }
        self.backend
            .builder
            .build_direct_call(conversion, &arguments, "")
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// A conversion whose concrete side mixes run-time parameters with the
    /// concrete functions it converts. Sizes and offsets come from the value's
    /// descriptor, whose components follow the type's fields or alternatives,
    /// exactly as dynamic code reads products and sums. Erased parts are
    /// copied unchanged; only function fields are rewrapped.
    fn emit_descriptor_value_conversion(
        &mut self,
        function: FunctionValue<'context>,
        plan: &ValueConversionPlan,
        destination: PointerValue<'context>,
        source: PointerValue<'context>,
        descriptor: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        self.backend
            .copy_dynamic_value(descriptor, source, destination)?;
        match (&plan.concrete, &plan.dynamic) {
            (CheckedType::Product(left), CheckedType::Product(right)) => {
                for (index, (left, right)) in left.elements.iter().zip(&right.elements).enumerate()
                {
                    let Some(child) = self.planned_conversion_child(
                        plan,
                        &left.value_type,
                        &right.value_type,
                        span,
                    )?
                    else {
                        continue;
                    };
                    let (field_destination, component) =
                        self.dynamic_product_field(descriptor, destination, index as u32)?;
                    let (field_source, _) =
                        self.dynamic_product_field(descriptor, source, index as u32)?;
                    self.call_value_conversion(
                        child,
                        &left.value_type,
                        field_destination,
                        field_source,
                        Some(component),
                        span,
                    )?;
                }
            }
            (
                CheckedType::Wrapper {
                    representation: left,
                    ..
                },
                CheckedType::Wrapper {
                    representation: right,
                    ..
                },
            ) => {
                // A wrapper's descriptor is its representation's.
                if let Some(child) = self.planned_conversion_child(plan, left, right, span)? {
                    self.call_value_conversion(
                        child,
                        left,
                        destination,
                        source,
                        Some(descriptor),
                        span,
                    )?;
                }
            }
            (CheckedType::Sum(left), CheckedType::Sum(right)) => {
                let count = left.alternatives.len();
                let payload_destination =
                    self.dynamic_sum_payload(descriptor, destination, count)?;
                let payload_source = self.dynamic_sum_payload(descriptor, source, count)?;
                let tag = self.dynamic_sum_tag(source, span)?;
                let merge = self
                    .backend
                    .context
                    .append_basic_block(function, "conversion.done");
                let mut cases = Vec::new();
                for (index, (left, right)) in left
                    .alternatives
                    .iter()
                    .zip(&right.alternatives)
                    .enumerate()
                {
                    if let Some(child) = self.planned_conversion_child(plan, left, right, span)? {
                        let block = self
                            .backend
                            .context
                            .append_basic_block(function, "conversion.alternative");
                        cases.push((index, block, child, left));
                    }
                }
                self.backend
                    .builder
                    .build_switch(
                        tag,
                        merge,
                        &cases
                            .iter()
                            .map(|(index, block, _, _)| {
                                (
                                    self.backend
                                        .context
                                        .i32_type()
                                        .const_int(*index as u64, false),
                                    *block,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                    .map_err(compiler_diagnostic)?;
                for (index, block, child, alternative) in cases {
                    self.backend.builder.position_at_end(block);
                    let component = self
                        .backend
                        .load_descriptor_component(descriptor, index as u32)?;
                    self.call_value_conversion(
                        child,
                        alternative,
                        payload_destination,
                        payload_source,
                        Some(component),
                        span,
                    )?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
            }
            // A function that mentions run-time parameters already has the
            // dynamic-shape ABI, and symbolic array counts are the
            // runtime-Natural restriction.
            _ => {}
        }
        self.backend
            .builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(super) fn convert_boundary_value(
        &self,
        value: inkwell::values::BasicValueEnum<'context>,
        owner: crate::lower::EmissionOwner,
        call: crate::LoweredCallId,
        argument: Option<usize>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        let conversion = self.reabstraction_thunk_code(owner, call, argument, span)?;
        let source = self
            .backend
            .entry_builder()
            .build_alloca(value.get_type(), "conversion.source")
            .map_err(compiler_diagnostic)?;
        let destination = self
            .backend
            .entry_builder()
            .build_alloca(value.get_type(), "conversion.destination")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(source, value)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_direct_call(conversion, &[destination.into(), source.into()], "")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_load(value.get_type(), destination, "conversion.value")
            .map_err(compiler_diagnostic)
    }
}

/// Where a resource of a reabstracted row is found on the dynamic side.
#[derive(Debug, Clone, Copy)]
enum RowSource {
    Fixed(usize),
    Bundle(usize),
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    fn conversion_field_pointer(
        &self,
        storage: PointerValue<'context>,
        parameter: &CheckedType,
        index: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let offset = if let CheckedType::Product(product) = parameter {
            let llvm = self.backend.compile_product_type(product)?;
            self.backend
                .target_data
                .offset_of_element(&llvm, index as u32)
                .expect("conversion product field exists")
        } else {
            0
        };
        self.backend
            .dynamic_byte_offset(storage, self.backend.size_type.const_int(offset, false))
    }

    pub(super) fn conversion_copy(
        &self,
        ty: &CheckedType,
        source: PointerValue<'context>,
        destination: PointerValue<'context>,
    ) -> CodeGenerationResult<()> {
        let layout = self.backend.descriptor_layout(ty)?;
        let size = layout.store_size;
        self.backend
            .builder
            .build_memcpy(
                destination,
                layout.align,
                source,
                layout.align,
                self.backend.size_type.const_int(size, false),
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    fn conversion_load(
        &self,
        ty: &CheckedType,
        source: PointerValue<'context>,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let value = self
            .backend
            .builder
            .build_load(
                self.backend.compile_type(ty)?,
                source,
                "conversion.argument",
            )
            .map_err(compiler_diagnostic)?;
        value
            .as_instruction_value()
            .unwrap()
            .set_alignment(
                self.backend
                    .target_data
                    .get_abi_alignment(&value.get_type()),
            )
            .map_err(|error| Diagnostic::new(staple_syntax::Span::Compiler, error.to_string()))?;
        Ok(value)
    }

    /// Where each of the concrete row's resources comes from on the dynamic
    /// side: a fixed resource of the dynamic row, or the next slot of the
    /// bundle that stands for its variable.
    fn row_sources(
        &self,
        concrete: &crate::CheckedEffectSet,
        dynamic: &crate::CheckedEffectSet,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Vec<RowSource>> {
        let mut used = vec![false; dynamic.resources.len()];
        let mut bundled = 0usize;
        let mut sources = Vec::with_capacity(concrete.resources.len());
        for resource in &concrete.resources {
            let fixed = dynamic.resources.iter().enumerate().position(|(index, fixed)| {
                !used[index]
                    && fixed.value_type == resource.value_type
                    && fixed.mutable == resource.mutable
            });
            match fixed {
                Some(index) => {
                    used[index] = true;
                    sources.push(RowSource::Fixed(index));
                }
                None if dynamic.variable.is_some() => {
                    sources.push(RowSource::Bundle(bundled));
                    bundled += 1;
                }
                None => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "a reabstracted row has a resource its dynamic side lacks",
                    ));
                }
            }
        }
        if used.iter().any(|used| !*used) {
            return Err(Diagnostic::new(
                span.clone(),
                "a reabstracted row lacks a resource of its dynamic side",
            ));
        }
        Ok(sources)
    }

    pub(super) fn emit_reabstraction_thunk(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &crate::ReabstractionThunkPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        use crate::specialization::ReabstractionDirection as Direction;
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reabstraction declaration"))?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let parameters = function.get_params();
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let environment = parameters[0].into_pointer_value();
        let code = self
            .backend
            .builder
            .build_load(pointer, environment, "reabstraction.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let slot = self.backend.dynamic_byte_offset(
            environment,
            self.backend.size_type.const_int(
                self.backend.target_data.get_pointer_byte_size(None) as u64,
                false,
            ),
        )?;
        let closure_environment = self
            .backend
            .builder
            .build_load(pointer, slot, "reabstraction.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let parameter_type = self.backend.compile_type(&plan.concrete.parameter)?;
        let source = self
            .backend
            .entry_builder()
            .build_alloca(parameter_type, "reabstraction.source")
            .map_err(compiler_diagnostic)?;
        let target = self
            .backend
            .entry_builder()
            .build_alloca(parameter_type, "reabstraction.target")
            .map_err(compiler_diagnostic)?;
        let result_type = self.backend.compile_type(&plan.concrete.result)?;
        let result = self
            .backend
            .entry_builder()
            .build_alloca(result_type, "reabstraction.result")
            .map_err(compiler_diagnostic)?;
        let concrete_types = super::flattened_parameter_types(&plan.concrete.parameter);
        let concrete_indirect = self.backend.indirect_parameter_mask(&plan.concrete);
        let concrete_whole = plan
            .concrete
            .mutations
            .contains(&crate::CheckedMutation::Whole);
        let dynamic_whole = plan
            .dynamic
            .mutations
            .contains(&crate::CheckedMutation::Whole);
        let dynamic_product =
            !dynamic_whole && matches!(plan.dynamic.parameter.as_ref(), CheckedType::Product(_));
        let concrete_caller = plan.direction == Direction::DynamicToConcrete;
        let resource_count = plan.concrete.effects.resources.len();
        // The dynamic side passes its row's fixed resources by pointer and, for
        // a row with a variable, one bundle pointer holding the rest.
        let dynamic_fixed = plan.dynamic.effects.resources.len();
        let bundle_slots = usize::from(plan.dynamic.effects.variable.is_some());
        let row_sources =
            self.row_sources(&plan.concrete.effects, &plan.dynamic.effects, span)?;
        let value_start = if concrete_caller {
            1 + resource_count
        } else {
            2 + dynamic_fixed + bundle_slots
        };
        let mut caller_slots = Vec::new();
        if (!concrete_caller && !dynamic_product) || (concrete_caller && concrete_whole) {
            let incoming = parameters[value_start].into_pointer_value();
            self.conversion_copy(&plan.concrete.parameter, incoming, source)?;
            for index in 0..concrete_types.len() {
                caller_slots.push(self.conversion_field_pointer(
                    incoming,
                    &plan.concrete.parameter,
                    index,
                )?);
            }
        } else {
            for (index, ty) in concrete_types.iter().enumerate() {
                let field =
                    self.conversion_field_pointer(source, &plan.concrete.parameter, index)?;
                let parameter = parameters[value_start + index];
                if !concrete_caller || concrete_indirect[index] {
                    let incoming = parameter.into_pointer_value();
                    self.conversion_copy(ty, incoming, field)?;
                    caller_slots.push(incoming);
                } else {
                    self.backend
                        .builder
                        .build_store(field, parameter)
                        .map_err(compiler_diagnostic)?
                        .set_alignment(
                            self.backend
                                .target_data
                                .get_abi_alignment(&parameter.get_type()),
                        )
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    caller_slots.push(field);
                }
            }
        }
        let argument_conversion = self.planned_conversion_function(&plan.children[0], span)?;
        self.backend
            .builder
            .build_direct_call(argument_conversion, &[target.into(), source.into()], "")
            .map_err(compiler_diagnostic)?;
        let mut arguments: Vec<BasicMetadataValueEnum<'context>> = vec![closure_environment.into()];
        if concrete_caller {
            arguments.push(result.into());
        }
        let mut resource_pointers = Vec::new();
        for (index, resource) in plan.concrete.effects.resources.iter().enumerate() {
            let indirect = resource.mutable || !self.backend.layout.is_copy(&resource.value_type);
            if concrete_caller {
                let incoming = parameters[1 + index];
                if indirect {
                    resource_pointers.push(incoming.into_pointer_value());
                } else {
                    let slot = self
                        .backend
                        .entry_builder()
                        .build_alloca(incoming.get_type(), "reabstraction.resource")
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(slot, incoming)
                        .map_err(compiler_diagnostic)?;
                    resource_pointers.push(slot);
                }
                continue;
            }
            let incoming = match row_sources[index] {
                RowSource::Fixed(position) => parameters[2 + position].into_pointer_value(),
                RowSource::Bundle(position) => {
                    let bundle = parameters[2 + dynamic_fixed].into_pointer_value();
                    let slot = self.backend.dynamic_byte_offset(
                        bundle,
                        self.backend.size_type.const_int(
                            position as u64
                                * self.backend.target_data.get_pointer_byte_size(None) as u64,
                            false,
                        ),
                    )?;
                    self.backend
                        .builder
                        .build_load(pointer, slot, "reabstraction.bundle.resource")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value()
                }
            };
            if indirect {
                arguments.push(incoming.into());
            } else {
                arguments.push(
                    self.conversion_load(&resource.value_type, incoming)?
                        .into(),
                );
            }
        }
        if concrete_caller {
            self.push_dynamic_row_arguments(
                span,
                &plan.dynamic.effects,
                &plan.concrete.effects,
                resource_pointers,
                None,
                false,
                &mut arguments,
            )?;
        }
        let mut callee_mutations = Vec::new();
        if concrete_caller {
            if dynamic_product {
                for index in 0..concrete_types.len() {
                    arguments.push(
                        self.conversion_field_pointer(target, &plan.concrete.parameter, index)?
                            .into(),
                    );
                }
            } else {
                arguments.push(target.into());
            }
            let signature = self
                .backend
                .compile_dynamic_function_type(&plan.dynamic, 0)?;
            self.backend
                .builder
                .build_indirect_call(signature, code, &arguments, "")
                .map_err(compiler_diagnostic)?;
        } else {
            if concrete_whole {
                arguments.push(target.into());
            } else {
                for (index, ty) in concrete_types.iter().enumerate() {
                    let field =
                        self.conversion_field_pointer(target, &plan.concrete.parameter, index)?;
                    if concrete_indirect[index] {
                        let slot = self
                            .backend
                            .entry_builder()
                            .build_alloca(self.backend.compile_type(ty)?, "reabstraction.borrow")
                            .map_err(compiler_diagnostic)?;
                        self.conversion_copy(ty, field, slot)?;
                        arguments.push(slot.into());
                        if plan
                            .concrete
                            .mutations
                            .contains(&crate::CheckedMutation::Element(index))
                        {
                            callee_mutations.push((index, slot));
                        }
                    } else {
                        arguments.push(self.conversion_load(ty, field)?.into());
                    }
                }
            }
            let signature = self.backend.compile_closure_function_type(&plan.concrete)?;
            let value = self
                .backend
                .builder
                .build_indirect_call(signature, code, &arguments, "reabstraction.call")
                .map_err(compiler_diagnostic)?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| {
                    Diagnostic::new(span.clone(), "reabstraction callee returned no value")
                })?;
            self.backend
                .builder
                .build_store(result, value)
                .map_err(compiler_diagnostic)?;
        }
        for (index, slot) in callee_mutations {
            let field = self.conversion_field_pointer(target, &plan.concrete.parameter, index)?;
            self.conversion_copy(concrete_types[index], slot, field)?;
        }
        if !plan.concrete.mutations.is_empty() {
            let writeback = self.planned_conversion_function(&plan.children[2], span)?;
            self.backend
                .builder
                .build_direct_call(writeback, &[source.into(), target.into()], "")
                .map_err(compiler_diagnostic)?;
            for (index, ty) in concrete_types.iter().enumerate() {
                if concrete_whole
                    || plan
                        .concrete
                        .mutations
                        .contains(&crate::CheckedMutation::Element(index))
                {
                    let field =
                        self.conversion_field_pointer(source, &plan.concrete.parameter, index)?;
                    self.conversion_copy(ty, field, caller_slots[index])?;
                }
            }
        }
        let result_conversion = self.planned_conversion_function(&plan.children[1], span)?;
        if concrete_caller {
            let destination = self
                .backend
                .entry_builder()
                .build_alloca(result_type, "reabstraction.return")
                .map_err(compiler_diagnostic)?;
            self.backend
                .builder
                .build_direct_call(result_conversion, &[destination.into(), result.into()], "")
                .map_err(compiler_diagnostic)?;
            let value = self.conversion_load(&plan.concrete.result, destination)?;
            self.backend
                .builder
                .build_return(Some(&value))
                .map_err(compiler_diagnostic)?;
        } else {
            self.backend
                .builder
                .build_direct_call(
                    result_conversion,
                    &[parameters[1].into(), result.into()],
                    "",
                )
                .map_err(compiler_diagnostic)?;
            self.backend
                .builder
                .build_return(None)
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }
}
