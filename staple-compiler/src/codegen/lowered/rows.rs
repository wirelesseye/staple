//! The run-time representation of effect rows in the dynamic ABI (Stage 5).
//!
//! A row without a variable passes each of its resources by address, in
//! canonical order. A row that names a variable passes one *bundle* instead:
//! a chain of nodes, each holding `(key, address)` entries, where the key is a
//! per-resource-type global. Code finds a resource by its key, walking the
//! chain, so a bundle stands for a *set* of resources and its layout says
//! nothing about where a resource sorts. Two consequences:
//!
//! * a bundle that holds more resources than a row needs still serves that
//!   row, so a caller passes its own bundle on, adding one node in front for
//!   the fixed resources of the call's row;
//! * function types that are equal after substitution share one ABI, however
//!   their rows were written: `() ->{F, Label} ()` at `F := {E, IO}` and
//!   `() ->{E, IO, Label} ()` both receive a bundle holding `IO`, `Label`, and
//!   `E`'s resources. Only concrete function types need a reabstraction thunk.
//!
//! The empty row's bundle is null. A lookup that misses traps; the checker
//! guarantees it never happens.
use super::{CodeGenerationResult, Diagnostic, LoweredEmitter, compiler_diagnostic};
use crate::{CheckedEffectSet, CheckedType};
use inkwell::{
    AddressSpace,
    values::{BasicMetadataValueEnum, FunctionValue, PointerValue},
};

const LOOKUP: &str = "__staple_effect_row_lookup";

/// For each fixed resource of a callee row without a variable, its index in
/// the call row.
pub(super) fn row_positions(
    callee_row: &CheckedEffectSet,
    call_row: &CheckedEffectSet,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<Vec<usize>> {
    let mut taken = vec![false; call_row.resources.len()];
    let mut positions = Vec::with_capacity(callee_row.resources.len());
    for resource in &callee_row.resources {
        let position = call_row
            .resources
            .iter()
            .enumerate()
            .position(|(index, candidate)| {
                !taken[index]
                    && crate::types_equivalent(&candidate.value_type, &resource.value_type)
            })
            .ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "a call row lacks a fixed resource of its callee",
                )
            })?;
        taken[position] = true;
        positions.push(position);
    }
    Ok(positions)
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    /// Pushes the resource arguments of a dynamic call. `pointers` follow
    /// `call_row.resources`; `own_bundle` is the caller's bundle, when the
    /// call row names the caller's variable.
    pub(super) fn push_dynamic_row_arguments(
        &self,
        span: &staple_syntax::Span,
        callee_row: &CheckedEffectSet,
        call_row: &CheckedEffectSet,
        pointers: Vec<PointerValue<'context>>,
        own_bundle: Option<PointerValue<'context>>,
        arguments: &mut Vec<BasicMetadataValueEnum<'context>>,
    ) -> CodeGenerationResult<()> {
        if callee_row.variable.is_none() {
            if call_row.variable.is_some() || call_row.resources.len() != callee_row.resources.len()
            {
                return Err(Diagnostic::new(
                    span.clone(),
                    "a dynamic call row has effects its callee does not take",
                ));
            }
            for position in row_positions(callee_row, call_row, span)? {
                arguments.push(pointers[position].into());
            }
            return Ok(());
        }
        let bundle = self.build_row_bundle(span, call_row, &pointers, own_bundle)?;
        arguments.push(bundle.into());
        Ok(())
    }

    /// The bundle for `call_row`, whose resources are at `pointers`. A row
    /// that names the caller's variable extends `own_bundle`.
    pub(super) fn build_row_bundle(
        &self,
        span: &staple_syntax::Span,
        call_row: &CheckedEffectSet,
        pointers: &[PointerValue<'context>],
        own_bundle: Option<PointerValue<'context>>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let null = self
            .backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null();
        let next = if call_row.variable.is_some() {
            own_bundle.ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "a call row naming a variable needs the caller's bundle",
                )
            })?
        } else {
            null
        };
        if call_row.resources.is_empty() {
            return Ok(next);
        }
        let entries = call_row
            .resources
            .iter()
            .zip(pointers)
            .map(|(resource, pointer)| {
                Ok((self.resource_key(&resource.value_type, span)?, *pointer))
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        self.build_row_node(&entries, next)
    }

    /// One bundle node: `{ next, count, [count x { key, address }] }`.
    fn build_row_node(
        &self,
        entries: &[(PointerValue<'context>, PointerValue<'context>)],
        next: PointerValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let entry_type = self
            .backend
            .context
            .struct_type(&[pointer.into(), pointer.into()], false);
        let node_type = self.backend.context.struct_type(
            &[
                pointer.into(),
                self.backend.size_type.into(),
                entry_type.array_type(entries.len() as u32).into(),
            ],
            false,
        );
        let node = self
            .backend
            .entry_builder()
            .build_alloca(node_type, "effect.bundle")
            .map_err(compiler_diagnostic)?;
        let field = |index: u32| {
            self.backend
                .builder
                .build_struct_gep(node_type, node, index, "effect.bundle.field")
                .map_err(compiler_diagnostic)
        };
        self.backend
            .builder
            .build_store(field(0)?, next)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(
                field(1)?,
                self.backend
                    .size_type
                    .const_int(entries.len() as u64, false),
            )
            .map_err(compiler_diagnostic)?;
        let array = field(2)?;
        for (index, (key, address)) in entries.iter().enumerate() {
            for (part, value) in [(0u64, *key), (1, *address)] {
                let slot = unsafe {
                    self.backend.builder.build_gep(
                        entry_type.array_type(entries.len() as u32),
                        array,
                        &[
                            self.backend.context.i32_type().const_zero(),
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            self.backend.context.i32_type().const_int(part, false),
                        ],
                        "effect.bundle.entry",
                    )
                }
                .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(slot, value)
                    .map_err(compiler_diagnostic)?;
            }
        }
        Ok(node)
    }

    /// The address of the resource of type `value_type` in `bundle`.
    pub(super) fn lookup_row_resource(
        &self,
        bundle: PointerValue<'context>,
        value_type: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let key = self.resource_key(value_type, span)?;
        let lookup = self.row_lookup_function();
        self.backend
            .builder
            .build_direct_call(lookup, &[bundle.into(), key.into()], "effect.resource")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .map(|value| value.into_pointer_value())
            .ok_or_else(|| Diagnostic::new(span.clone(), "effect row lookup has no result"))
    }

    /// The key global of one resource type. Its address is the key.
    fn resource_key(
        &self,
        value_type: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let origin = crate::Origin {
            span: span.clone(),
            syntax: staple_syntax::SyntaxId::COMPILER,
        };
        let canonical = crate::specialization::CanonicalType::concrete(value_type, &origin)?;
        let name = format!("__staple_effect_key.{canonical:?}");
        let module = &self.backend.llvm_module;
        let global = module.get_global(&name).unwrap_or_else(|| {
            let byte = self.backend.context.i8_type();
            let global = module.add_global(byte, None, &name);
            global.set_linkage(inkwell::module::Linkage::Internal);
            global.set_constant(true);
            global.set_initializer(&byte.const_zero());
            global
        });
        Ok(global.as_pointer_value())
    }

    /// `ptr __staple_effect_row_lookup(ptr bundle, ptr key)`, built on first use.
    fn row_lookup_function(&self) -> FunctionValue<'context> {
        let module = &self.backend.llvm_module;
        if let Some(function) = module.get_function(LOOKUP) {
            return function;
        }
        let context = self.backend.context;
        let pointer = context.ptr_type(AddressSpace::default());
        let size = self.backend.size_type;
        let function = module.add_function(
            LOOKUP,
            pointer.fn_type(&[pointer.into(), pointer.into()], false),
            Some(inkwell::module::Linkage::Internal),
        );
        let builder = context.create_builder();
        let entry = context.append_basic_block(function, "entry");
        let node_check = context.append_basic_block(function, "node");
        let node_body = context.append_basic_block(function, "node.body");
        let entry_check = context.append_basic_block(function, "entry.check");
        let entry_body = context.append_basic_block(function, "entry.body");
        let advance = context.append_basic_block(function, "next");
        let found = context.append_basic_block(function, "found");
        let missing = context.append_basic_block(function, "missing");
        let bundle = function.get_nth_param(0).unwrap().into_pointer_value();
        let key = function.get_nth_param(1).unwrap().into_pointer_value();
        let word = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let offset = |base, bytes| unsafe {
            builder
                .build_gep(context.i8_type(), base, &[bytes], "offset")
                .unwrap()
        };
        builder.position_at_end(entry);
        builder.build_unconditional_branch(node_check).unwrap();

        builder.position_at_end(node_check);
        let node = builder.build_phi(pointer, "node").unwrap();
        let node_value = node.as_basic_value().into_pointer_value();
        let is_null = builder.build_is_null(node_value, "end").unwrap();
        builder
            .build_conditional_branch(is_null, missing, node_body)
            .unwrap();

        builder.position_at_end(node_body);
        let count = builder
            .build_load(
                size,
                offset(node_value, size.const_int(word, false)),
                "count",
            )
            .unwrap()
            .into_int_value();
        builder.build_unconditional_branch(entry_check).unwrap();

        builder.position_at_end(entry_check);
        let index = builder.build_phi(size, "index").unwrap();
        let index_value = index.as_basic_value().into_int_value();
        let done = builder
            .build_int_compare(inkwell::IntPredicate::EQ, index_value, count, "done")
            .unwrap();
        builder
            .build_conditional_branch(done, advance, entry_body)
            .unwrap();

        builder.position_at_end(entry_body);
        let scaled = builder
            .build_int_mul(index_value, size.const_int(2 * word, false), "scaled")
            .unwrap();
        let position = builder
            .build_int_add(scaled, size.const_int(2 * word, false), "position")
            .unwrap();
        let slot = offset(node_value, position);
        let candidate = builder
            .build_load(pointer, slot, "key")
            .unwrap()
            .into_pointer_value();
        let matches = builder
            .build_int_compare(inkwell::IntPredicate::EQ, candidate, key, "match")
            .unwrap();
        let following = builder
            .build_int_add(index_value, size.const_int(1, false), "following")
            .unwrap();
        builder
            .build_conditional_branch(matches, found, entry_check)
            .unwrap();
        index.add_incoming(&[(&size.const_zero(), node_body), (&following, entry_body)]);

        builder.position_at_end(found);
        let address = builder
            .build_load(
                pointer,
                offset(slot, size.const_int(word, false)),
                "address",
            )
            .unwrap();
        builder.build_return(Some(&address)).unwrap();

        builder.position_at_end(advance);
        let next = builder
            .build_load(pointer, node_value, "next")
            .unwrap()
            .into_pointer_value();
        builder.build_unconditional_branch(node_check).unwrap();
        node.add_incoming(&[(&bundle, entry), (&next, advance)]);

        builder.position_at_end(missing);
        let trap = module.get_function("llvm.trap").unwrap_or_else(|| {
            module.add_function("llvm.trap", context.void_type().fn_type(&[], false), None)
        });
        builder.build_direct_call(trap, &[], "").unwrap();
        builder.build_unreachable().unwrap();
        function
    }
}
